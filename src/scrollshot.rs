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
use crate::shot;
use crate::state::{Action, Rect, ScrollShotResult, log, push_action};

const CONTROL_CONTINUE: u8 = 0;
const CONTROL_FINISH: u8 = 1;
const CONTROL_CANCEL: u8 = 2;
const MAX_SCROLLS: usize = 90;
const MAX_IMAGE_HEIGHT: usize = 50_000;
const MAX_IMAGE_PIXELS: usize = 60_000_000;
const MAX_CAPTURE_PIXELS: usize = 20_000_000;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static CONTROL: AtomicU8 = AtomicU8::new(CONTROL_CONTINUE);
static TARGET_PID: AtomicI32 = AtomicI32::new(0);
static WORKER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

type AxError = i32;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
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
    TARGET_PID.store(target_pid, Ordering::Release);
    let worker = thread::spawn(move || {
        let result = match capture(rect, target_pid, scale, dir) {
            Ok((path, bytes, None)) => ScrollShotResult::Finished(path, bytes),
            Ok((path, bytes, Some(warning))) => {
                ScrollShotResult::FinishedWithWarning(path, bytes, warning)
            }
            Err(err) if err == "__cancelled__" => ScrollShotResult::Cancelled,
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
) -> Result<(PathBuf, u64, Option<String>), String> {
    if !unsafe { AXIsProcessTrusted() } {
        return Err(
            "滚动截屏需要辅助功能权限：请在系统设置 → 隐私与安全性 → 辅助功能中允许 iRec"
                .to_string(),
        );
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
        let inner_result = (|| {
            let mut stitched = initial.clone();
            let mut previous = stitched.clone();
            let mut last_value = session.value();
            let mut scrolls = 0;
            let mut screens = 1;
            let mut user_finished = false;
            let mut last_shift = None;
            let scroll_lines = (rect.h / 200.0).round().clamp(2.0, 4.0) as i32;
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
                    post_scroll(-scroll_lines)?;
                    scrolls += 1;
                    thread::sleep(Duration::from_millis(450));
                    if CONTROL.load(Ordering::Acquire) == CONTROL_CANCEL {
                        let frame = capture_frame(&filter, &config)?;
                        if !frame.is_nearly_identical(&previous) {
                            session.record(-scroll_lines);
                        }
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
                last_value = value;

                if frame.is_nearly_identical(&previous) {
                    if finish_requested {
                        break;
                    }
                    if !session.has_scrollbar() {
                        thread::sleep(Duration::from_millis(300));
                        frame = capture_frame(&filter, &config)?;
                        if frame.is_nearly_identical(&previous) {
                            break;
                        }
                    } else if value.is_some_and(|value| value >= 0.999_99) || !value_changed {
                        break;
                    } else {
                        continue;
                    }
                }
                if !finish_requested {
                    session.record(-scroll_lines);
                }
                let overlap = match find_overlap_with_hint(&previous, &frame, last_shift) {
                    Ok(overlap) => overlap,
                    Err(first_error) => {
                        // Smooth scrolling or a lazy-loading page can still be
                        // moving when the frame arrives. Give it one more beat.
                        thread::sleep(Duration::from_millis(550));
                        frame = capture_frame(&filter, &config)?;
                        find_overlap_with_hint(&previous, &frame, last_shift).map_err(|_| {
                            format!(
                                "{first_error}（已重试一次；请重新框选更大的页面区域，避开固定工具栏和动画内容）"
                            )
                        })?
                    }
                };
                let before = stitched.height;
                if !stitched.append_with_overlap(&frame, overlap)? {
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
                if finish_requested || value.is_some_and(|value| value >= 0.999_99) {
                    break;
                }
            }

            if CONTROL.load(Ordering::Acquire) == CONTROL_CANCEL {
                return Err("__cancelled__".to_string());
            }
            if stitched.height <= height && !user_finished {
                return Err("页面没有可继续滚动的内容".to_string());
            }

            let image = stitched.to_cg_image()?;
            let (path, bytes) = shot::save_png(&image, &dir)?;
            log(&format!(
                "scroll shot saved {} ({}x{})",
                path.display(),
                width,
                stitched.height
            ));
            Ok((path, bytes))
        })();

        // A finished long shot stays where it ended; only cancel/failure
        // rolls the page back to the original position.
        let restore_result = if inner_result.is_ok() {
            session.skip_restore();
            Ok(())
        } else {
            session.restore(|| capture_frame(&filter, &config), &initial)
        };
        match (inner_result, restore_result) {
            (Ok((path, bytes)), Ok(())) => Ok((path, bytes, None)),
            (Ok((path, bytes)), Err(error)) => {
                let warning = format!("长图已保存到 {}，但{error}", path.display());
                Ok((path, bytes, Some(warning)))
            }
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(restore_error)) if error == "__cancelled__" => {
                Err(format!("__cancelled__；{restore_error}"))
            }
            (Err(error), Err(restore_error)) => Err(format!("{error}；{restore_error}")),
        }
    })()
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

    fn record(&mut self, delta: i32) {
        self.steps.push(delta);
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
        for delta in self.steps.clone().into_iter().rev() {
            post_scroll_at(self.center, -delta)?;
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
    let system = retained_ax(unsafe { AXUIElementCreateSystemWide() })
        .ok_or_else(|| "无法连接 macOS 辅助功能服务".to_string())?;
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

fn post_scroll(delta: i32) -> Result<(), String> {
    // Wheel events are routed by the actual pointer. Use wherever the user
    // has the pointer, without moving it. The caller pauses when it leaves
    // the selection, so the user can click the preview panel or menu.
    let point = cursor_position().unwrap_or(CGPoint { x: 0.0, y: 0.0 });
    post_scroll_at(point, delta)
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
        let sx = (self.width / 64).max(1);
        let sy = (self.height / 64).max(1);
        let mut difference = 0u64;
        let mut samples = 0u64;
        for y in (0..self.height).step_by(sy) {
            for x in (0..self.width).step_by(sx) {
                let offset = (y * self.width + x) * 4;
                difference += self.rgba[offset].abs_diff(other.rgba[offset]) as u64;
                difference += self.rgba[offset + 1].abs_diff(other.rgba[offset + 1]) as u64;
                difference += self.rgba[offset + 2].abs_diff(other.rgba[offset + 2]) as u64;
                samples += 3;
            }
        }
        samples > 0 && difference as f64 / (samples as f64) < 1.5
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

    fn to_cg_image(&self) -> Result<CFRetained<CGImage>, String> {
        let data = unsafe { CFData::new(None, self.rgba.as_ptr(), self.rgba.len() as isize) }
            .ok_or_else(|| "无法建立长图像素数据".to_string())?;
        let provider = CGDataProvider::with_cf_data(Some(&data))
            .ok_or_else(|| "无法建立长图像数据源".to_string())?;
        let color_space =
            CGColorSpace::new_device_rgb().ok_or_else(|| "无法建立 RGB 色彩空间".to_string())?;
        let bitmap_info = CGBitmapInfo::from_bits_retain(
            CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
        );
        unsafe {
            CGImage::new(
                self.width,
                self.height,
                8,
                32,
                self.width * 4,
                Some(&color_space),
                bitmap_info,
                Some(&provider),
                ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        }
        .ok_or_else(|| "无法生成拼接后的长图".to_string())
    }
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
    let step_x = (previous.width / 160).max(1);
    let (active_columns, active_rows) = motion_mask(previous, next);
    let sample_columns: Vec<usize> = active_columns.iter().copied().step_by(step_x).collect();
    if sample_columns.is_empty() || active_rows.is_empty() {
        return Err("画面没有可用的滚动内容，无法拼接".to_string());
    }
    let mut best = (usize::MAX, 0usize);
    let mut best_bad = 1.0f64;
    let mut second = usize::MAX;
    // A two-pixel stride can miss the true offset by one pixel and make two
    // neighboring shifts look equally plausible. Check every row offset.
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
            continue;
        }
        let average = (score / samples) as usize;
        if average < best.0 {
            second = best.0;
            best_bad = bad as f64 / samples as f64;
            best = (average, shift);
        } else if shift.abs_diff(best.1) > height / 40 {
            second = second.min(average);
        }
    }
    // A perfect (or near-perfect) match may have a second candidate one
    // luminance step away on repetitive pages. Only reject an exact tie in
    // that case; for noisy frames keep the small tolerance.
    let ambiguous = if best.0 <= 1 {
        second <= best.0
    } else {
        second <= best.0.saturating_add(1)
    };
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

/// Columns and rows that actually change between the two frames. Fixed
/// toolbars, sidebars and blank margins are excluded so they cannot bias the
/// match toward a zero offset or create a false tie.
fn motion_mask(previous: &PixelFrame, next: &PixelFrame) -> (Vec<usize>, Vec<usize>) {
    let width = previous.width;
    let height = previous.height;
    let step_x = (width / 300).max(1);
    let mut columns = vec![0u64; width];
    let mut rows = vec![0u64; height];
    for (y, row) in rows.iter_mut().enumerate() {
        for x in (0..width).step_by(step_x) {
            let offset = (y * width + x) * 4;
            let lum_a = (previous.rgba[offset] as u32 * 3
                + previous.rgba[offset + 1] as u32 * 6
                + previous.rgba[offset + 2] as u32)
                / 10;
            let lum_b = (next.rgba[offset] as u32 * 3
                + next.rgba[offset + 1] as u32 * 6
                + next.rgba[offset + 2] as u32)
                / 10;
            let difference = lum_a.abs_diff(lum_b) as u64;
            columns[x] += difference;
            *row += difference;
        }
    }

    fn select(activity: &[u64], total: usize) -> Vec<usize> {
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

    (select(&columns, width), select(&rows, height))
}

#[cfg(test)]
mod tests {
    use super::{PixelFrame, find_overlap};

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

    #[test]
    fn finds_offset_with_sparse_text_and_fixed_sidebar() {
        let width = 700;
        let height = 740;
        let shift = 240;
        let previous = sparse_page_frame(width, height, 0);
        let next = sparse_page_frame(width, height, shift);
        assert_eq!(find_overlap(&previous, &next).unwrap(), height - shift);
    }

}
