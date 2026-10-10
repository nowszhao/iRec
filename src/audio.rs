//! PCM 读取与混音：把麦克风采样混进系统声音，输出单一音轨。
//!
//! ScreenCaptureKit 的麦克风输出（`SCStreamOutputType::Microphone`）是设备原生
//! 格式的 PCM，包在 AudioBufferList 里；系统声音则是另一路 PCM。这里把它们统一
//! 换算到 48 kHz 立体声 float，再写回系统声音的采样缓冲，最后只编码一条音轨。

use objc2::rc::Retained;
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioStreamBasicDescription};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer,
};

/// 混音器内部格式：48 kHz 立体声 float，交错存放。
pub const RATE: f64 = 48_000.0;
pub const CHANNELS: usize = 2;

/// 麦克风增益。Mac 内建麦克风实测峰值只有 -54 dB 左右，直接混入几乎听不见，
/// 这里统一放大约 +10 dB（可按需调整）。
pub const MIC_GAIN: f32 = 1.8;

/// 实测得到的麦克风速率（Hz）。ScreenCaptureKit 声明的采样率在部分机器上是
/// 实际送达速率的一半（声明 24 kHz、实际 48 kHz），直接照声明重采样会把声音
/// 放慢一倍，所以这里用实测值，测量出来之前才回落到 ASBD。
static MEASURED_MIC_RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn set_measured_mic_rate(hz: u32) {
    MEASURED_MIC_RATE.store(hz, std::sync::atomic::Ordering::SeqCst);
}

fn measured_mic_rate() -> Option<f64> {
    match MEASURED_MIC_RATE.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        hz => Some(hz as f64),
    }
}

/// 分别记录麦克风 / 系统声音的输入格式（各一次）。
static LOG_MIC_FORMAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LOG_SYS_FORMAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn log_format(buffer: &CMSampleBuffer, label: &str) {
    let flag = if label == "mic" { &LOG_MIC_FORMAT } else { &LOG_SYS_FORMAT };
    if flag.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Some(desc) = (unsafe { buffer.format_description() }) else { return };
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&desc) };
    if asbd.is_null() {
        return;
    }
    let a = unsafe { &*asbd };
    crate::state::log(&format!(
        "{} format: rate={} ch={} bits={} {}",
        label,
        a.mSampleRate,
        a.mChannelsPerFrame,
        a.mBitsPerChannel,
        if (a.mFormatFlags & FLAG_FLOAT) != 0 { "float" } else { "int" }
    ));
}

/// 软限幅：轻声几乎原样通过，接近满刻度时平滑压缩，避免硬削波的毛刺感。
fn soft_limit(v: f32) -> f32 {
    if v.abs() < 0.7 {
        v
    } else {
        let sign = if v < 0.0 { -1.0 } else { 1.0 };
        sign * (0.7 + 0.3 * (1.0 - (-(v.abs() - 0.7) / 0.3).exp()))
    }
}

const FORMAT_LINEAR_PCM: u32 = 0x6C70_636D; // 'lpcm'
const FLAG_FLOAT: u32 = 1 << 0;
const FLAG_PACKED: u32 = 1 << 3;
/// 麦克风队列最多缓存 2 秒，异常情况下不会无限增长。
pub const MAX_QUEUED_FRAMES: usize = (RATE * 2.0) as usize * CHANNELS;

#[derive(Clone, Copy)]
struct Format {
    rate: f64,
    channels: usize,
    float: bool,
    bits: u32,
}

fn format_of(buffer: &CMSampleBuffer) -> Option<Format> {
    let desc = unsafe { buffer.format_description() }?;
    let asbd: *const AudioStreamBasicDescription =
        unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&desc) };
    if asbd.is_null() {
        return None;
    }
    let a = unsafe { &*asbd };
    if a.mFormatID != FORMAT_LINEAR_PCM || (a.mFormatFlags & FLAG_PACKED) == 0 {
        return None;
    }
    Some(Format {
        rate: if a.mSampleRate > 0.0 { a.mSampleRate } else { RATE },
        channels: a.mChannelsPerFrame.max(1) as usize,
        float: (a.mFormatFlags & FLAG_FLOAT) != 0,
        bits: a.mBitsPerChannel,
    })
}

struct Raw {
    /// 每个 buffer 的起始指针：交错时只有 1 个，平面时按声道排列。
    ptrs: Vec<*mut u8>,
    bytes: usize,
    interleaved: bool,
    _block: Option<Retained<CMBlockBuffer>>,
    _cf_block: Option<objc2_core_foundation::CFRetained<CMBlockBuffer>>,
}

fn raw_buffers(buffer: &CMSampleBuffer, channels: usize) -> Option<Raw> {
    // 首选直接从 block buffer 读：单声道/交错 PCM 这条路径最简单。
    if let Some(block) = unsafe { buffer.data_buffer() } {
        let mut length = 0usize;
        let mut total = 0usize;
        let mut ptr: *mut std::os::raw::c_char = std::ptr::null_mut();
        let status = unsafe { block.data_pointer(0, &mut length, &mut total, &mut ptr) };
        if status == 0 && !ptr.is_null() && length > 0 {
            return Some(Raw {
                ptrs: vec![ptr as *mut u8],
                bytes: length,
                interleaved: channels > 1,
                _block: None,
                _cf_block: Some(block),
            });
        }
    }
    let mut storage = vec![0u128; 64];
    let list = storage.as_mut_ptr() as *mut AudioBufferList;
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    let mut needed = 0usize;
    let status = unsafe {
        buffer.audio_buffer_list_with_retained_block_buffer(
            &mut needed, list, 1024, None, None, 0, &mut block,
        )
    };
    if status != 0 {
        return None;
    }
    let abl = unsafe { &*list };
    let count = abl.mNumberBuffers as usize;
    if count == 0 {
        return None;
    }
    let bufs: &[AudioBuffer] = unsafe { std::slice::from_raw_parts(abl.mBuffers.as_ptr(), count) };
    let retained = if block.is_null() {
        None
    } else {
        unsafe { Retained::retain(block) }
    };
    Some(Raw {
        ptrs: bufs.iter().map(|b| b.mData as *mut u8).collect(),
        bytes: bufs[0].mDataByteSize as usize,
        interleaved: count == 1 && channels > 1,
        _block: retained,
        _cf_block: None,
    })
}

fn sample_size(fmt: &Format) -> Option<usize> {
    match (fmt.float, fmt.bits) {
        (true, 32) | (false, 32) => Some(4),
        (false, 16) => Some(2),
        _ => None,
    }
}

/// 采样缓冲中的帧数（每声道）。
fn frame_count(fmt: &Format, raw: &Raw) -> Option<usize> {
    let size = sample_size(fmt)?;
    if raw.interleaved {
        Some(raw.bytes / (size * fmt.channels))
    } else {
        Some(raw.bytes / size)
    }
}

fn read_sample(fmt: &Format, base: *const u8, index: usize) -> f32 {
    unsafe {
        match (fmt.float, fmt.bits) {
            (true, 32) => *(base.add(index * 4) as *const f32),
            (false, 32) => *(base.add(index * 4) as *const i32) as f32 / i32::MAX as f32,
            (false, 16) => *(base.add(index * 2) as *const i16) as f32 / i16::MAX as f32,
            _ => 0.0,
        }
    }
}

fn write_sample(fmt: &Format, base: *mut u8, index: usize, value: f32) {
    unsafe {
        match (fmt.float, fmt.bits) {
            (true, 32) => *(base.add(index * 4) as *mut f32) = value,
            (false, 32) => {
                *(base.add(index * 4) as *mut i32) = (value * i32::MAX as f32) as i32;
            }
            (false, 16) => {
                *(base.add(index * 2) as *mut i16) = (value * i16::MAX as f32) as i16;
            }
            _ => {}
        }
    }
}

/// 把采样缓冲读成「按声道分开的 f32」，保留原始采样率。
fn read_planar(buffer: &CMSampleBuffer) -> Option<(Format, Vec<Vec<f32>>)> {
    let fmt = format_of(buffer)?;
    sample_size(&fmt)?;
    let raw = raw_buffers(buffer, fmt.channels)?;
    let frames = frame_count(&fmt, &raw)?;
    let channels = if raw.interleaved { fmt.channels } else { raw.ptrs.len() };
    let mut planar: Vec<Vec<f32>> = (0..channels).map(|_| Vec::with_capacity(frames)).collect();
    for c in 0..channels {
        let ptr = raw.ptrs[if raw.interleaved { 0 } else { c }];
        for i in 0..frames {
            let index = if raw.interleaved { i * fmt.channels + c } else { i };
            planar[c].push(read_sample(&fmt, ptr, index));
        }
    }
    Some((fmt, planar))
}

fn resample(input: &[f32], from: f64, to: f64) -> Vec<f32> {
    if input.is_empty() || (from - to).abs() < 1.0 {
        return input.to_vec();
    }
    let out_len = ((input.len() as f64) * to / from).ceil() as usize;
    let mut out = Vec::with_capacity(out_len);
    let step = from / to;
    for i in 0..out_len {
        let pos = i as f64 * step;
        let left = pos.floor() as usize;
        let right = (left + 1).min(input.len() - 1);
        let frac = pos - left as f64;
        out.push(input[left] + (input[right] - input[left]) * frac as f32);
    }
    out
}

/// 麦克风采样 → 48 kHz 立体声交错的 f32，同时返回这段的峰值（整段统计用）。
pub fn extract_canonical(buffer: &CMSampleBuffer) -> Option<(Vec<f32>, f32)> {
    let (fmt, planar) = read_planar(buffer)?;
    let fmt = Format {
        rate: measured_mic_rate().unwrap_or(fmt.rate),
        ..fmt
    };
    let channels: Vec<Vec<f32>> = match planar.len() {
        0 => return None,
        1 => vec![resample(&planar[0], fmt.rate, RATE); 2],
        n => (0..2)
            .map(|c| resample(&planar[c.min(n - 1)], fmt.rate, RATE))
            .collect(),
    };
    let frames = channels[0].len();
    let mut out = Vec::with_capacity(frames * CHANNELS);
    let mut peak = 0.0f32;
    for i in 0..frames {
        out.push(channels[0][i]);
        out.push(channels[1][i]);
        peak = peak.max(channels[0][i].abs()).max(channels[1][i].abs());
    }
    Some((out, peak))
}

static LOGGED_MIX_FORMAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 按时间轴对齐取一段麦克风采样。
///
/// `mic_start` 是队列头对应的时间（秒），`from` / `to` 是目标区间（秒）。
/// 返回交叠部分的采样（不足的地方由调用方补零），并把队列头推进到 `to`。
pub fn take_range(mic: &mut Vec<f32>, mic_start: &mut f64, from: f64, to: f64, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; frames * CHANNELS];
    if mic.is_empty() {
        return out;
    }
    let rate = RATE;
    // 队列头晚于目标区间开头：丢弃迟到的部分
    let skip = ((from - *mic_start) * rate).round() as isize;
    if skip > 0 {
        let skip = (skip as usize).min(mic.len() / CHANNELS);
        mic.drain(..skip * CHANNELS);
        *mic_start += skip as f64 / rate;
    }
    let start_offset = ((from - *mic_start) * rate).round();
    let take = frames.min(mic.len() / CHANNELS);
    if take > 0 {
        out[..take * CHANNELS].copy_from_slice(&mic[..take * CHANNELS]);
        mic.drain(..take * CHANNELS);
        *mic_start += take as f64 / rate;
    }
    let _ = (to, start_offset);
    out
}

/// 缓冲里有多少帧（每声道）。
pub fn buffer_frames(buffer: &CMSampleBuffer) -> Option<usize> {
    let fmt = format_of(buffer)?;
    sample_size(&fmt)?;
    let raw = raw_buffers(buffer, fmt.channels)?;
    frame_count(&fmt, &raw)
}

/// 把 48 kHz 立体声交错的采样写进系统声音的采样缓冲（带增益与软限幅）。
/// `replace` 为 true 时先用静音覆盖原内容（仅录麦克风模式）。
pub fn write_into(buffer: &CMSampleBuffer, samples: &[f32], replace: bool) -> bool {
    let Some(fmt) = format_of(buffer) else { return false };
    if sample_size(&fmt).is_none() {
        return false;
    }
    let Some(raw) = raw_buffers(buffer, fmt.channels) else { return false };
    let Some(frames) = frame_count(&fmt, &raw) else { return false };
    if frames == 0 {
        return false;
    }
    let channels = if raw.interleaved { fmt.channels } else { raw.ptrs.len() };
    let step = RATE / fmt.rate.max(1.0);
    let available = samples.len() / CHANNELS;

    for c in 0..channels {
        let ptr = raw.ptrs[if raw.interleaved { 0 } else { c }];
        for i in 0..frames {
            let pos = i as f64 * step;
            let left = pos.floor() as usize;
            let mic_sample = if left + 1 < available {
                let frac = (pos - left as f64) as f32;
                let a = samples[left * CHANNELS + c.min(CHANNELS - 1)];
                let b = samples[(left + 1) * CHANNELS + c.min(CHANNELS - 1)];
                a + (b - a) * frac
            } else {
                samples
                    .get(left * CHANNELS + c.min(CHANNELS - 1))
                    .copied()
                    .unwrap_or(0.0)
            } * MIC_GAIN;
            let index = if raw.interleaved { i * fmt.channels + c } else { i };
            let current = if replace { 0.0 } else { read_sample(&fmt, ptr, index) };
            write_sample(&fmt, ptr, index, soft_limit(current + mic_sample));
        }
    }
    true
}
