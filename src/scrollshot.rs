//! Accessibility-guided scrolling screenshots for vertically scrollable content.

use std::path::PathBuf;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSRunningApplication, NSWorkspace};
use objc2_core_foundation::{
    CFArray, CFData, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGEvent,
    CGEventTapLocation, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGScrollEventUnit,
    CGWarpMouseCursorPosition,
};
use objc2_foundation::{MainThreadMarker, NSError};
use objc2_screen_capture_kit::{SCContentFilter, SCScreenshotManager, SCStreamConfiguration};

use crate::recorder::{fetch_shareable_content, main_display, own_windows};
use crate::scrollpreview;
use crate::state::{Action, Rect, ScrollShotResult, log, push_action};

const CONTROL_CONTINUE: u8 = 0;
const CONTROL_FINISH: u8 = 1;
const CONTROL_CANCEL: u8 = 2;
const MAX_SCROLLS: usize = 90;
const MAX_IMAGE_HEIGHT: usize = 50_000;
const MAX_IMAGE_PIXELS: usize = 60_000_000;
const MAX_CAPTURE_PIXELS: usize = 20_000_000;
const MAX_STATIONARY_SCROLLS: usize = 3;
const MAX_UNRENDERED_SCROLLS: usize = 12;
const OVERLAP_SAMPLE_COLUMNS: usize = 48;
const OVERLAP_SAMPLE_ROWS: usize = 72;
const FIXED_UI_SEARCH_START: usize = 48;
const FIXED_UI_SEARCH_END: usize = 99;
const FIXED_UI_MIN_RUN: usize = 8;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static CONTROL: AtomicU8 = AtomicU8::new(CONTROL_CONTINUE);
static TARGET_PID: AtomicI32 = AtomicI32::new(0);
static CROP_READY: AtomicBool = AtomicBool::new(false);
static WORKER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

type AxError = i32;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> *const CFType;
    fn AXUIElementCreateApplication(pid: libc::pid_t) -> *const CFType;
    fn AXUIElementGetPid(element: *const CFType, pid: *mut libc::pid_t) -> AxError;
    fn AXUIElementCopyElementAtPosition(
        application: *const CFType,
        x: f32,
        y: f32,
        element: *mut *const CFType,
    ) -> AxError;
    fn AXUIElementCopyAttributeValue(
        element: *const CFType,
        attribute: *const CFString,
        value: *mut *const CFType,
    ) -> AxError;
    fn AXUIElementSetAttributeValue(
        element: *const CFType,
        attribute: *const CFString,
        value: *const CFType,
    ) -> AxError;
}

/// Capture the app that was active before the region picker takes focus.
pub fn frontmost_target_pid() -> Result<libc::pid_t, String> {
    let app = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .ok_or_else(|| "没有找到当前前台应用".to_string())?;
    let pid = app.processIdentifier();
    if pid <= 0 || pid == std::process::id() as libc::pid_t {
        return Err("请先切换到要滚动截屏的应用，再启动滚动截屏".to_string());
    }
    Ok(pid)
}

pub fn start(rect: Rect, target_pid: libc::pid_t, scale: f64, dir: PathBuf) {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return;
    }
    CONTROL.store(CONTROL_CONTINUE, Ordering::Release);
    CROP_READY.store(false, Ordering::Release);
    TARGET_PID.store(target_pid, Ordering::Release);
    let worker = thread::spawn(move || {
        let result = match capture(rect, target_pid, scale, dir) {
            Ok(((), warning)) => ScrollShotResult::ReadyForCrop(warning),
            Err(err) if err == "__cancelled__" || err.starts_with("__cancelled__；") => {
                if err != "__cancelled__" {
                    log(&format!("scroll shot cancelled; restore warning: {err}"));
                }
                ScrollShotResult::Cancelled
            }
            Err(err) => {
                log(&format!("scroll shot failed: {err}"));
                ScrollShotResult::Failed(err)
            }
        };
        push_action(Action::ScrollShotFinished(result));
    });
    *WORKER.lock().unwrap_or_else(|e| e.into_inner()) = Some(worker);
}

pub fn request_finish() {
    if ACTIVE.load(Ordering::Acquire) {
        // A cancel already in flight must win; do not turn it into a finish.
        let _ = CONTROL.compare_exchange(
            CONTROL_CONTINUE,
            CONTROL_FINISH,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

pub fn activate_target(_mtm: MainThreadMarker) -> bool {
    let pid = TARGET_PID.load(Ordering::Acquire);
    pid > 0 && activate_pid(_mtm, pid)
}

pub fn activate_pid(_mtm: MainThreadMarker, pid: libc::pid_t) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .map(|target| {
            target.activateWithOptions(objc2_app_kit::NSApplicationActivationOptions::empty())
        })
        .unwrap_or(false)
}

pub fn request_cancel() {
    if ACTIVE.load(Ordering::Acquire) {
        CONTROL.store(CONTROL_CANCEL, Ordering::Release);
    }
}

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

pub fn crop_is_ready() -> bool {
    CROP_READY.load(Ordering::Acquire)
}

pub fn set_crop_ready(ready: bool) {
    CROP_READY.store(ready, Ordering::Release);
}

pub fn cancel_requested() -> bool {
    CONTROL.load(Ordering::Acquire) == CONTROL_CANCEL
}

pub fn cancel_and_wait() {
    request_cancel();
    if let Some(worker) = WORKER.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = worker.join();
    }
    ACTIVE.store(false, Ordering::Release);
    TARGET_PID.store(0, Ordering::Release);
}

pub fn finish_on_main_thread(mtm: MainThreadMarker) {
    if let Some(worker) = WORKER.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = worker.join();
    }
    ACTIVE.store(false, Ordering::Release);
    TARGET_PID.store(0, Ordering::Release);
    NSApplication::sharedApplication(mtm)
        .setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

fn capture(
    rect: Rect,
    target_pid: libc::pid_t,
    scale: f64,
    dir: PathBuf,
) -> Result<((), Option<String>), String> {
    if !crate::permissions::accessibility() {
        log("scroll shot: Accessibility permission unavailable; using visual scroll detection");
    }
    if rect.w < 80.0 || rect.h < 80.0 {
        return Err("滚动截屏选区太小，请至少框选 80 × 80 的页面区域".to_string());
    }
    log(&format!(
        "scroll shot: start pid {target_pid} rect {:.0},{:.0} {:.0}x{:.0}",
        rect.x, rect.y, rect.w, rect.h
    ));

    wait_for_target_frontmost(target_pid)?;
    (|| {
        thread::sleep(Duration::from_millis(350));
        let content = fetch_shareable_content()?;
        let display = main_display(&content)?;
        let display_frame = unsafe { display.frame() };
        let display_h = display_frame.size.height;
        let source = CGRect {
            origin: CGPoint {
                x: rect.x,
                y: display_h - rect.y - rect.h,
            },
            size: CGSize {
                width: rect.w,
                height: rect.h,
            },
        };
        let center = CGPoint {
            x: rect.x + rect.w / 2.0,
            y: display_h - rect.y - rect.h / 2.0,
        };
        let target = find_scroll_target(target_pid, center)?;
        let mut session = ScrollSession::new(target_pid, center, target);
        log(&format!(
            "scroll shot: wheel target at selection center {:.0},{:.0}; accessibility scrollbar={} value={}",
            center.x,
            center.y,
            session.has_scrollbar(),
            session
                .value()
                .map_or_else(|| "unavailable".to_string(), |value| format!("{value:.3}"))
        ));
        let scale = scale.clamp(1.0, 2.0);
        let width = (rect.w * scale).round().max(2.0) as usize;
        let height = (rect.h * scale).round().max(2.0) as usize;
        let frame_pixels = width
            .checked_mul(height)
            .ok_or_else(|| "选区尺寸无效".to_string())?;
        if frame_pixels > MAX_CAPTURE_PIXELS {
            return Err("选区过大，请缩小选区后重试".to_string());
        }

        let excluded = own_windows(&content);
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &excluded,
            )
        };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(width);
            config.setHeight(height);
            config.setSourceRect(source);
            config.setShowsCursor(false);
            config.setScalesToFit(false);
            config.setPreservesAspectRatio(true);
        }

        let initial = capture_frame(&filter, &config)?;
        scrollpreview::publish(
            initial.width,
            initial.height,
            &initial.rgba,
            initial.height,
            1,
        );
        // Put the pointer into the page once so the first wheel event lands
        // on the content. After this the pointer is the user's; moving it
        // outside the selection pauses the capture instead of fighting it.
        CGWarpMouseCursorPosition(center);
        let mut no_scroll_progress = false;
        let mut partial_stitch_warning = None;
        let mut stitched = initial.clone();
        let mut capture_height = initial.height;
        let mut screens = 1;
        let mut user_finished = false;
        let mut last_value = session.value();
        let mut pending_steps = Vec::new();
        let mut previous = initial.clone();
        let inner_result = (|| {
            let mut fixed_ui_checked = false;
            let mut scrolls = 0;
            let mut last_shift = None;
            let mut stationary_scrolls = 0;
            let mut unrendered_scrolls = 0;
            let scroll_lines = (rect.h / 200.0).round().clamp(2.0, 4.0) as i32;
            // CGEvent uses a negative vertical delta to move down the page.
            // If that direction is blocked at the current edge, the capture
            // loop retries in the opposite direction and prepends the frames.
            let mut scroll_delta = -scroll_lines;
            let mut tried_reverse = false;
            let mut stitch_upward = false;
            push_action(Action::ScrollShotProgress(
                "正在滚动截屏… 可随时完成或取消".to_string(),
            ));

            while scrolls < MAX_SCROLLS && stitched.height < MAX_IMAGE_HEIGHT {
                if CONTROL.load(Ordering::Acquire) == CONTROL_CANCEL {
                    return Err("__cancelled__".to_string());
                }
                let mut finish_requested = CONTROL.load(Ordering::Acquire) == CONTROL_FINISH;
                if !finish_requested {
                    match wait_for_cursor(rect, display_h) {
                        PauseResult::Continue => {}
                        PauseResult::Finish => {
                            user_finished = true;
                            break;
                        }
                        PauseResult::Cancel => return Err("__cancelled__".to_string()),
                    }
                    ensure_target_frontmost(target_pid)?;
                    // Use the same point used for AX hit testing so wheel
                    // events keep targeting the selected scroll view even
                    // when the pointer rests over a fixed button in the area.
                    post_scroll_at(center, scroll_delta)?;
                    scrolls += 1;
                    pending_steps.push(scroll_delta);
                    thread::sleep(Duration::from_millis(450));
                    if CONTROL.load(Ordering::Acquire) == CONTROL_CANCEL {
                        return Err("__cancelled__".to_string());
                    }
                    if CONTROL.load(Ordering::Acquire) == CONTROL_FINISH {
                        finish_requested = true;
                        user_finished = true;
                    }
                }
                ensure_target_frontmost(target_pid)?;
                let mut frame = capture_frame(&filter, &config)?;
                let value = session.value();
                let value_changed = match (last_value, value) {
                    (Some(before), Some(after)) => (after - before).abs() > 0.000_01,
                    _ => false,
                };
                let reached_scroll_end = match (last_value, value) {
                    (Some(before), Some(after)) => session.reached_scroll_end(before, after),
                    _ => false,
                };
                last_value = value;

                if frame.is_nearly_identical(&previous) {
                    stationary_scrolls += 1;
                    if finish_requested {
                        if value_changed && !pending_steps.is_empty() {
                            session.record_steps(&pending_steps);
                        }
                        break;
                    }
                    if !session.has_scrollbar() {
                        thread::sleep(Duration::from_millis(300));
                        frame = capture_frame(&filter, &config)?;
                        if !frame.is_nearly_identical(&previous) {
                            stationary_scrolls = 0;
                        } else if stationary_scrolls >= MAX_STATIONARY_SCROLLS {
                            if retry_opposite_direction(
                                screens,
                                &mut tried_reverse,
                                &mut scroll_delta,
                                &mut stationary_scrolls,
                                &mut pending_steps,
                                &mut stitch_upward,
                            ) {
                                log(
                                    "scroll shot: initial direction did not move content; trying reverse direction",
                                );
                                continue;
                            }
                            log(&format!(
                                "scroll shot: no visual movement after trying both directions ({stationary_scrolls} stationary events)"
                            ));
                            break;
                        } else {
                            continue;
                        }
                    } else if value_changed {
                        // Accessibility confirms the document is moving even
                        // when this frame contains mostly blank or unchanged
                        // pixels. Keep scrolling until content becomes visible.
                        session.record_steps(&pending_steps);
                        pending_steps.clear();
                        if reached_scroll_end {
                            break;
                        }
                        unrendered_scrolls += 1;
                        if unrendered_scrolls >= MAX_UNRENDERED_SCROLLS {
                            no_scroll_progress = true;
                            log(
                                "scroll shot: scrollbar moved but captured pixels did not; keeping the current viewport",
                            );
                            break;
                        }
                        continue;
                    } else {
                        if stationary_scrolls >= MAX_STATIONARY_SCROLLS {
                            if retry_opposite_direction(
                                screens,
                                &mut tried_reverse,
                                &mut scroll_delta,
                                &mut stationary_scrolls,
                                &mut pending_steps,
                                &mut stitch_upward,
                            ) {
                                log(
                                    "scroll shot: accessibility position did not move; trying reverse direction",
                                );
                                continue;
                            }
                            log(&format!(
                                "scroll shot: scrollbar and pixels unchanged after trying both directions ({stationary_scrolls} stationary events)"
                            ));
                            break;
                        }
                        continue;
                    }
                }
                if !frame.is_nearly_identical(&previous) {
                    stationary_scrolls = 0;
                    unrendered_scrolls = 0;
                }
                // Check for a stationary footer only after confirming that
                // the page actually moved. Otherwise a failed wheel event
                // could make the whole viewport look like a fixed overlay.
                if !fixed_ui_checked {
                    fixed_ui_checked = true;
                    if let Some(fixed_ui_top) = fixed_bottom_ui_top(&previous, &frame) {
                        capture_height = fixed_ui_top;
                        stitched = stitched.crop_to_height(capture_height)?;
                        previous = previous.crop_to_height(capture_height)?;
                        frame = frame.crop_to_height(capture_height)?;
                        log(&format!(
                            "scroll shot: excluded fixed bottom UI from row {fixed_ui_top}/{}",
                            initial.height
                        ));
                    }
                } else if frame.height > capture_height {
                    frame = frame.crop_to_height(capture_height)?;
                }
                session.record_steps(&pending_steps);
                pending_steps.clear();
                let overlap = match find_frame_overlap(&previous, &frame, last_shift, stitch_upward)
                {
                    Ok(overlap) => overlap,
                    Err(_) => {
                        // Smooth scrolling or lazy rendering may still be
                        // settling. Retry both frame orders on a fresh capture.
                        thread::sleep(Duration::from_millis(550));
                        frame = capture_frame(&filter, &config)?
                            .crop_to_height_at_most(capture_height)?;
                        match find_frame_overlap(&previous, &frame, last_shift, stitch_upward) {
                            Ok(overlap) => overlap,
                            Err(error) => {
                                log(&format!(
                                    "scroll shot: stopping after overlap retry; preserving {screens} stitched screens: {error}"
                                ));
                                partial_stitch_warning =
                                    Some(format!("拼接暂停，已保留 {screens} 屏"));
                                break;
                            }
                        }
                    }
                };
                let (overlap, detected_upward) = overlap;
                stitch_upward = detected_upward;
                let before = stitched.height;
                let stitched_progress = if stitch_upward {
                    stitched.prepend_with_overlap(&frame, overlap)?
                } else {
                    stitched.append_with_overlap(&frame, overlap)?
                };
                if !stitched_progress {
                    partial_stitch_warning =
                        Some("长图已达到尺寸上限，保留已采集内容供裁剪".to_string());
                    break;
                }
                screens += 1;
                scrollpreview::publish(
                    stitched.width,
                    stitched.height,
                    &stitched.rgba,
                    frame.height,
                    screens,
                );
                last_shift = Some(stitched.height - before);
                previous = frame;
                push_action(Action::ScrollShotProgress(format!(
                    "滚动截屏中：已拼接 {} 屏，可随时完成或取消",
                    screens
                )));
                if finish_requested {
                    break;
                }
                if reached_scroll_end {
                    log("scroll shot: accessibility scrollbar reached an edge");
                    break;
                }
            }

            if partial_stitch_warning.is_none()
                && (scrolls >= MAX_SCROLLS || stitched.height >= MAX_IMAGE_HEIGHT)
            {
                partial_stitch_warning =
                    Some("长图已达到采集上限，保留已采集内容供裁剪".to_string());
                log(&format!(
                    "scroll shot: capture limit reached (scrolls={scrolls}, height={})",
                    stitched.height
                ));
            }

            Ok(())
        })();

        let disposition = capture_disposition(
            &inner_result,
            CONTROL.load(Ordering::Acquire) == CONTROL_CANCEL,
            screens,
        );
        let cancelled = disposition == CaptureDisposition::Cancelled;
        match disposition {
            CaptureDisposition::Complete | CaptureDisposition::Cancelled => {}
            CaptureDisposition::PreserveForCrop(ref warning) => {
                // A failed wheel/capture/overlap step must not throw away the
                // already captured page. Keep the last valid stitched pixels,
                // attempt to restore the original scroll position, and let the
                // user trim and save what was captured.
                let pending_moved = pending_scroll_moved(
                    session.has_scrollbar(),
                    last_value,
                    session.value(),
                    if session.has_scrollbar() {
                        None
                    } else {
                        capture_frame(&filter, &config)
                            .ok()
                            .map(|frame| !frame.is_nearly_identical(&previous))
                    },
                );
                if pending_moved {
                    session.record_steps(&pending_steps);
                }
                partial_stitch_warning = Some(warning.clone());
            }
        }

        if cancelled && !pending_steps.is_empty() {
            let visual_moved = if session.has_scrollbar() {
                None
            } else {
                capture_frame(&filter, &config)
                    .ok()
                    .map(|frame| !frame.is_nearly_identical(&previous))
            };
            if pending_scroll_moved(
                session.has_scrollbar(),
                last_value,
                session.value(),
                visual_moved,
            ) {
                session.record_steps(&pending_steps);
            }
        }

        if !cancelled {
            let stitched_height = stitched.height;
            let completion = publish_disposition_for_crop(
                &disposition,
                stitched,
                capture_height,
                user_finished,
                partial_stitch_warning.is_some(),
                scale,
                dir.clone(),
            )?
            .ok_or_else(|| "滚动截图结果未进入裁剪预览".to_string())?;
            match completion {
                StitchCompletion::NoScroll => {
                    no_scroll_progress = true;
                    log(&format!(
                        "scroll shot: no stitched progress (stitched={stitched_height} viewport={capture_height} original={height}); preserving the viewport for crop"
                    ));
                    push_action(Action::ScrollShotProgress(
                        "页面没有继续滚动，已保留当前画面供裁剪".to_string(),
                    ));
                }
                StitchCompletion::Complete | StitchCompletion::Partial => {}
            }
        }

        // A finished long shot stays where it ended; only cancel/failure
        // rolls the page back to the original position.
        let restore_result = if cancelled || no_scroll_progress || partial_stitch_warning.is_some() {
            session.restore(|| capture_frame(&filter, &config), &initial)
        } else {
            session.skip_restore();
            Ok(())
        };
        if cancelled {
            return match restore_result {
                Ok(()) => Err("__cancelled__".to_string()),
                Err(error) => Err(format!("__cancelled__；{error}")),
            };
        }
        if let Err(error) = restore_result {
            let warning = match partial_stitch_warning {
                Some(warning) => format!("{warning}；原位置恢复未确认"),
                None => error,
            };
            log(&format!("scroll shot: crop result restore warning: {warning}"));
            return Ok(((), Some(warning)));
        }
        Ok(((), partial_stitch_warning))
    })()
}

pub fn save_crop_png(
    width: usize,
    height: usize,
    rgba: &[u8],
    dir: &std::path::Path,
) -> Result<(PathBuf, u64), String> {
    if width < 2 || height < 2 || rgba.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err("裁剪范围无效".to_string());
    }
    let data = unsafe { CFData::new(None, rgba.as_ptr(), rgba.len() as isize) }
        .ok_or_else(|| "无法建立长图像素数据".to_string())?;
    let provider = CGDataProvider::with_cf_data(Some(&data))
        .ok_or_else(|| "无法建立长图数据源".to_string())?;
    let color_space =
        CGColorSpace::new_device_rgb().ok_or_else(|| "无法建立 RGB 色彩空间".to_string())?;
    let bitmap_info = CGBitmapInfo::from_bits_retain(
        CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
    );
    let image = unsafe {
        CGImage::new(
            width,
            height,
            8,
            32,
            width * 4,
            Some(&color_space),
            bitmap_info,
            Some(&provider),
            ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
    .ok_or_else(|| "无法生成裁剪长图".to_string())?;
    crate::shot::save_png(&image, dir)
}

fn wait_for_target_frontmost(target_pid: libc::pid_t) -> Result<(), String> {
    for _ in 0..40 {
        if NSRunningApplication::runningApplicationWithProcessIdentifier(target_pid).is_none() {
            return Err("目标应用已退出，请重新开始滚动截屏".to_string());
        }
        let frontmost = NSWorkspace::sharedWorkspace().frontmostApplication();
        if frontmost.is_some_and(|app| app.processIdentifier() == target_pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("无法把目标应用切到前台；请先打开要截取的窗口再重试".to_string())
}

struct CursorGuard {
    previous: CGPoint,
}

impl CursorGuard {
    fn park(point: CGPoint) -> Self {
        let previous = cursor_position().unwrap_or(point);
        CGWarpMouseCursorPosition(point);
        Self { previous }
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        CGWarpMouseCursorPosition(self.previous);
    }
}

fn ensure_target_frontmost(target_pid: libc::pid_t) -> Result<(), String> {
    let frontmost = NSWorkspace::sharedWorkspace().frontmostApplication();
    if frontmost.is_some_and(|app| app.processIdentifier() == target_pid) {
        Ok(())
    } else {
        Err("目标应用在截屏过程中失去前台，已停止；请重新开始".to_string())
    }
}

enum PauseResult {
    Continue,
    Finish,
    Cancel,
}

fn cursor_in_selection(rect: Rect, display_h: f64) -> bool {
    let Some(point) = cursor_position() else {
        return true;
    };
    let left = rect.x;
    let right = rect.x + rect.w;
    let top = display_h - rect.y - rect.h;
    let bottom = display_h - rect.y;
    point.x >= left && point.x <= right && point.y >= top && point.y <= bottom
}

fn wait_for_cursor(rect: Rect, display_h: f64) -> PauseResult {
    if cursor_in_selection(rect, display_h) {
        return PauseResult::Continue;
    }
    push_action(Action::ScrollShotProgress(
        "已暂停：把鼠标移回选区继续，或点预览窗的「完成」「取消」".to_string(),
    ));
    loop {
        match CONTROL.load(Ordering::Acquire) {
            CONTROL_CANCEL => return PauseResult::Cancel,
            CONTROL_FINISH => return PauseResult::Finish,
            _ => {}
        }
        if cursor_in_selection(rect, display_h) {
            push_action(Action::ScrollShotProgress(
                "继续滚动截屏… 可随时点「完成」或「取消」".to_string(),
            ));
            return PauseResult::Continue;
        }
        thread::sleep(Duration::from_millis(80));
    }
}

enum ScrollTarget {
    Scrollbar {
        element: CFRetained<CFType>,
        original: CFRetained<CFType>,
    },
    Visual,
}

struct ScrollSession {
    target_pid: libc::pid_t,
    center: CGPoint,
    target: ScrollTarget,
    steps: Vec<i32>,
    restored: bool,
}

impl ScrollSession {
    fn new(target_pid: libc::pid_t, center: CGPoint, target: ScrollTarget) -> Self {
        Self {
            target_pid,
            center,
            target,
            steps: Vec::new(),
            restored: false,
        }
    }

    fn has_scrollbar(&self) -> bool {
        matches!(self.target, ScrollTarget::Scrollbar { .. })
    }

    fn value(&self) -> Option<f64> {
        let ScrollTarget::Scrollbar { element, .. } = &self.target else {
            return None;
        };
        copy_attribute(element, "AXValue").and_then(|value| number_value(&value))
    }

    fn reached_scroll_end(&self, before: f64, after: f64) -> bool {
        let ScrollTarget::Scrollbar { element, .. } = &self.target else {
            return false;
        };
        let min = copy_attribute(element, "AXMinValue")
            .and_then(|value| number_value(&value))
            .unwrap_or(0.0);
        let max = copy_attribute(element, "AXMaxValue")
            .and_then(|value| number_value(&value))
            .unwrap_or(1.0);
        scroll_reached_end(before, after, min, max)
    }

    fn record_steps(&mut self, steps: &[i32]) {
        self.steps.extend_from_slice(steps);
    }

    fn restore_steps(&self) -> impl Iterator<Item = i32> + '_ {
        self.steps.iter().rev().map(|delta| -*delta)
    }

    fn restore(
        &mut self,
        capture: impl FnMut() -> Result<PixelFrame, String>,
        initial: &PixelFrame,
    ) -> Result<(), String> {
        if self.restored {
            return Ok(());
        }
        // Never replay twice: a second reverse pass would undo the restore.
        self.restored = true;
        if self.steps.is_empty() {
            return Ok(());
        }

        let restored_scrollbar = match &self.target {
            ScrollTarget::Scrollbar { element, original } => {
                self.restore_scrollbar(element, original)
            }
            ScrollTarget::Visual => false,
        };
        if restored_scrollbar {
            return Ok(());
        }
        self.replay_back(capture, initial)
    }

    fn skip_restore(&mut self) {
        self.restored = true;
    }

    fn restore_scrollbar(
        &self,
        element: &CFRetained<CFType>,
        original: &CFRetained<CFType>,
    ) -> bool {
        let Some(original_value) = number_value(original) else {
            return false;
        };
        let attribute = CFString::from_str("AXValue");
        let status = unsafe {
            AXUIElementSetAttributeValue(
                std::ptr::from_ref(&**element),
                std::ptr::from_ref(&*attribute),
                std::ptr::from_ref(&**original),
            )
        };
        if status != 0 {
            log(&format!(
                "scroll shot: AX restore failed ({status}); replaying wheel steps"
            ));
            return false;
        }
        // Browsers may animate the jump back to the original offset, so give
        // the accessibility value a moment to settle before verifying it.
        let mut actual_value = None;
        for _ in 0..20 {
            actual_value =
                copy_attribute(element, "AXValue").and_then(|value| number_value(&value));
            if actual_value.is_some_and(|value| (value - original_value).abs() <= 0.001) {
                log(&format!(
                    "scroll shot: restored target pid {} to {original_value:.3}",
                    self.target_pid
                ));
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        log(&format!(
            "scroll shot: AX restore verification failed (expected {original_value:.3}, got {})",
            actual_value.map_or_else(|| "none".to_string(), |value| format!("{value:.3}"))
        ));
        false
    }

    fn replay_back(
        &mut self,
        mut capture: impl FnMut() -> Result<PixelFrame, String>,
        initial: &PixelFrame,
    ) -> Result<(), String> {
        if self.steps.is_empty() {
            return Err("画面没有发生滚动，无法恢复原位置".to_string());
        }
        if NSRunningApplication::runningApplicationWithProcessIdentifier(self.target_pid).is_none()
        {
            return Err("目标应用已退出，无法恢复原滚动位置".to_string());
        }
        ensure_target_frontmost(self.target_pid)
            .map_err(|_| "目标应用不在前台，未执行滚动位置回放".to_string())?;
        let _guard = CursorGuard::park(self.center);
        for delta in self.restore_steps() {
            post_scroll_at(self.center, delta)?;
            thread::sleep(Duration::from_millis(180));
        }
        thread::sleep(Duration::from_millis(500));
        let current = capture()?;
        if current.is_nearly_identical(initial) {
            log(&format!(
                "scroll shot: replayed {} steps for pid {}",
                self.steps.len(),
                self.target_pid
            ));
            Ok(())
        } else {
            Err("已反向滚动，但画面与开始时不一致".to_string())
        }
    }
}

impl Drop for ScrollSession {
    fn drop(&mut self) {
        if !self.restored && !self.steps.is_empty() {
            log(&format!(
                "scroll shot: dropped without restoring {} steps for pid {}",
                self.steps.len(),
                self.target_pid
            ));
        }
    }
}

fn find_scroll_target(pid: libc::pid_t, point: CGPoint) -> Result<ScrollTarget, String> {
    let root = retained_ax(unsafe { AXUIElementCreateApplication(pid) });
    if root.is_none() {
        log("scroll shot: AX application unavailable; using visual scrolling");
        return Ok(ScrollTarget::Visual);
    }
    let mut element_ptr = ptr::null();
    let Some(system) = retained_ax(unsafe { AXUIElementCreateSystemWide() }) else {
        log("scroll shot: AX system element unavailable; using visual scrolling");
        return Ok(ScrollTarget::Visual);
    };
    let status = unsafe {
        AXUIElementCopyElementAtPosition(
            std::ptr::from_ref(&*system),
            point.x as f32,
            point.y as f32,
            &mut element_ptr,
        )
    };
    if status != 0 {
        // WeChat and some custom-drawn apps return kAXErrorNotImplemented
        // (-25208) for hit testing. Wheel events and frame matching still
        // work, so do not fail the whole capture.
        log(&format!(
            "scroll shot: AX hit test failed ({status}); using visual scrolling"
        ));
        return Ok(ScrollTarget::Visual);
    }
    let Some(mut current) = retained_ax(element_ptr) else {
        log("scroll shot: AX hit test returned no element; using visual scrolling");
        return Ok(ScrollTarget::Visual);
    };
    let mut hit_pid = 0;
    let pid_status = unsafe { AXUIElementGetPid(std::ptr::from_ref(&*current), &mut hit_pid) };
    if pid_status != 0 || hit_pid != pid {
        return Err("选区位置与记录的目标应用不匹配，请重新选区".to_string());
    }
    for _ in 0..32 {
        let candidate = vertical_scrollbar_candidate(&current).and_then(|scrollbar| {
            copy_attribute(&scrollbar, "AXValue").map(|original| (scrollbar, original))
        });
        if let Some((element, original)) = candidate {
            return Ok(ScrollTarget::Scrollbar { element, original });
        }
        let Some(parent) = copy_attribute(&current, "AXParent") else {
            break;
        };
        current = parent;
    }
    // Chrome exposes a scroll area but not an AX scrollbar. Frame matching
    // still detects the end, and wheel deltas can be replayed in reverse.
    Ok(ScrollTarget::Visual)
}

/// Browsers differ in how they expose scrollers: some set
/// `AXVerticalScrollBar` on the scroll area, others only list the scrollbar
/// as a child element.
fn vertical_scrollbar_candidate(element: &CFType) -> Option<CFRetained<CFType>> {
    let scrollbar = copy_attribute(element, "AXVerticalScrollBar")
        .filter(|scrollbar| usable_scrollbar(scrollbar));
    if let Some(scrollbar) = scrollbar {
        return Some(scrollbar);
    }
    let children = copy_attribute(element, "AXChildren")?;
    let children = children.downcast_ref::<CFArray>()?;
    let children: &CFArray<CFType> = unsafe { children.cast_unchecked::<CFType>() };
    for index in 0..children.len() {
        let child = children.get(index)?;
        let role = copy_attribute(&child, "AXRole")
            .and_then(|role| role.downcast_ref::<CFString>().map(|role| role.to_string()));
        if role.as_deref() != Some("AXScrollBar") {
            continue;
        }
        let orientation = copy_attribute(&child, "AXOrientation").and_then(|value| {
            value
                .downcast_ref::<CFString>()
                .map(|value| value.to_string())
        });
        if orientation.as_deref() != Some("AXHorizontalOrientation") && usable_scrollbar(&child) {
            return Some(child);
        }
    }
    None
}

fn usable_scrollbar(scrollbar: &CFType) -> bool {
    let Some(value) = copy_attribute(scrollbar, "AXValue").and_then(|value| number_value(&value))
    else {
        return false;
    };
    let min = copy_attribute(scrollbar, "AXMinValue")
        .and_then(|value| number_value(&value))
        .unwrap_or(0.0);
    let max = copy_attribute(scrollbar, "AXMaxValue")
        .and_then(|value| number_value(&value))
        .unwrap_or(1.0);
    max > min && value >= min - 0.001 && value <= max + 0.001
}

fn scroll_reached_end(before: f64, after: f64, min: f64, max: f64) -> bool {
    let epsilon = ((max - min).abs() * 0.000_01).max(0.000_01);
    let moved = (after - before).abs() > epsilon;
    moved
        && ((after <= min + epsilon && before > min + epsilon)
            || (after >= max - epsilon && before < max - epsilon))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StitchCompletion {
    Complete,
    NoScroll,
    Partial,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CaptureDisposition {
    Complete,
    Cancelled,
    PreserveForCrop(String),
}

fn capture_disposition(
    result: &Result<(), String>,
    cancel_requested: bool,
    screens: usize,
) -> CaptureDisposition {
    match result {
        Ok(()) => CaptureDisposition::Complete,
        Err(error) if cancel_requested || error == "__cancelled__" => {
            CaptureDisposition::Cancelled
        }
        Err(error) => {
            log(&format!(
                "scroll shot: preserving {screens} stitched screens after capture interruption: {error}"
            ));
            CaptureDisposition::PreserveForCrop(format!(
                "拼接已暂停，保留已采集的 {screens} 屏供裁剪"
            ))
        }
    }
}

fn classify_stitch_completion(
    stitched_height: usize,
    viewport_height: usize,
    user_finished: bool,
    partial_stitch: bool,
) -> StitchCompletion {
    if partial_stitch {
        StitchCompletion::Partial
    } else if stitched_height <= viewport_height && !user_finished {
        StitchCompletion::NoScroll
    } else {
        StitchCompletion::Complete
    }
}

fn publish_stitched_for_crop(
    stitched: PixelFrame,
    viewport_height: usize,
    user_finished: bool,
    partial_stitch: bool,
    scale: f64,
    output_dir: PathBuf,
) -> Result<StitchCompletion, String> {
    let completion = classify_stitch_completion(
        stitched.height,
        viewport_height,
        user_finished,
        partial_stitch,
    );
    scrollpreview::publish_crop_image(
        stitched.width,
        stitched.height,
        scale,
        stitched.rgba,
        output_dir,
    )?;
    Ok(completion)
}

fn publish_disposition_for_crop(
    disposition: &CaptureDisposition,
    stitched: PixelFrame,
    viewport_height: usize,
    user_finished: bool,
    partial_stitch: bool,
    scale: f64,
    output_dir: PathBuf,
) -> Result<Option<StitchCompletion>, String> {
    if *disposition == CaptureDisposition::Cancelled {
        return Ok(None);
    }
    publish_stitched_for_crop(
        stitched,
        viewport_height,
        user_finished,
        partial_stitch || matches!(disposition, CaptureDisposition::PreserveForCrop(_)),
        scale,
        output_dir,
    )
    .map(Some)
}

fn retry_opposite_direction(
    screens: usize,
    tried_reverse: &mut bool,
    scroll_delta: &mut i32,
    stationary_scrolls: &mut usize,
    pending_steps: &mut Vec<i32>,
    stitch_upward: &mut bool,
) -> bool {
    if screens != 1 || *tried_reverse {
        return false;
    }
    *scroll_delta = -*scroll_delta;
    *stitch_upward = *scroll_delta > 0;
    *tried_reverse = true;
    *stationary_scrolls = 0;
    // Neither visual pixels nor the scrollbar confirmed movement in the
    // failed direction, so do not replay those unconfirmed events on restore.
    pending_steps.clear();
    true
}

fn pending_scroll_moved(
    has_scrollbar: bool,
    before: Option<f64>,
    after: Option<f64>,
    visual_moved: Option<bool>,
) -> bool {
    if has_scrollbar {
        matches!((before, after), (Some(before), Some(after)) if (after - before).abs() > 0.000_01)
    } else {
        visual_moved.unwrap_or(false)
    }
}

fn retained_ax(pointer: *const CFType) -> Option<CFRetained<CFType>> {
    NonNull::new(pointer.cast_mut()).map(|pointer| unsafe { CFRetained::from_raw(pointer) })
}

fn copy_attribute(element: &CFType, name: &str) -> Option<CFRetained<CFType>> {
    let attribute = CFString::from_str(name);
    let mut value = ptr::null();
    let status = unsafe {
        AXUIElementCopyAttributeValue(element, std::ptr::from_ref(&*attribute), &mut value)
    };
    if status != 0 {
        return None;
    }
    retained_ax(value)
}

fn number_value(value: &CFType) -> Option<f64> {
    value.downcast_ref::<CFNumber>()?.as_f64()
}

fn cursor_position() -> Option<CGPoint> {
    CGEvent::new(None).map(|event| CGEvent::location(Some(&event)))
}

fn post_scroll_at(point: CGPoint, delta: i32) -> Result<(), String> {
    let event = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Line, 1, delta, 0, 0)
        .ok_or_else(|| "无法建立滚动事件".to_string())?;
    CGEvent::set_location(Some(&event), point);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    Ok(())
}

fn capture_frame(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
) -> Result<PixelFrame, String> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let result = if image.is_null() {
            Err(if error.is_null() {
                "屏幕截图失败".to_string()
            } else {
                unsafe { Retained::retain(error) }
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "屏幕截图失败".to_string())
            })
        } else {
            unsafe { Retained::retain(image) }
                .ok_or_else(|| "屏幕截图返回空图像".to_string())
                .and_then(|image| PixelFrame::from_image(&image))
        };
        let _ = tx.send(result);
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            config,
            Some(&block),
        )
    };
    rx.recv_timeout(Duration::from_secs(12))
        .map_err(|_| "等待屏幕截图超时".to_string())?
}

#[derive(Clone)]
struct PixelFrame {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

impl PixelFrame {
    fn from_image(image: &CGImage) -> Result<Self, String> {
        let width = CGImage::width(Some(image));
        let height = CGImage::height(Some(image));
        let row_bytes = CGImage::bytes_per_row(Some(image));
        if width < 2
            || height < 2
            || width.saturating_mul(height) > MAX_CAPTURE_PIXELS
            || CGImage::bits_per_pixel(Some(image)) != 32
        {
            return Err("截图像素格式不受支持，无法可靠拼接".to_string());
        }
        let required = row_bytes
            .checked_mul(height)
            .ok_or_else(|| "截图缓冲区尺寸无效".to_string())?;
        let provider =
            CGImage::data_provider(Some(image)).ok_or_else(|| "截图缺少像素数据".to_string())?;
        let data =
            CGDataProvider::data(Some(&provider)).ok_or_else(|| "无法读取截图像素".to_string())?;
        if data.length() < required as isize || row_bytes < width * 4 {
            return Err("截图像素数据不完整".to_string());
        }
        let alpha = CGImage::bitmap_info(Some(image)).bits() & CGBitmapInfo::AlphaInfoMask.bits();
        let byte_order =
            CGImage::bitmap_info(Some(image)).bits() & CGBitmapInfo::ByteOrderInfoMask.bits();
        let little_endian = byte_order == CGImageByteOrderInfo::Order32Little.0;
        let alpha_first = matches!(alpha, 1 | 2 | 4 | 6);
        let has_alpha = matches!(alpha, 1 | 2 | 3 | 4 | 7);
        if !(1..=6).contains(&alpha) {
            return Err("截图透明通道格式不受支持，无法可靠拼接".to_string());
        }
        let (r, g, b, a) = match (little_endian, alpha_first) {
            (false, true) => (1, 2, 3, 0),
            (false, false) => (0, 1, 2, 3),
            (true, true) => (2, 1, 0, 3),
            (true, false) => (3, 2, 1, 0),
        };
        let src = unsafe { std::slice::from_raw_parts(data.byte_ptr(), required) };
        let mut rgba = vec![0; width * height * 4];
        for y in 0..height {
            let source_row = &src[y * row_bytes..y * row_bytes + width * 4];
            let target_row = &mut rgba[y * width * 4..(y + 1) * width * 4];
            for (src_pixel, dst_pixel) in source_row
                .chunks_exact(4)
                .zip(target_row.chunks_exact_mut(4))
            {
                dst_pixel[0] = src_pixel[r];
                dst_pixel[1] = src_pixel[g];
                dst_pixel[2] = src_pixel[b];
                dst_pixel[3] = if has_alpha { src_pixel[a] } else { 255 };
            }
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    fn is_nearly_identical(&self, other: &Self) -> bool {
        if self.width != other.width || self.height != other.height {
            return false;
        }
        // Sparse 64×64 sampling misses text in mostly white pages: a small
        // scroll can move every text line while all sample points stay white.
        // Denser sampling makes motion detection reliable without comparing
        // every pixel in a large Retina capture.
        let sx = (self.width / 256).max(1);
        let sy = (self.height / 256).max(1);
        let mut changed = 0usize;
        let mut samples = 0u64;
        for y in (0..self.height).step_by(sy) {
            for x in (0..self.width).step_by(sx) {
                let offset = (y * self.width + x) * 4;
                let difference = self.rgba[offset].abs_diff(other.rgba[offset]) as u16
                    + self.rgba[offset + 1].abs_diff(other.rgba[offset + 1]) as u16
                    + self.rgba[offset + 2].abs_diff(other.rgba[offset + 2]) as u16;
                if difference >= 36 {
                    changed += 1;
                }
                samples += 1;
            }
        }
        // Sparse text on a white page can change far fewer than 0.1% of
        // sampled points between frames. Keep the noise floor low enough to
        // detect that movement while ignoring a handful of blinking pixels.
        samples > 0 && changed < (samples as usize / 10_000).max(4)
    }

    fn append_below(&mut self, next: &Self, overlap: usize) -> Result<(), String> {
        if self.width != next.width || overlap >= next.height {
            return Err("截图尺寸变化，无法可靠拼接".to_string());
        }
        self.rgba
            .extend_from_slice(&next.rgba[overlap * self.width * 4..]);
        self.height += next.height - overlap;
        Ok(())
    }

    fn crop_to_height(&self, height: usize) -> Result<Self, String> {
        if height < 2 || height > self.height {
            return Err("滚动截图裁剪高度无效".to_string());
        }
        Ok(Self {
            width: self.width,
            height,
            rgba: self.rgba[..height * self.width * 4].to_vec(),
        })
    }

    fn crop_to_height_at_most(self, height: usize) -> Result<Self, String> {
        if height < 2 || self.height < height {
            return Err("截图尺寸变化，无法保持滚动截图区域一致".to_string());
        }
        if self.height == height {
            Ok(self)
        } else {
            self.crop_to_height(height)
        }
    }

    /// Stitch `next` below the current image. Returns `false` when the
    /// configured size limits would be exceeded.
    #[cfg(test)]
    fn append_next(
        &mut self,
        previous: &Self,
        next: &Self,
        hint: Option<usize>,
    ) -> Result<bool, String> {
        let overlap = find_overlap_with_hint(previous, next, hint)?;
        self.append_with_overlap(next, overlap)
    }

    fn append_with_overlap(&mut self, next: &Self, overlap: usize) -> Result<bool, String> {
        if self.width != next.width || overlap >= next.height {
            return Err("截图尺寸变化，无法可靠拼接".to_string());
        }
        let added = next.height.saturating_sub(overlap);
        if added == 0 {
            return Err("页面没有继续向下滚动，请检查选区或滚动区域".to_string());
        }
        let new_height = self
            .height
            .checked_add(added)
            .ok_or_else(|| "长图高度超出限制".to_string())?;
        let pixels = self
            .width
            .checked_mul(new_height)
            .ok_or_else(|| "长图尺寸超出限制".to_string())?;
        if new_height > MAX_IMAGE_HEIGHT || pixels > MAX_IMAGE_PIXELS {
            return Ok(false);
        }
        self.append_below(next, overlap)?;
        Ok(true)
    }

    /// Stitch a newly captured frame above the current image. `overlap` is
    /// the shared bottom part of `next` and the top part of the current image.
    fn prepend_with_overlap(&mut self, next: &Self, overlap: usize) -> Result<bool, String> {
        if self.width != next.width || overlap >= next.height {
            return Err("截图尺寸变化，无法可靠拼接".to_string());
        }
        let added = next.height - overlap;
        let new_height = self
            .height
            .checked_add(added)
            .ok_or_else(|| "长图高度超出限制".to_string())?;
        let pixels = self
            .width
            .checked_mul(new_height)
            .ok_or_else(|| "长图尺寸超出限制".to_string())?;
        if new_height > MAX_IMAGE_HEIGHT || pixels > MAX_IMAGE_PIXELS {
            return Ok(false);
        }

        let prefix_bytes = added
            .checked_mul(self.width)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| "长图尺寸超出限制".to_string())?;
        let total_bytes = self
            .rgba
            .len()
            .checked_add(prefix_bytes)
            .ok_or_else(|| "长图尺寸超出限制".to_string())?;
        let mut prepended = Vec::with_capacity(total_bytes);
        prepended.extend_from_slice(&next.rgba[..prefix_bytes]);
        prepended.extend_from_slice(&self.rgba);
        self.rgba = prepended;
        self.height = new_height;
        Ok(true)
    }
}

/// Find a large, visually structured region near the bottom that remains at
/// the same screen coordinates while the document behind it scrolls. Fixed
/// composers and floating controls are excluded from every stitched frame so
/// they are not repeated once per scroll step.
fn fixed_bottom_ui_top(previous: &PixelFrame, next: &PixelFrame) -> Option<usize> {
    if previous.width != next.width
        || previous.height != next.height
        || previous.width < 64
        || previous.height < 240
    {
        return None;
    }
    let width = previous.width;
    let height = previous.height;
    let x_step = (width / 400).max(1);
    // Sample multiple overlapping horizontal bands. A composer may only span
    // the central half of a wide selection, while moving page content remains
    // visible on both sides of it.
    let band_start = width / 12;
    let band_end = width * 11 / 12;
    let band_width = ((band_end - band_start) * 3 / 5).max(48);
    let band_stride = ((band_end - band_start - band_width) / 4).max(1);
    let bands: Vec<Vec<usize>> = (0..=4)
        .map(|index| {
            let start = band_start + index * band_stride;
            (start..(start + band_width).min(band_end))
                .step_by(x_step)
                .collect::<Vec<usize>>()
        })
        .filter(|columns| columns.len() >= 24)
        .collect();
    if bands.is_empty() {
        return None;
    }

    let search_start = height * FIXED_UI_SEARCH_START / 100;
    let search_end = height * FIXED_UI_SEARCH_END / 100;
    let mut stable_rows = vec![vec![false; height]; bands.len()];
    let mut informative_rows = vec![vec![false; height]; bands.len()];
    for y in search_start..search_end {
        let row = y * width * 4;
        for (band_index, sample_columns) in bands.iter().enumerate() {
            let mut same = 0usize;
            let mut edges = 0usize;
            let mut previous_luma = None;
            let mut changed = 0usize;
            for &x in sample_columns {
                let offset = row + x * 4;
                let a = luminance(&previous.rgba, offset);
                let b = luminance(&next.rgba, offset);
                // Semi-transparent panels retain a faint trace of the page
                // underneath. Allow small blending deltas, but compare the
                // full row inside each horizontal band.
                same += usize::from(a.abs_diff(b) <= 32);
                changed += usize::from(a.abs_diff(b) > 32);
                if previous_luma.is_some_and(|value: u32| value.abs_diff(a) >= 18) {
                    edges += 1;
                }
                previous_luma = Some(a);
            }
            stable_rows[band_index][y] = same * 100 >= sample_columns.len() * 84;
            informative_rows[band_index][y] =
                edges >= (sample_columns.len() / 100).max(3) || changed == 0;
        }
    }

    let min_run = (height * FIXED_UI_MIN_RUN / 100).max(48);
    let min_informative = (height / 100).max(8);
    // A page may contain an unchanged card or sticky section above a bottom
    // composer. Keep scanning and choose the lowest stable informative run.
    for band_index in 0..bands.len() {
        let mut lowest_bottom_ui = None;
        let mut y = search_start;
        while y < search_end {
            if !stable_rows[band_index][y] {
                y += 1;
                continue;
            }
            let start = y;
            while y < search_end && stable_rows[band_index][y] {
                y += 1;
            }
            let end = y;
            if end - start < min_run || end * 100 < height * 82 {
                continue;
            }
            let informative = informative_rows[band_index][start..end]
                .iter()
                .filter(|row| **row)
                .count();
            if informative < min_informative {
                continue;
            }
            let top = start.saturating_sub((height / 80).max(12));
            if top >= height / 2 && top < height - 64 {
                lowest_bottom_ui = Some(top);
            }
        }
        if let Some(top) = lowest_bottom_ui {
            return Some(top);
        }
    }
    None
}

fn find_overlap(previous: &PixelFrame, next: &PixelFrame) -> Result<usize, String> {
    find_overlap_in_range(previous, next, None)
}

fn find_overlap_with_hint(
    previous: &PixelFrame,
    next: &PixelFrame,
    hint: Option<usize>,
) -> Result<usize, String> {
    if let Some(hint) = hint {
        let height = previous.height;
        let min_shift = (height / 100).max(2);
        let max_shift = (height * 92 / 100).max(min_shift + 1);
        let low = (hint / 2).max(min_shift);
        let high = (hint * 3 / 2).min(max_shift);
        let hinted = if high > low {
            find_overlap_in_range(previous, next, Some((low, high))).ok()
        } else {
            None
        };
        if let Some(overlap) = hinted {
            return Ok(overlap);
        }
    }
    find_overlap(previous, next)
}

/// Determine frame order from the image overlap. Wheel event signs vary
/// between apps and input sources, so the preferred direction is only a hint.
fn find_frame_overlap(
    previous: &PixelFrame,
    next: &PixelFrame,
    hint: Option<usize>,
    prefer_upward: bool,
) -> Result<(usize, bool), String> {
    let preferred = if prefer_upward {
        find_overlap_with_hint(next, previous, hint).map(|overlap| (overlap, true))
    } else {
        find_overlap_with_hint(previous, next, hint).map(|overlap| (overlap, false))
    };
    preferred.or_else(|first_error| {
        let opposite = if prefer_upward {
            find_overlap_with_hint(previous, next, hint).map(|overlap| (overlap, false))
        } else {
            find_overlap_with_hint(next, previous, hint).map(|overlap| (overlap, true))
        };
        opposite.map_err(|_| first_error)
    })
}

fn find_overlap_in_range(
    previous: &PixelFrame,
    next: &PixelFrame,
    range: Option<(usize, usize)>,
) -> Result<usize, String> {
    if previous.width != next.width || previous.height != next.height {
        return Err("截图尺寸变化，无法可靠拼接".to_string());
    }
    let height = previous.height;
    let min_shift = (height / 100).max(2);
    let max_shift = (height * 92 / 100).max(min_shift + 1);
    let (min_shift, max_shift) = match range {
        Some((low, high)) => (
            low.clamp(min_shift, max_shift),
            high.clamp(min_shift, max_shift),
        ),
        None => (min_shift, max_shift),
    };
    let Some((sample_columns, active_rows)) = overlap_sample_points(previous, next) else {
        return Err("画面没有可用的滚动内容，无法拼接".to_string());
    };
    let mut best = (usize::MAX, 0usize);
    let mut best_bad = 1.0f64;
    let mut second = usize::MAX;
    // A two-pixel stride can miss the true offset by one pixel and make two
    // neighboring shifts look equally plausible. Check every row offset.
    let mut scores = Vec::with_capacity(max_shift - min_shift + 1);
    for shift in min_shift..=max_shift {
        let overlap = height - shift;
        let mut score = 0u64;
        let mut bad = 0u64;
        let mut samples = 0u64;
        for &y in &active_rows {
            if y >= overlap {
                break;
            }
            let prev_row = (y + shift) * previous.width * 4;
            let next_row = y * next.width * 4;
            for &x in &sample_columns {
                let a = prev_row + x * 4;
                let b = next_row + x * 4;
                let lum_a = (previous.rgba[a] as u32 * 3
                    + previous.rgba[a + 1] as u32 * 6
                    + previous.rgba[a + 2] as u32)
                    / 10;
                let lum_b = (next.rgba[b] as u32 * 3
                    + next.rgba[b + 1] as u32 * 6
                    + next.rgba[b + 2] as u32)
                    / 10;
                let difference = lum_a.abs_diff(lum_b);
                score += difference as u64;
                if difference > 14 {
                    bad += 1;
                }
                samples += 1;
            }
        }
        if samples == 0 {
            scores.push((shift, usize::MAX, 1.0));
            continue;
        }
        let average = (score / samples) as usize;
        let bad_ratio = bad as f64 / samples as f64;
        scores.push((shift, average, bad_ratio));
        if average < best.0 {
            second = best.0;
            best_bad = bad_ratio;
            best = (average, shift);
        } else if shift.abs_diff(best.1) > height / 40 {
            second = second.min(average);
        }
    }
    // A fixed animation can corrupt part of the true match. A valid offset
    // forms a low-error basin around neighboring shifts; isolated ties caused
    // by fixed UI and blank regions do not.
    let ambiguous = overlap_is_ambiguous(&scores, best.1, height);
    if best.1 == 0 || best.0 > 32 || best_bad > 0.30 || ambiguous {
        log(&format!(
            "scroll shot: overlap failed best={} shift={} bad={:.2} second={} cols={} rows={}",
            best.0,
            best.1,
            best_bad,
            second,
            sample_columns.len(),
            active_rows.len()
        ));
        return Err("画面重叠无法可靠识别，已停止拼接；请框选稳定的页面内容后重试".to_string());
    }
    // `shift` is the scroll distance; the number of rows shared by both
    // frames is the viewport height minus that distance.
    Ok(height - best.1)
}

fn overlap_is_ambiguous(scores: &[(usize, usize, f64)], best_shift: usize, height: usize) -> bool {
    let Some((_, best_score, _)) = scores.iter().find(|(shift, _, _)| *shift == best_shift) else {
        return true;
    };
    if *best_score == usize::MAX {
        return true;
    }
    let tolerance = best_score.saturating_add((best_score / 4).max(2));
    let distinct_distance = (height / 12).max(6);
    let competing: Vec<usize> = scores
        .iter()
        .filter_map(|(shift, score, bad_ratio)| {
            (*score <= tolerance && *bad_ratio <= 0.30).then_some(*shift)
        })
        .collect();
    competing.iter().any(|shift| {
        shift.abs_diff(best_shift) >= distinct_distance
            && (shift.abs_diff(best_shift) > 1 || competing.len() > 1)
    })
}

fn overlap_sample_points(
    previous: &PixelFrame,
    next: &PixelFrame,
) -> Option<(Vec<usize>, Vec<usize>)> {
    let width = previous.width;
    let height = previous.height;
    if width < 2 || height < 2 || next.width != width || next.height != height {
        return None;
    }
    let step_x = (width / 300).max(1);
    let mut column_activity = vec![0u64; width];
    let mut row_activity = vec![0u64; height];
    for (y, row) in row_activity.iter_mut().enumerate() {
        for x in (0..width).step_by(step_x) {
            let offset = (y * width + x) * 4;
            let a = luminance(&previous.rgba, offset);
            let b = luminance(&next.rgba, offset);
            let difference = a.abs_diff(b) as u64;
            column_activity[x] += difference;
            *row += difference;
        }
    }

    let mut columns = active_indices(&column_activity, width);
    let mut rows = active_indices(&row_activity, height);
    // Avoid overfitting the match to a blinking caret, spinner or cursor.
    // Keep deterministic, evenly distributed samples across the content.
    columns = evenly_spaced(columns, OVERLAP_SAMPLE_COLUMNS);
    rows = evenly_spaced(rows, OVERLAP_SAMPLE_ROWS);
    (!columns.is_empty() && !rows.is_empty()).then_some((columns, rows))
}

fn luminance(rgba: &[u8], offset: usize) -> u32 {
    (rgba[offset] as u32 * 3 + rgba[offset + 1] as u32 * 6 + rgba[offset + 2] as u32) / 10
}

fn active_indices(activity: &[u64], total: usize) -> Vec<usize> {
    let max = activity.iter().copied().max().unwrap_or(0);
    if max < 8 {
        return (0..total).collect();
    }
    let threshold = (max / 12).max(2);
    let active: Vec<usize> = activity
        .iter()
        .enumerate()
        .filter_map(|(index, value)| (*value >= threshold).then_some(index))
        .collect();
    if active.len() < total / 8 {
        (0..total).collect()
    } else {
        active
    }
}

fn evenly_spaced(values: Vec<usize>, max_samples: usize) -> Vec<usize> {
    if values.len() <= max_samples {
        return values;
    }
    (0..max_samples)
        .map(|index| values[index * (values.len() - 1) / (max_samples - 1).max(1)])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        CaptureDisposition, PixelFrame, capture_disposition, find_frame_overlap, find_overlap,
        fixed_bottom_ui_top,
    };

    fn patterned_frame(width: usize, height: usize, first_row: usize) -> PixelFrame {
        let mut rgba = vec![0; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let row = first_row + y;
                let mut value = (x as u64 + 0x9e37_79b9) ^ (row as u64 + 0x85eb_ca6b);
                value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                let value = (value ^ (value >> 31)) as u8;
                let offset = (y * width + x) * 4;
                rgba[offset..offset + 4].copy_from_slice(&[value, value, value, 255]);
            }
        }
        PixelFrame {
            width,
            height,
            rgba,
        }
    }

    #[test]
    fn finds_vertical_offset_between_adjacent_frames() {
        let width = 96;
        let height = 120;
        let shift = 24;
        let previous = patterned_frame(width, height, 0);
        let next = patterned_frame(width, height, shift);
        assert_eq!(find_overlap(&previous, &next).unwrap(), height - shift);
    }

    #[test]
    fn stitches_frames_back_into_full_page() {
        let width = 96;
        let height = 120;
        let shift = 24;
        let first = patterned_frame(width, height, 0);
        let second = patterned_frame(width, height, shift);

        let overlap = find_overlap(&first, &second).unwrap();
        assert_eq!(overlap, height - shift);
        let mut stitched = first.clone();
        assert!(stitched.append_next(&first, &second, None).unwrap());

        assert_eq!(stitched.height, height + shift);
        assert_eq!(
            stitched.rgba,
            patterned_frame(width, height + shift, 0).rgba
        );
    }

    #[test]
    fn prepends_frames_when_scroll_capture_moves_toward_the_top() {
        let width = 96;
        let height = 120;
        let shift = 24;
        let earlier = patterned_frame(width, height, 40);
        let later = patterned_frame(width, height, 40 + shift);
        let overlap = find_overlap(&earlier, &later).unwrap();
        assert_eq!(overlap, height - shift);

        let mut stitched = later;
        assert!(stitched.prepend_with_overlap(&earlier, overlap).unwrap());
        assert_eq!(stitched.height, height + shift);
        assert_eq!(
            stitched.rgba,
            patterned_frame(width, height + shift, 40).rgba
        );
    }

    #[test]
    fn infers_frame_order_from_pixels_when_scroll_direction_is_reversed() {
        let width = 96;
        let height = 120;
        let shift = 24;
        let upper = patterned_frame(width, height, 200);
        let lower = patterned_frame(width, height, 200 + shift);

        assert_eq!(
            find_frame_overlap(&lower, &upper, None, false).unwrap(),
            (height - shift, true)
        );
        assert_eq!(
            find_frame_overlap(&upper, &lower, None, true).unwrap(),
            (height - shift, false)
        );
    }

    fn sparse_page_frame(width: usize, height: usize, top_row: usize) -> PixelFrame {
        let mut rgba = vec![255; width * height * 4];
        for index in 0..width * 12 {
            let mut value = (index as u64)
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(0x517c_c1b7_2722_0a95);
            value ^= value >> 31;
            value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value ^= value >> 29;
            let x = (value % (width as u64 * 4 / 5)) as usize;
            let page_y = ((value >> 17) % 4200) as usize;
            let run = 2 + ((value >> 33) % 28) as usize;
            let thick = 2 + ((value >> 41) % 7) as usize;
            for y in page_y..page_y + thick {
                let Some(screen_y) = y.checked_sub(top_row) else {
                    continue;
                };
                if screen_y >= height {
                    continue;
                }
                for x in x..(x + run).min(width * 4 / 5) {
                    let offset = (screen_y * width + x) * 4;
                    rgba[offset..offset + 4].copy_from_slice(&[40, 40, 40, 255]);
                }
            }
        }
        // Fixed file list on the right: it must not drag the match toward 0.
        let sidebar = width * 86 / 100;
        for y in 0..height {
            for x in sidebar..width {
                let offset = (y * width + x) * 4;
                let tint = if x == sidebar || y % 90 == 0 {
                    210
                } else {
                    248
                };
                rgba[offset..offset + 4].copy_from_slice(&[tint, tint, tint, 255]);
            }
        }
        // Fixed toolbar at the top of the viewport.
        for y in 0..40 {
            for x in 0..width {
                let offset = (y * width + x) * 4;
                rgba[offset..offset + 4].copy_from_slice(&[228, 228, 228, 255]);
            }
        }
        for i in 0..14 {
            let x = 16 + i * 32;
            for y in 12..24 {
                let offset = (y * width + x) * 4;
                rgba[offset..offset + 4].copy_from_slice(&[60, 60, 60, 255]);
            }
        }
        PixelFrame {
            width,
            height,
            rgba,
        }
    }

    fn page_frame_with_fixed_composer(
        width: usize,
        height: usize,
        page_top: usize,
        composer_top: usize,
    ) -> PixelFrame {
        let mut frame = patterned_frame(width, height, page_top);
        let left = width / 18;
        let right = width - left;
        for y in composer_top..height {
            for x in left..right {
                let offset = (y * width + x) * 4;
                for channel in 0..3 {
                    frame.rgba[offset + channel] =
                        ((frame.rgba[offset + channel] as u16 * 6 + 246 * 94) / 100) as u8;
                }
            }
        }
        // Stationary divider and several rows of text and controls.
        for x in 12..width - 12 {
            let offset = (composer_top * width + x) * 4;
            frame.rgba[offset..offset + 4].copy_from_slice(&[190, 190, 190, 255]);
        }
        for line in 0..4 {
            let glyph_top = composer_top + 28 + line * 34;
            if glyph_top + 14 > height {
                break;
            }
            for glyph in 0..12 {
                let glyph_left = width / 4 + glyph * 8;
                for gy in 0..14 {
                    for gx in 0..5 {
                        if (gx * 3 + gy * 5 + glyph) % 4 != 0 {
                            let offset = ((glyph_top + gy) * width + glyph_left + gx) * 4;
                            frame.rgba[offset..offset + 4].copy_from_slice(&[70, 70, 70, 255]);
                        }
                    }
                }
            }
        }
        frame
    }

    fn page_frame_with_centered_translucent_composer(
        width: usize,
        height: usize,
        page_top: usize,
        composer_top: usize,
    ) -> PixelFrame {
        let mut frame = patterned_frame(width, height, page_top);
        let left = width * 25 / 100;
        let right = width * 75 / 100;
        for y in composer_top..height {
            for x in left..right {
                let offset = (y * width + x) * 4;
                for channel in 0..3 {
                    frame.rgba[offset + channel] =
                        ((frame.rgba[offset + channel] as u16 * 8 + 248 * 92) / 100) as u8;
                }
            }
        }
        // Stationary top edge and a few rows of controls provide structural
        // evidence even though most of the panel is nearly blank.
        for x in left..right {
            let offset = (composer_top * width + x) * 4;
            frame.rgba[offset..offset + 4].copy_from_slice(&[184, 184, 184, 255]);
        }
        for line in 0..3 {
            let glyph_top = composer_top + 36 + line * 42;
            if glyph_top + 16 >= height {
                continue;
            }
            for glyph in 0..18 {
                let glyph_left = left + 32 + glyph * 11;
                for gy in 0..14 {
                    for gx in 0..6 {
                        if (gx * 5 + gy * 3 + glyph) % 4 == 0 {
                            continue;
                        }
                        let offset = ((glyph_top + gy) * width + glyph_left + gx) * 4;
                        frame.rgba[offset..offset + 4].copy_from_slice(&[68, 68, 68, 255]);
                    }
                }
            }
        }
        frame
    }

    fn add_stationary_band(frame: &mut PixelFrame, top: usize, bottom: usize) {
        for y in top..bottom {
            for x in frame.width / 10..frame.width * 9 / 10 {
                let offset = (y * frame.width + x) * 4;
                let shade = if y % 24 < 2 || x % 29 < 2 { 198 } else { 238 };
                frame.rgba[offset..offset + 4].copy_from_slice(&[shade, shade, shade, 255]);
            }
        }
    }

    #[test]
    fn finds_offset_with_sparse_text_and_fixed_sidebar() {
        let width = 700;
        let height = 740;
        let shift = 240;
        let previous = sparse_page_frame(width, height, 0);
        let next = sparse_page_frame(width, height, shift);
        assert_eq!(find_overlap(&previous, &next).unwrap(), height - shift);
    }

    #[test]
    fn finds_offset_with_fixed_toolbar_and_local_animation() {
        let width = 520;
        let height = 640;
        let shift = 190;
        let mut previous = sparse_page_frame(width, height, 0);
        let mut next = sparse_page_frame(width, height, shift);
        // A blinking animation stays fixed on screen and changes between
        // captures; the page content behind it still has a known offset.
        for (frame, color) in [
            (&mut previous, [30, 90, 220, 255]),
            (&mut next, [230, 60, 40, 255]),
        ] {
            for y in 180..208 {
                for x in 160..196 {
                    let offset = (y * width + x) * 4;
                    frame.rgba[offset..offset + 4].copy_from_slice(&color);
                }
            }
        }
        assert_eq!(find_overlap(&previous, &next).unwrap(), height - shift);
    }

    #[test]
    fn excludes_a_fixed_bottom_composer_before_stitching() {
        let width = 320;
        let height = 600;
        let shift = 180;
        let composer_top = 438;
        let previous = page_frame_with_fixed_composer(width, height, 0, composer_top);
        let next = page_frame_with_fixed_composer(width, height, shift, composer_top);

        let content_height = fixed_bottom_ui_top(&previous, &next).unwrap();
        assert!(content_height < composer_top);
        assert!(
            content_height >= composer_top - height / 30,
            "detected content boundary {content_height}, expected near {composer_top}"
        );

        let first_content = previous.crop_to_height(content_height).unwrap();
        let second_content = next.crop_to_height(content_height).unwrap();
        let overlap = find_overlap(&first_content, &second_content).unwrap();
        assert_eq!(overlap, content_height - shift);

        let mut stitched = first_content;
        assert!(
            stitched
                .append_with_overlap(&second_content, overlap)
                .unwrap()
        );
        assert_eq!(stitched.height, content_height + shift);
        assert_eq!(stitched.width, width);
        let expected = patterned_frame(width, content_height + shift, 0);
        let mismatched = stitched
            .rgba
            .iter()
            .zip(&expected.rgba)
            .filter(|(actual, expected)| actual != expected)
            .count();
        assert_eq!(
            mismatched, 0,
            "the fixed composer must be absent from every stitched screen (crop={content_height})"
        );
    }

    #[test]
    fn chooses_bottom_composer_over_a_stationary_page_card() {
        let width = 320;
        let height = 1000;
        let shift = 200;
        let page_card_top = 700;
        let page_card_bottom = 850;
        let composer_top = 870;
        let mut previous = page_frame_with_fixed_composer(width, height, 0, composer_top);
        let mut next = page_frame_with_fixed_composer(width, height, shift, composer_top);
        add_stationary_band(&mut previous, page_card_top, page_card_bottom);
        add_stationary_band(&mut next, page_card_top, page_card_bottom);

        let content_height = fixed_bottom_ui_top(&previous, &next).unwrap();
        assert!(
            content_height >= page_card_bottom,
            "selected crop {content_height} cut into the stable page card"
        );
        assert!(
            content_height < composer_top,
            "selected crop {content_height} includes the fixed composer"
        );
    }

    #[test]
    fn retry_frame_keeps_the_fixed_bottom_composer_excluded() {
        let width = 320;
        let height = 600;
        let shift = 180;
        let composer_top = 438;
        let previous = page_frame_with_fixed_composer(width, height, 0, composer_top);
        let retry_full_frame = page_frame_with_fixed_composer(width, height, shift, composer_top);
        let capture_height = fixed_bottom_ui_top(&previous, &retry_full_frame).unwrap();
        let previous_content = previous.crop_to_height_at_most(capture_height).unwrap();
        let retry_content = retry_full_frame
            .crop_to_height_at_most(capture_height)
            .unwrap();

        assert_eq!(retry_content.height, previous_content.height);
        let overlap = find_overlap(&previous_content, &retry_content).unwrap();
        assert_eq!(overlap, capture_height - shift);
        let mut stitched = previous_content;
        assert!(
            stitched
                .append_with_overlap(&retry_content, overlap)
                .unwrap()
        );
        let expected = patterned_frame(width, capture_height + shift, 0);
        let mismatched = stitched
            .rgba
            .iter()
            .zip(&expected.rgba)
            .filter(|(actual, expected)| actual != expected)
            .count();
        assert_eq!(
            mismatched, 0,
            "retry output must exclude the composer (crop={capture_height})"
        );
    }

    #[test]
    fn stitches_multiple_viewports_without_repeating_fixed_bottom_ui() {
        let width = 320;
        let height = 600;
        let shift = 150;
        let composer_top = 438;
        let frames: Vec<_> = (0..5)
            .map(|index| page_frame_with_fixed_composer(width, height, index * shift, composer_top))
            .collect();

        let capture_height = fixed_bottom_ui_top(&frames[0], &frames[1]).unwrap();
        let mut previous = frames[0].crop_to_height(capture_height).unwrap();
        let mut stitched = previous.clone();
        for frame in frames.iter().skip(1) {
            let frame = frame
                .clone()
                .crop_to_height_at_most(capture_height)
                .unwrap();
            let overlap = find_overlap(&previous, &frame).unwrap();
            assert_eq!(overlap, capture_height - shift);
            assert!(stitched.append_with_overlap(&frame, overlap).unwrap());
            previous = frame;
        }

        let expected_height = capture_height + shift * (frames.len() - 1);
        assert_eq!(stitched.height, expected_height);
        assert_eq!(
            stitched.rgba,
            patterned_frame(width, expected_height, 0).rgba,
            "fixed composer pixels must never appear in the stitched page"
        );
    }

    #[test]
    fn excludes_a_narrow_translucent_composer_while_page_moves_on_both_sides() {
        let width = 960;
        let height = 900;
        let shift = 220;
        let composer_top = 620;
        let first = page_frame_with_centered_translucent_composer(width, height, 0, composer_top);
        let second =
            page_frame_with_centered_translucent_composer(width, height, shift, composer_top);

        let content_height = fixed_bottom_ui_top(&first, &second).unwrap();
        assert!(content_height < composer_top);
        assert!(content_height >= composer_top - 24);

        let first_content = first.crop_to_height(content_height).unwrap();
        let second_content = second.crop_to_height(content_height).unwrap();
        let overlap = find_overlap(&first_content, &second_content).unwrap();
        let mut stitched = first_content;
        assert!(
            stitched
                .append_with_overlap(&second_content, overlap)
                .unwrap()
        );
        assert_eq!(
            stitched.rgba,
            patterned_frame(width, content_height + shift, 0).rgba
        );
    }

    #[test]
    fn does_not_remove_a_stationary_blank_margin() {
        let width = 320;
        let height = 600;
        let previous = patterned_frame(width, height, 0);
        let next = patterned_frame(width, height, 160);
        assert_eq!(fixed_bottom_ui_top(&previous, &next), None);
    }

    #[test]
    fn ignores_a_single_blinking_pixel_when_detecting_scroll() {
        let previous = PixelFrame {
            width: 800,
            height: 900,
            rgba: vec![255; 800 * 900 * 4],
        };
        let mut next = previous.clone();
        next.rgba[0..4].copy_from_slice(&[0, 0, 0, 255]);
        assert!(previous.is_nearly_identical(&next));
    }

    #[test]
    fn detects_a_small_scroll_of_sparse_text_on_white_background() {
        let width = 800;
        let height = 900;
        let make_frame = |offset: usize| {
            let mut frame = PixelFrame {
                width,
                height,
                rgba: vec![255; width * height * 4],
            };
            // Three short lines of text occupy only a small part of the
            // otherwise white viewport, like a typical document page.
            for line in 0..3 {
                let top = 180 + line * 240;
                for y in top..top + 8 {
                    let page_y = y + offset;
                    if page_y >= height {
                        continue;
                    }
                    for glyph in 0..46 {
                        let left = 48 + glyph * 13;
                        for x in left..left + 7 {
                            if (x + y + glyph) % 4 == 0 {
                                continue;
                            }
                            let pixel = (page_y * width + x) * 4;
                            frame.rgba[pixel..pixel + 4].copy_from_slice(&[32, 32, 32, 255]);
                        }
                    }
                }
            }
            frame
        };
        let before = make_frame(0);
        let after = make_frame(8);
        assert!(!before.is_nearly_identical(&after));
    }

    #[test]
    fn keeps_identical_white_frames_stationary() {
        let frame = PixelFrame {
            width: 800,
            height: 900,
            rgba: vec![255; 800 * 900 * 4],
        };
        assert!(frame.is_nearly_identical(&frame));
    }

    #[test]
    fn detects_scrollbar_end_only_when_moving_toward_that_end() {
        use super::scroll_reached_end;

        assert!(scroll_reached_end(0.8, 1.0, 0.0, 1.0));
        assert!(scroll_reached_end(0.2, 0.0, 0.0, 1.0));
        assert!(!scroll_reached_end(1.0, 1.0, 0.0, 1.0));
        assert!(!scroll_reached_end(0.0, 0.0, 0.0, 1.0));
        assert!(!scroll_reached_end(0.8, 0.7, 0.0, 1.0));
    }

    #[test]
    fn classifies_no_scroll_as_a_cropable_result_instead_of_failure() {
        use super::{CaptureDisposition, PixelFrame, StitchCompletion, publish_disposition_for_crop};
        use std::path::PathBuf;

        let frame = PixelFrame {
            width: 2,
            height: 2,
            rgba: vec![
                1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255,
            ],
        };
        assert_eq!(
            publish_disposition_for_crop(
                &CaptureDisposition::Complete,
                frame.clone(),
                2,
                false,
                false,
                2.0,
                PathBuf::new(),
            ),
            Ok(Some(StitchCompletion::NoScroll)),
            "a page with no scroll progress must publish the initial viewport for cropping"
        );
        assert_eq!(
            crate::scrollpreview::cropped_rgba().unwrap(),
            (2, 2, frame.rgba)
        );
        crate::scrollpreview::clear_crop_image();
    }

    #[test]
    fn interrupted_capture_preserves_valid_frames_for_the_crop_preview() {
        use super::{PixelFrame, StitchCompletion, publish_disposition_for_crop};
        use std::path::PathBuf;

        let frame = PixelFrame {
            width: 2,
            height: 3,
            rgba: vec![
                1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255, 13, 14, 15,
                255, 16, 17, 18, 255,
            ],
        };
        let disposition = capture_disposition(&Err("画面重叠无法可靠识别".to_string()), false, 2);
        assert_eq!(
            disposition,
            CaptureDisposition::PreserveForCrop("拼接已暂停，保留已采集的 2 屏供裁剪".to_string())
        );
        assert_eq!(
            publish_disposition_for_crop(
                &disposition,
                frame.clone(),
                2,
                false,
                false,
                2.0,
                PathBuf::new(),
            )
            .unwrap(),
            Some(StitchCompletion::Partial)
        );
        assert_eq!(
            crate::scrollpreview::cropped_rgba().unwrap(),
            (2, 3, frame.rgba),
        );
        crate::scrollpreview::clear_crop_image();
    }

    #[test]
    fn explicit_cancel_does_not_open_a_partial_result() {
        use super::{PixelFrame, publish_disposition_for_crop};
        use std::path::PathBuf;

        assert_eq!(
            capture_disposition(&Err("__cancelled__".to_string()), false, 1),
            CaptureDisposition::Cancelled
        );
        let disposition = CaptureDisposition::Cancelled;
        assert_eq!(
            publish_disposition_for_crop(
                &disposition,
                PixelFrame {
                    width: 2,
                    height: 2,
                    rgba: vec![255; 16],
                },
                2,
                false,
                false,
                2.0,
                PathBuf::new(),
            )
            .unwrap(),
            None
        );
        assert_eq!(
            capture_disposition(&Err("目标应用失去前台".to_string()), true, 1),
            CaptureDisposition::Cancelled
        );
    }

    #[test]
    fn no_scroll_result_publishes_the_initial_frame_for_crop() {
        use super::{PixelFrame, StitchCompletion, publish_stitched_for_crop};
        use std::path::PathBuf;

        let frame = PixelFrame {
            width: 2,
            height: 2,
            rgba: vec![
                1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255,
            ],
        };
        assert_eq!(
            publish_stitched_for_crop(frame.clone(), 2, false, false, 2.0, PathBuf::new())
                .unwrap(),
            StitchCompletion::NoScroll
        );
        assert_eq!(
            crate::scrollpreview::cropped_rgba().unwrap(),
            (2, 2, frame.rgba)
        );
        let status = crate::scroll_shot_crop_status(None);
        assert!(status.contains("裁剪"));
        assert!(!status.contains("失败"));
        assert!(!status.contains("错误"));
        crate::scrollpreview::clear_crop_image();
    }

    #[test]
    fn accepts_a_short_capture_when_the_user_finishes_it() {
        use super::{StitchCompletion, classify_stitch_completion};

        assert_eq!(
            classify_stitch_completion(900, 1_300, true, false),
            StitchCompletion::Complete
        );
    }

    #[test]
    fn keeps_a_partial_stitch_as_a_cropable_result() {
        use super::{
            PixelFrame, StitchCompletion, classify_stitch_completion, publish_stitched_for_crop,
        };
        use std::path::PathBuf;

        assert_eq!(
            classify_stitch_completion(2_100, 1_300, false, true),
            StitchCompletion::Partial
        );

        let frame = PixelFrame {
            width: 2,
            height: 3,
            rgba: vec![
                1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255, 13, 14, 15, 255,
                16, 17, 18, 255,
            ],
        };
        assert_eq!(
            publish_stitched_for_crop(
                frame.clone(),
                2,
                false,
                true,
                2.0,
                PathBuf::new(),
            )
            .unwrap(),
            StitchCompletion::Partial
        );
        assert_eq!(
            crate::scrollpreview::cropped_rgba().unwrap(),
            (2, 3, frame.rgba)
        );
        crate::scrollpreview::clear_crop_image();
    }

    #[test]
    fn treats_a_viewport_crop_as_progress_when_a_fixed_composer_is_removed() {
        use super::{StitchCompletion, classify_stitch_completion};

        let original_viewport_height = 1_600;
        let content_viewport_height = 1_300;
        let stitched_height = 1_350;
        assert!(stitched_height < original_viewport_height);
        assert_eq!(
            classify_stitch_completion(stitched_height, content_viewport_height, false, false),
            StitchCompletion::Complete
        );
    }

    #[test]
    fn retries_the_other_direction_once_if_the_first_scroll_direction_is_stuck() {
        use super::retry_opposite_direction;

        let mut delta = -4;
        let mut tried_reverse = false;
        let mut stationary = 3;
        let mut pending = vec![-4, -4, -4];
        let mut stitch_upward = false;
        assert!(retry_opposite_direction(
            1,
            &mut tried_reverse,
            &mut delta,
            &mut stationary,
            &mut pending,
            &mut stitch_upward,
        ));
        assert_eq!(delta, 4);
        assert!(tried_reverse);
        assert!(stitch_upward);
        assert_eq!(stationary, 0);
        assert!(pending.is_empty());
        assert!(!retry_opposite_direction(
            1,
            &mut tried_reverse,
            &mut delta,
            &mut stationary,
            &mut pending,
            &mut stitch_upward,
        ));
        assert!(!retry_opposite_direction(
            2,
            &mut false,
            &mut delta,
            &mut stationary,
            &mut pending,
            &mut stitch_upward,
        ));
    }

    #[test]
    fn restores_mixed_scroll_directions_in_reverse_order() {
        let mut session = super::ScrollSession::new(
            1,
            objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 },
            super::ScrollTarget::Visual,
        );
        session.record_steps(&[-2, -2, 3, 3, -1]);
        assert_eq!(
            session.restore_steps().collect::<Vec<_>>(),
            vec![1, -3, -3, 2, 2]
        );
    }
}
