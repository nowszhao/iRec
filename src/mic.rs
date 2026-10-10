//! 麦克风采集：用 AVAudioEngine 直接拿 48 kHz 立体声 PCM。
//!
//! 不走 ScreenCaptureKit 的 `SCStreamOutputType::Microphone`：那条路给的是设备原生
//! 格式（本机实测声明 24 kHz 单声道），而且声明的采样率和真实送达速率不一致，照它
//! 重采样会把声音拉慢、变调。AVAudioEngine 由 CoreAudio 完成高质量转换，拿到的就是
//! 我们需要的 48 kHz 立体声 float，混音时 1:1 即可。

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_avf_audio::{
    AVAudioEngine, AVAudioFormat, AVAudioPCMBuffer, AVAudioTime,
};

use crate::audio;

/// 采集到的 48 kHz 立体声交错采样（跨线程共享）。
pub type Queue = Arc<Mutex<Vec<f32>>>;

type TapBlock = dyn Fn(NonNull<AVAudioPCMBuffer>, NonNull<AVAudioTime>);

static CURRENT: Mutex<Option<Mic>> = Mutex::new(None);

struct Mic {
    engine: Retained<AVAudioEngine>,
    _block: RcBlock<TapBlock>,
    queue: Queue,
}

// SAFETY: engine / block / queue 都只在 start / stop 之间被访问，stop 在主线程调用。
unsafe impl Send for Mic {}

/// 启动麦克风采集；已经在采集时直接返回既有的队列。
pub fn start() -> Result<Queue, String> {
    let mut guard = CURRENT.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(mic) = guard.as_ref() {
        return Ok(mic.queue.clone());
    }

    let engine = unsafe { AVAudioEngine::new() };
    let input = unsafe { engine.inputNode() };
    let format = unsafe {
        AVAudioFormat::initStandardFormatWithSampleRate_channels(
            AVAudioFormat::alloc(),
            audio::RATE,
            audio::CHANNELS as u32,
        )
    };
    let Some(format) = format else {
        return Err("无法创建 48 kHz 立体声采集格式".to_string());
    };

    let queue: Queue = Arc::new(Mutex::new(Vec::new()));
    let sink = queue.clone();
    let block: RcBlock<TapBlock> = RcBlock::new(move |buf: NonNull<AVAudioPCMBuffer>, _when| {
        let pcm = unsafe { buf.as_ref() };
        let frames = unsafe { pcm.frameLength() } as usize;
        if frames == 0 {
            return;
        }
        let channels = unsafe { pcm.format().channelCount() } as usize;
        let data = unsafe { pcm.floatChannelData() };
        if data.is_null() {
            return;
        }
        let left = unsafe { *data };
        let right = if channels >= 2 {
            unsafe { *data.add(1) }
        } else {
            left
        };
        let Ok(mut q) = sink.lock() else { return };
        let room = audio::MAX_QUEUED_FRAMES.saturating_sub(q.len());
        let take = (frames * audio::CHANNELS).min(room);
        for i in 0..(take / audio::CHANNELS) {
            q.push(unsafe { *left.as_ptr().add(i) });
            q.push(unsafe { *right.as_ptr().add(i) });
        }
    });
    let tap: *mut block2::DynBlock<TapBlock> =
        &*block as *const block2::DynBlock<TapBlock> as *mut block2::DynBlock<TapBlock>;

    unsafe { input.installTapOnBus_bufferSize_format_block(0, 4096, Some(&format), tap) };
    unsafe { engine.prepare() };
    unsafe { engine.startAndReturnError() }.map_err(|e| format!("麦克风启动失败：{e}"))?;

    crate::state::log(&format!(
        "mic: AVAudioEngine started at {} Hz, {} ch",
        audio::RATE as u32,
        audio::CHANNELS
    ));
    let shared = queue.clone();
    *guard = Some(Mic {
        engine,
        _block: block,
        queue,
    });
    Ok(shared)
}

/// 停止采集并释放引擎。
pub fn stop() {
    let taken = CURRENT.lock().ok().and_then(|mut g| g.take());
    if let Some(mic) = taken {
        unsafe { mic.engine.stop() };
        unsafe { mic.engine.inputNode().removeTapOnBus(0) };
        drop(mic._block);
    }
}
