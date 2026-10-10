//! ScreenCaptureKit capture + AVAssetWriter (HEVC + AAC) into an .mp4 file.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread};
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeMPEG4, AVMediaTypeAudio,
    AVMediaTypeVideo, AVVideoAverageBitRateKey, AVVideoCodecKey, AVVideoCodecTypeHEVC,
    AVVideoCompressionPropertiesKey, AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey,
    AVVideoMaxKeyFrameIntervalKey, AVVideoWidthKey,
};
use objc2_avf_audio::{AVEncoderBitRateKey, AVFormatIDKey, AVNumberOfChannelsKey, AVSampleRateKey};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_media::CMSampleBuffer;
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSMutableDictionary, NSObject, NSObjectProtocol,
    NSString, NSURL,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};

/// Reads an AVFoundation string constant. Some bindings expose them as plain
/// statics, others as `extern` statics, so the read is always in an `unsafe`
/// block and the lint is silenced for the safe cases.
macro_rules! av_key {
    ($name:path) => {{
        #[allow(unused_unsafe)]
        let value: Option<&'static NSString> = unsafe { $name };
        value.expect("AVFoundation string constant missing")
    }};
}

use crate::audio;
use crate::frame::{Crop, FrameWriter};
use crate::state::{log, AudioSource, Rect, Settings};

/// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ('420v')
const PIXEL_FORMAT_420V: u32 = 0x34323076;
/// Set once we have asked macOS for permission, so we never nag twice in the
/// same session.
static ASKED_PERMISSION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Asks macOS for screen recording access (shows the system prompt if needed).
/// Returns the state right after the call.
pub fn request_permission(force: bool) -> bool {
    if !force && ASKED_PERMISSION.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return unsafe { CGPreflightScreenCaptureAccess() };
    }
    let granted = unsafe { CGRequestScreenCaptureAccess() };
    crate::state::log(&format!(
        "requested screen capture access -> granted={granted}"
    ));
    granted
}

/// How often a movie fragment is flushed to disk (seconds).
const MOVIE_FRAGMENT_SECONDS: u32 = 5;
/// kAudioFormatMPEG4AAC ('aac ')
const AUDIO_FORMAT_AAC: f64 = 0x61616320_u32 as f64;

type Shared = Arc<Mutex<RecorderState>>;
type SettingsDict = NSMutableDictionary<NSString, AnyObject>;

/// Recorder state shared with the ScreenCaptureKit sample handler queue.
static CURRENT: Mutex<Option<Shared>> = Mutex::new(None);

pub struct RecorderState {
    writer: Retained<AVAssetWriter>,
    /// Created once the first video frame tells us its real pixel size.
    video_in: Option<Retained<AVAssetWriterInput>>,
    frame_writer: Option<FrameWriter>,
    audio_in: Option<Retained<AVAssetWriterInput>>,
    settings: Settings,
    crop: Crop,
    /// 待混音的麦克风采样（48 kHz 立体声交错），按到达顺序消费。
    mic_queue: Vec<f32>,
    mic_unsupported_logged: bool,
    /// 已收到麦克风采样；未收到时用于提示用户检查权限与输入设备。
    mic_started_at: Option<std::time::Instant>,
    mic_wait_buffers: u32,
    /// 整段录制中麦克风的最大峰值，停止时写入日志，便于判断输入电平。
    mic_peak: f32,
    /// 混音时麦克风队列不够用（欠载）的次数，停止时写日志。
    mic_underruns: u32,
    /// 实测麦克风速率用：累计收到的帧数与起始时间。
    mic_frames_seen: u32,
    mic_t0: Option<std::time::Instant>,
    mic_rate_logged: bool,
    write_started: bool,
    session_started: bool,
    stopped: bool,
    pub frames: u64,
    error: Option<String>,
}

// SAFETY: the Objective-C objects below are only touched from the main thread
// (start/stop) or from the serial sample handler queue while holding `CURRENT`'s
// mutex. Nothing leaks them across threads unguarded.
unsafe impl Send for RecorderState {}

pub struct Recorder {
    stream: Retained<SCStream>,
    _output: Retained<SampleHandler>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    shared: Shared,
    path: PathBuf,
}

// SAFETY: see `RecorderState`.
unsafe impl Send for Recorder {}

impl Recorder {
    /// `region` is in Cocoa screen coordinates (y from the bottom of the main
    /// display). `None` captures the whole main display.
    pub fn start(region: Option<Rect>, settings: &Settings) -> Result<Self, String> {
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            request_permission(false);
        }
        let content = match fetch_shareable_content() {
            Ok(content) => content,
            Err(first_error) => {
                if unsafe { CGPreflightScreenCaptureAccess() } {
                    return Err(first_error);
                }
                request_permission(false);
                log(&format!("first attempt failed ({first_error}), retried after permission request"));
                fetch_shareable_content()?
            }
        };
        let display = main_display(&content)?;
        let frame = unsafe { display.frame() };
        let display_h = frame.size.height as f64;

        // ScreenCaptureKit's own sub-region capture (`sourceRect`) yields frames
        // VideoToolbox refuses to encode, so we always capture the whole display
        // and crop while copying frames (see `frame` module).
        let scale = settings.scale.max(0.5);
        let mut capture_w = (frame.size.width as f64 * scale).round() as usize;
        let mut capture_h = (frame.size.height as f64 * scale).round() as usize;
        capture_w -= capture_w % 2;
        capture_h -= capture_h % 2;
        capture_w = capture_w.max(2);
        capture_h = capture_h.max(2);

        let crop = match region {
            Some(r) => {
                let x = (r.x * scale).round().max(0.0) as usize;
                let y = ((display_h - r.y - r.h) * scale).round().max(0.0) as usize;
                let w = (r.w * scale).round().max(2.0) as usize;
                let h = (r.h * scale).round().max(2.0) as usize;
                // Keep everything even so NV12 chroma stays aligned.
                let x = (x - x % 2).min(capture_w.saturating_sub(2));
                let y = (y - y % 2).min(capture_h.saturating_sub(2));
                let w = (w - w % 2).clamp(2, capture_w - x);
                let h = (h - h % 2).clamp(2, capture_h - y);
                Crop {
                    x,
                    y,
                    width: w,
                    height: h,
                }
            }
            None => Crop {
                x: 0,
                y: 0,
                width: capture_w,
                height: capture_h,
            },
        };

        let cfg = unsafe { SCStreamConfiguration::new() };
        unsafe {
            cfg.setWidth(capture_w);
            cfg.setHeight(capture_h);
            cfg.setSourceRect(CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: frame.size,
            });
            cfg.setPixelFormat(PIXEL_FORMAT_420V);
            cfg.setMinimumFrameInterval(objc2_core_media::CMTime::new(1, settings.fps));
            cfg.setQueueDepth(6);
            cfg.setShowsCursor(settings.show_cursor);
            // 系统声音采集同时充当混音载体：仅录麦克风时其采样会被静音覆盖。
            cfg.setCapturesAudio(settings.audio.any());
            cfg.setCaptureMicrophone(settings.audio.wants_mic());
            cfg.setSampleRate(48_000);
            cfg.setChannelCount(2);
            cfg.setExcludesCurrentProcessAudio(true);
            cfg.setScalesToFit(false);
            cfg.setPreservesAspectRatio(true);
        }

        let path = unique_output_path(&settings.out_dir);
        let url =
            NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path.to_str().unwrap()), false);
        let (writer, audio_in) = make_writer(&url, settings)?;
        crate::state::log(&format!(
            "start: capture {capture_w}x{capture_h} crop {:?} fps {} bpp {} audio {:?}",
            crop, settings.fps, settings.quality.bpp(), settings.audio
        ));
        let _ = crop;
        if std::env::var("SCREEN_REC_DEBUG").is_ok() {
            eprintln!(
                "irec: capture {}x{} crop {:?}",
                capture_w, capture_h, crop
            );
        }

        // Our own windows (control window, region HUD) must stay out of the
        // video, otherwise the selection frame would be recorded as well.
        let excluded = own_windows(&content);
        let excluded_count = excluded.len();
        log(&format!("start: excluding {excluded_count} own window(s)"));
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &excluded,
            )
        };

        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &cfg, None)
        };

        let shared: Shared = Arc::new(Mutex::new(RecorderState {
            writer: writer.clone(),
            video_in: None,
            frame_writer: None,
            audio_in: audio_in.clone(),
            settings: settings.clone(),
            crop,
            mic_queue: Vec::new(),
            mic_unsupported_logged: false,
            mic_started_at: None,
            mic_wait_buffers: 0,
            mic_peak: 0.0,
            mic_underruns: 0,
            mic_frames_seen: 0,
            mic_t0: None,
            mic_rate_logged: false,
            write_started: false,
            session_started: false,
            stopped: false,
            frames: 0,
            error: None,
        }));
        {
            let mut guard = CURRENT.lock().unwrap();
            *guard = Some(shared.clone());
        }

        let handler = SampleHandler::new();
        let queue = DispatchQueue::new("irec.sample", None);
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*handler),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| e.to_string())?;
            if settings.audio.any() {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        ProtocolObject::from_ref(&*handler),
                        SCStreamOutputType::Audio,
                        Some(&queue),
                    )
                    .map_err(|e| e.to_string())?;
            }
            if settings.audio.wants_mic() {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        ProtocolObject::from_ref(&*handler),
                        SCStreamOutputType::Microphone,
                        Some(&queue),
                    )
                    .map_err(|e| e.to_string())?;
            }
            let (done, error) = wait_flag();
            let done_c = done.clone();
            let error_c = error.clone();
            let block = RcBlock::new(move |err: *mut NSError| {
                if !err.is_null() {
                    *error_c.lock().unwrap() = Some(ns_error_message(err));
                }
                done_c.store(true, Ordering::SeqCst);
            });
            stream.startCaptureWithCompletionHandler(Some(&block));
            wait_for(&done, 10.0, "startCapture");
            if let Some(msg) = error.lock().ok().and_then(|g| g.clone()) {
                let mut guard = CURRENT.lock().unwrap();
                *guard = None;
                return Err(format!("无法开始录屏：{msg}"));
            }
        }

        Ok(Self {
            stream,
            _output: handler,
            _queue: queue,
            shared,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when the writer already gave up, so the UI can stop right away.
    pub fn failed(&self) -> Option<String> {
        self.shared
            .lock()
            .ok()
            .and_then(|state| state.error.clone())
    }

    /// Stop capture, finalise the file and return (path, bytes, frames).
    pub fn stop(self) -> Result<(PathBuf, u64, u64), String> {
        if let Ok(mut state) = self.shared.lock() {
            state.stopped = true;
        }

        let (done, error) = wait_flag();
        let done_c = done.clone();
        let error_c = error.clone();
        let block = RcBlock::new(move |err: *mut NSError| {
            if !err.is_null() {
                *error_c.lock().unwrap() = Some(ns_error_message(err));
            }
            done_c.store(true, Ordering::SeqCst);
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&block)) };
        wait_for(&done, 10.0, "stopCapture");
        let stop_error = error.lock().ok().and_then(|g| g.clone());

        {
            let mut guard = CURRENT.lock().unwrap();
            *guard = None;
        }

        let state = self.shared.lock().map_err(|e| e.to_string())?;
        if state.settings.audio.wants_mic() {
            log(&format!(
                "mic: peak={:.5} gain=x{} underrun_buffers={}",
                state.mic_peak, audio::MIC_GAIN, state.mic_underruns
            ));
        }

        if let Some(audio_in) = &state.audio_in {
            unsafe { audio_in.markAsFinished() };
        }
        if let Some(video_in) = &state.video_in {
            unsafe { video_in.markAsFinished() };
        }

        if state.write_started {
            let (done2, _) = wait_flag();
            let done2_c = done2.clone();
            let block = RcBlock::new(move || {
                done2_c.store(true, Ordering::SeqCst);
            });
            unsafe { state.writer.finishWritingWithCompletionHandler(&block) };
            wait_for(&done2, 30.0, "finishWriting");
        }

        let status = unsafe { state.writer.status() };
        let writer_error = unsafe { state.writer.error() };
        let frames = state.frames;
        let state_error = state.error.clone();

        crate::state::log(&format!(
            "stop: frames={frames} status={:?} writer_error={:?} state_error={state_error:?}",
            status.0,
            writer_error.as_ref().map(|e| e.to_string())
        ));
        if frames == 0 {
            let _ = std::fs::remove_file(&self.path);
            return Err("没有捕获到任何画面，请检查「屏幕录制」权限。".to_string());
        }
        if let Some(err) = stop_error.filter(|e| !e.is_empty()) {
            return Err(format!("停止录屏失败：{err}"));
        }
        if let Some(err) = state_error {
            return Err(format!("写入视频失败：{err}"));
        }
        if status != AVAssetWriterStatus::Completed {
            let detail = writer_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| format!("writer status {:?}", status.0));
            return Err(format!("视频未正常完成：{detail}"));
        }

        let bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        Ok((self.path.clone(), bytes, frames))
    }
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = AnyThread]
    #[name = "IRecSampleHandler"]
    struct SampleHandler;

    unsafe impl NSObjectProtocol for SampleHandler {}

    unsafe impl SCStreamOutput for SampleHandler {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            handle_sample(sample_buffer, r#type);
        }
    }
);

impl SampleHandler {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn handle_sample(buffer: &CMSampleBuffer, r#type: SCStreamOutputType) {
    let shared = match CURRENT.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(s) => s.clone(),
            None => return,
        },
        Err(_) => return,
    };
    let mut state: MutexGuard<'_, RecorderState> = match shared.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    if state.stopped {
        return;
    }

    if r#type.0 == SCStreamOutputType::Screen.0 {
        // Skip frames ScreenCaptureKit flags as not complete.
        if matches!(frame_status(buffer), Some(status) if status != 0) {
            return;
        }
        if state.frame_writer.is_none() {
            match make_video_input(&state.writer, state.crop, &state.settings) {
                Ok((input, frame_writer)) => {
                    state.video_in = Some(input);
                    state.frame_writer = Some(frame_writer);
                }
                Err(err) => {
                    state.error = Some(err);
                    state.stopped = true;
                    return;
                }
            }
        }
        if !state.write_started {
            let ok = unsafe { state.writer.startWriting() };
            if !ok {
                state.error = Some(writer_error_text(&state.writer));
                state.stopped = true;
                return;
            }
            state.write_started = true;
        }
        let pts = unsafe { buffer.presentation_time_stamp() };
        if !state.session_started {
            unsafe { state.writer.startSessionAtSourceTime(pts) };
            state.session_started = true;
        }

        let ready = state
            .video_in
            .as_ref()
            .map(|input| unsafe { input.isReadyForMoreMediaData() })
            .unwrap_or(false);
        if ready {
            // Always go through our own pixel buffers: ScreenCaptureKit frames
            // are not always encodable as-is (see `frame` module docs).
            let outcome = match (&state.frame_writer, unsafe { buffer.image_buffer() }) {
                (Some(writer), Some(pixel_buffer)) => writer.append(&pixel_buffer, pts),
                _ => Ok(unsafe {
                    state
                        .video_in
                        .as_ref()
                        .map(|input| input.appendSampleBuffer(buffer))
                        .unwrap_or(false)
                }),
            };
            match outcome {
                Ok(true) => state.frames += 1,
                Ok(false) => {
                    let detail = writer_error_text(&state.writer);
                    crate::state::log(&format!("append failed at frame {}: {detail}", state.frames));
                    state.error = Some(format!("第 {} 帧写入失败：{}", state.frames, detail));
                    return;
                }
                Err(err) => {
                    state.error = Some(err);
                    state.stopped = true;
                    return;
                }
            }
        }
    } else if r#type.0 == SCStreamOutputType::Microphone.0 {
        // 麦克风：先排队，等系统声音的采样到达时一起混音。
        audio::log_format(buffer, "mic");
        if let Some((pcm, peak)) = audio::extract_canonical(buffer) {
            state.mic_started_at = Some(std::time::Instant::now());
            state.mic_peak = state.mic_peak.max(peak);
            // 实测每秒收到多少麦克风帧，用来核对 ASBD 声明的采样率。
            let t0 = *state.mic_t0.get_or_insert_with(std::time::Instant::now);
            state.mic_frames_seen += (pcm.len() / audio::CHANNELS) as u32;
            let elapsed = t0.elapsed().as_secs_f64();
            if elapsed >= 0.5 {
                let measured = (state.mic_frames_seen as f64 / elapsed).round() as u32;
                audio::set_measured_mic_rate(measured);
            }
            if !state.mic_rate_logged && elapsed >= 1.5 {
                state.mic_rate_logged = true;
                log(&format!(
                    "mic: measured {:.0} frames/s over {:.1}s (resampled to {} Hz)",
                    state.mic_frames_seen as f64 / elapsed,
                    elapsed,
                    audio::RATE as u32
                ));
            }
            let room = audio::MAX_QUEUED_FRAMES.saturating_sub(state.mic_queue.len());
            let take = pcm.len().min(room);
            state.mic_queue.extend_from_slice(&pcm[..take]);
        } else if !state.mic_unsupported_logged {
            state.mic_unsupported_logged = true;
            log("mic sample skipped: unsupported buffer format");
        }
    } else if state.session_started {
        if r#type.0 == SCStreamOutputType::Audio.0 {
            audio::log_format(buffer, "system");
        }
        if state.settings.audio.wants_mic() && state.mic_started_at.is_none() {
            // 麦克风迟迟没送来采样：多半是权限没给或输入设备没有信号。
            state.mic_wait_buffers += 1;
            if state.mic_wait_buffers == 60 && !state.mic_unsupported_logged {
                state.mic_unsupported_logged = true;
                log("麦克风未收到采样：请检查 系统设置 → 隐私与安全性 → 麦克风 是否已允许，以及输入设备是否有信号");
            }
        }
        if state.settings.audio.wants_mic() {
            let replace = state.settings.audio == AudioSource::Mic;
            if state.mic_queue.is_empty() {
                state.mic_underruns += 1;
            }
            audio::mix_into(buffer, &mut state.mic_queue, replace);
        }
        if let Some(audio_in) = &state.audio_in {
            if unsafe { audio_in.isReadyForMoreMediaData() } {
                let _ = unsafe { audio_in.appendSampleBuffer(buffer) };
            }
        }
    }
}

fn writer_error_text(writer: &AVAssetWriter) -> String {
    match unsafe { writer.error() } {
        Some(e) => format!(
            "{} [{} / {}] userInfo={:?}",
            e.localizedDescription(),
            e.domain(),
            e.code(),
            e.userInfo().description().to_string()
        ),
        None => format!("writer status {:?}", unsafe { writer.status() }.0),
    }
}

/// `SCStreamFrameInfoStatus` of a frame: 0 = complete, 1 = idle, 2 = blank,
/// 3 = suspended, 4 = started, 5 = stopped.
fn frame_status(buffer: &CMSampleBuffer) -> Option<i32> {
    let attachments = unsafe { buffer.sample_attachments_array(false)? };
    if attachments.count() < 1 {
        return None;
    }
    let raw = unsafe { attachments.value_at_index(0) };
    if raw.is_null() {
        return None;
    }
    let dict: &NSDictionary<NSString, AnyObject> =
        unsafe { &*(raw as *const NSDictionary<NSString, AnyObject>) };
    let value = unsafe { dict.objectForKey(SCStreamFrameInfoStatus)? };
    let number: &NSNumber = unsafe { &*(&*value as *const AnyObject as *const NSNumber) };
    Some(number.intValue())
}

/// Creates the writer and the (optional) audio input. The video input is added
/// once the first frame arrives and its real size is known.
fn make_writer(
    url: &NSURL,
    settings: &Settings,
) -> Result<(Retained<AVAssetWriter>, Option<Retained<AVAssetWriterInput>>), String> {
    let audio_in = if settings.audio.any() {
        let audio_settings: Retained<SettingsDict> = NSMutableDictionary::new();
        dict_set_number(&audio_settings, av_key!(AVFormatIDKey), AUDIO_FORMAT_AAC);
        dict_set_number(&audio_settings, av_key!(AVSampleRateKey), 48_000.0);
        dict_set_number(&audio_settings, av_key!(AVNumberOfChannelsKey), 2.0);
        dict_set_number(&audio_settings, av_key!(AVEncoderBitRateKey), 128_000.0);
        let input = unsafe {
            AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                av_key!(AVMediaTypeAudio),
                Some(&audio_settings),
            )
        };
        unsafe { input.setExpectsMediaDataInRealTime(true) };
        Some(input)
    } else {
        None
    };

    let writer = unsafe {
        AVAssetWriter::initWithURL_fileType_error(
            AVAssetWriter::alloc(),
            url,
            av_key!(AVFileTypeMPEG4),
        )
        .map_err(|e| format!("创建 AVAssetWriter 失败：{e}"))?
    };
    unsafe {
        writer.setShouldOptimizeForNetworkUse(false);
    }
    // Write movie fragments: if the app is killed mid-recording the file
    // still opens (moov + fragments) instead of being useless.
    unsafe {
        writer.setMovieFragmentInterval(objc2_core_media::CMTime::new(
            MOVIE_FRAGMENT_SECONDS as i64,
            1,
        ));
    }
    if let Some(audio_in) = &audio_in {
        unsafe {
            if writer.canAddInput(audio_in) {
                writer.addInput(audio_in);
            } else {
                return Err("无法添加音频轨道".to_string());
            }
        }
    }
    Ok((writer, audio_in))
}

/// Builds the HEVC video input for the given pixel size.
fn make_video_input(
    writer: &AVAssetWriter,
    crop: Crop,
    settings: &Settings,
) -> Result<(Retained<AVAssetWriterInput>, FrameWriter), String> {
    let width = crop.width - crop.width % 2;
    let height = crop.height - crop.height % 2;
    if std::env::var("SCREEN_REC_DEBUG").is_ok() {
        eprintln!("irec: encoding {width}x{height} @ {} fps", settings.fps);
    }
    let pixels_per_second = width as f64 * height as f64 * settings.fps as f64;
    let bitrate = (pixels_per_second * settings.quality.bpp()).clamp(1_200_000.0, 40_000_000.0);

    let compression: Retained<SettingsDict> = NSMutableDictionary::new();
    dict_set_number(&compression, av_key!(AVVideoAverageBitRateKey), bitrate);
    dict_set_number(
        &compression,
        av_key!(AVVideoExpectedSourceFrameRateKey),
        settings.fps as f64,
    );
    dict_set_number(
        &compression,
        av_key!(AVVideoMaxKeyFrameIntervalKey),
        (settings.fps * 2) as f64,
    );

    let video_settings: Retained<SettingsDict> = NSMutableDictionary::new();
    unsafe {
        video_settings.setObject_forKey(
            &*av_key!(AVVideoCodecTypeHEVC),
            ProtocolObject::from_ref(av_key!(AVVideoCodecKey)),
        );
    }
    unsafe {
        video_settings.setObject_forKey(
            &*NSNumber::numberWithInt(width as i32),
            ProtocolObject::from_ref(av_key!(AVVideoWidthKey)),
        );
    }
    unsafe {
        video_settings.setObject_forKey(
            &*NSNumber::numberWithInt(height as i32),
            ProtocolObject::from_ref(av_key!(AVVideoHeightKey)),
        );
    }
    let compression_obj: &AnyObject = &compression;
    unsafe {
        video_settings.setObject_forKey(
            compression_obj,
            ProtocolObject::from_ref(av_key!(AVVideoCompressionPropertiesKey)),
        );
    }

    let video_in = unsafe {
        AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
            av_key!(AVMediaTypeVideo),
            Some(&video_settings),
        )
    };
    unsafe { video_in.setExpectsMediaDataInRealTime(true) };

    unsafe {
        if writer.canAddInput(&video_in) {
            writer.addInput(&video_in);
        } else {
            return Err("无法添加视频轨道".to_string());
        }
        }
    let frame_writer = FrameWriter::new(&video_in, Crop { width, height, ..crop })?;
    Ok((video_in, frame_writer))
}



fn dict_set_number(dict: &SettingsDict, key: &'static NSString, value: f64) {
    let number = NSNumber::numberWithDouble(value);
    let object: &AnyObject = &number;
    unsafe { dict.setObject_forKey(object, ProtocolObject::from_ref(key)) };
}

/// CoreGraphics helpers used to trigger / probe the TCC prompt.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Retries once after asking macOS for permission, so the very first recording
/// works right after the user grants access.
fn fetch_shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<Retained<SCShareableContent>, String>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let result = if content.is_null() {
            Err(format!(
                "获取屏幕内容失败：{}",
                ns_error_message(error)
            ))
        } else {
            match unsafe { Retained::retain(content) } {
                Some(c) => Ok(c),
                None => Err("获取屏幕内容失败".to_string()),
            }
        };
        let _ = tx.send(result);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    rx.recv()
        .unwrap_or(Err("获取屏幕内容超时，请检查「屏幕录制」权限。".to_string()))
}

/// Windows owned by this process, to be excluded from the capture.
fn own_windows(content: &SCShareableContent) -> Retained<NSArray<SCWindow>> {
    let pid = std::process::id() as libc::pid_t;
    let mut own: Vec<Retained<SCWindow>> = Vec::new();
    for window in unsafe { content.windows() }.iter() {
        let is_own = unsafe { window.owningApplication() }
            .map(|app| unsafe { app.processID() } == pid)
            .unwrap_or(false);
        if is_own {
            own.push(window);
        }
    }
    NSArray::from_retained_slice(&own)
}

fn main_display(content: &SCShareableContent) -> Result<Retained<SCDisplay>, String> {
    let displays = unsafe { content.displays() };
    if displays.is_empty() {
        return Err("没有找到可录制的显示器".to_string());
    }
    let main_id = unsafe { CGMainDisplayID() };
    for display in displays.iter() {
        if unsafe { display.displayID() } == main_id {
            return Ok(display);
        }
    }
    Ok(displays.objectAtIndex(0))
}

fn unique_output_path(dir: &Path) -> PathBuf {
    let stamp = local_timestamp();
    let mut candidate = dir.join(format!("iRec-{stamp}.mp4"));
    let mut i = 1;
    while candidate.exists() {
        candidate = dir.join(format!("iRec-{stamp}-{i}.mp4"));
        i += 1;
    }
    candidate
}

fn local_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut out = String::new();
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let now = secs;
    unsafe { libc::localtime_r(&now, &mut tm) };
    let mut buf = [0i8; 32];
    unsafe {
        libc::strftime(
            buf.as_mut_ptr(),
            buf.len(),
            c"%Y%m%d-%H%M%S".as_ptr(),
            &tm,
        );
    }
    for &b in buf.iter() {
        if b == 0 {
            break;
        }
        out.push(b as u8 as char);
    }
    if out.is_empty() {
        let _ = write!(out, "{secs}");
    }
    out
}

fn ns_error_message(ptr: *mut NSError) -> String {
    if ptr.is_null() {
        return String::new();
    }
    match unsafe { Retained::retain(ptr) } {
        Some(err) => err.localizedDescription().to_string(),
        None => String::new(),
    }
}

type Flag = Arc<AtomicBool>;

fn wait_flag() -> (Flag, Arc<Mutex<Option<String>>>) {
    (
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(None)),
    )
}

/// Block the calling thread until `flag` flips or `timeout` elapses.
fn wait_for(flag: &AtomicBool, timeout: f64, what: &str) {
    let start = std::time::Instant::now();
    while !flag.load(Ordering::SeqCst) {
        if start.elapsed().as_secs_f64() > timeout {
            eprintln!("iRec: timed out waiting for {what}");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
}
