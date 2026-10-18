//! Persistent region HUD: the selection frame stays on screen after picking,
//! can be dragged / resized, and carries its own start-stop control bar.
//!
//! Three windows cooperate:
//! * `frame_win`  - click-through visual outline (cyan idle, red while recording)
//! * `bar_win`    - control bar: size + timer + 截屏 / 开始录制 / 停止录制 + 关闭
//! * `handle_win` - small grip at the bottom-right corner to resize the region
//!
//! All of them are excluded from the capture (see `recorder::own_windows`) so
//! they never show up in the recorded video.

use std::cell::Cell;
use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSButton, NSColor, NSEvent, NSFont, NSScreen, NSTextField,
    NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use crate::state::{Action, Rect, push_action};

const BAR_W: f64 = 396.0;
const BAR_H: f64 = 34.0;
const HANDLE: f64 = 18.0;
const MIN_SIZE: f64 = 48.0;

static HUD: Mutex<Option<Hud>> = Mutex::new(None);

struct Hud {
    rect: Rect,
    recording: bool,
    elapsed: String,
    /// The region was moved/resized while recording: it applies next time.
    pending: bool,
    controller: Retained<HudController>,
    frame_win: Retained<NSWindow>,
    frame_view: Retained<FrameView>,
    bar_win: Retained<NSWindow>,
    bar_label: Retained<NSTextField>,
    bar_shot: Retained<NSButton>,
    bar_button: Retained<NSButton>,
    bar_close: Retained<NSButton>,
    handle_win: Retained<NSWindow>,
}

// SAFETY: every HUD window, view and the controller are created and touched
// only on the main thread (overlay pick, drag handlers, app timer).
unsafe impl Send for Hud {}

#[derive(Default)]
struct FrameIvars {
    recording: Cell<bool>,
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = FrameIvars]
    struct FrameView;

    impl FrameView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            let recording = self.ivars().recording.get();
            let bounds = self.bounds();
            // Dark hairline so the outline stays visible on light content.
            let outer = NSBezierPath::bezierPathWithRect(NSRect::new(
                NSPoint::new(bounds.origin.x + 0.5, bounds.origin.y + 0.5),
                NSSize::new(bounds.size.width - 1.0, bounds.size.height - 1.0),
            ));
            outer.setLineWidth(1.0);
            NSColor::colorWithSRGBRed_green_blue_alpha(0.04, 0.06, 0.09, 0.5).setStroke();
            outer.stroke();

            let accent = if recording {
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.30, 0.26, 1.0)
            } else {
                NSColor::controlAccentColor()
            };
            let inner = NSBezierPath::bezierPathWithRect(NSRect::new(
                NSPoint::new(bounds.origin.x + 1.5, bounds.origin.y + 1.5),
                NSSize::new(bounds.size.width - 3.0, bounds.size.height - 3.0),
            ));
            inner.setLineWidth(1.0);
            accent.setStroke();
            inner.stroke();

            let tick = NSBezierPath::bezierPath();
            tick.setLineWidth(2.0);
            let x0 = bounds.origin.x + 1.5;
            let y0 = bounds.origin.y + 1.5;
            let x1 = x0 + bounds.size.width - 3.0;
            let y1 = y0 + bounds.size.height - 3.0;
            let arm = 16.0f64;
            for (px, sx) in [(x0, 1.0f64), (x1, -1.0f64)] {
                for (py, sy) in [(y0, 1.0f64), (y1, -1.0f64)] {
                    tick.moveToPoint(NSPoint::new(px, py + sy * arm));
                    tick.lineToPoint(NSPoint::new(px, py));
                    tick.lineToPoint(NSPoint::new(px + sx * arm, py));
                }
            }
            accent.setStroke();
            tick.stroke();
            }
        }
);

impl FrameView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(FrameIvars::default());
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Default)]
struct BarIvars {
    last: Cell<Option<NSPoint>>,
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = BarIvars]
    struct BarView;

    impl BarView {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.ivars().last.set(Some(screen_point(self, event)));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let point = screen_point(self, event);
            let last = self.ivars().last.get().unwrap_or(point);
            self.ivars().last.set(Some(point));
            move_by(point.x - last.x, point.y - last.y);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().last.set(None);
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            let bounds = self.bounds();
            let pill = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                NSRect::new(
                    NSPoint::new(bounds.origin.x + 1.0, bounds.origin.y + 2.0),
                    NSSize::new(bounds.size.width - 2.0, bounds.size.height - 4.0),
                ),
                9.0,
                9.0,
            );
            NSColor::windowBackgroundColor().setFill();
            pill.fill();
            pill.setLineWidth(1.0);
            NSColor::separatorColor().setStroke();
            pill.stroke();
        }
    }
);

impl BarView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(BarIvars::default());
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Default)]
struct HandleIvars {
    last: Cell<Option<NSPoint>>,
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = HandleIvars]
    struct HandleView;

    impl HandleView {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.ivars().last.set(Some(screen_point(self, event)));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let point = screen_point(self, event);
            self.ivars().last.set(Some(point));
            resize_to(point);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().last.set(None);
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            let bounds = self.bounds();
            let dot = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                NSPoint::new(bounds.origin.x + 2.0, bounds.origin.y + 2.0),
                NSSize::new(bounds.size.width - 4.0, bounds.size.height - 4.0),
            ));
            NSColor::windowBackgroundColor().setFill();
            dot.fill();
            dot.setLineWidth(2.0);
            NSColor::controlAccentColor().setStroke();
            dot.stroke();
        }
    }
);

impl HandleView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HandleIvars::default());
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Default)]
struct ControllerIvars {}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ControllerIvars]
    struct HudController;

    unsafe impl NSObjectProtocol for HudController {}

    impl HudController {
        /// One button that toggles: start when idle, stop while recording.
        #[unsafe(method(hudToggle:))]
        fn hud_toggle(&self, _sender: &AnyObject) {
            let state = peek(|h| (h.rect, h.recording));
            if let Some((rect, recording)) = state {
                if recording {
                    push_action(Action::Stop);
                } else {
                    push_action(Action::StartRegion(rect));
                }
            }
        }

        #[unsafe(method(hudShot:))]
        fn hud_shot(&self, _sender: &AnyObject) {
            let state = peek(|h| (h.rect, h.recording));
            if let Some((rect, recording)) = state {
                if !recording {
                    push_action(Action::ShotRegion(rect));
                }
            }
        }

        #[unsafe(method(hudClose:))]
        fn hud_close(&self, _sender: &AnyObject) {
            hide();
        }
    }
);

impl HudController {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ControllerIvars::default());
        unsafe { msg_send![super(this), init] }
    }
}

fn screen_point(view: &NSView, event: &NSEvent) -> NSPoint {
    match view.window() {
        Some(window) => window.convertPointToScreen(event.locationInWindow()),
        None => event.locationInWindow(),
    }
}

fn with_hud<R>(f: impl FnOnce(&mut Hud) -> R) -> Option<R> {
    let mut guard = HUD.lock().ok()?;
    guard.as_mut().map(f)
}

fn peek<R>(f: impl FnOnce(&Hud) -> R) -> Option<R> {
    let guard = HUD.lock().ok()?;
    guard.as_ref().map(f)
}

fn screen_frame(mtm: MainThreadMarker) -> NSRect {
    NSScreen::mainScreen(mtm)
        .map(|s| s.frame())
        .unwrap_or(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(1440.0, 900.0),
        ))
}

fn clamp_value(v: f64, min: f64, max: f64) -> f64 {
    if max < min { min } else { v.clamp(min, max) }
}

fn clamp_rect(rect: Rect, mtm: MainThreadMarker) -> Rect {
    let s = screen_frame(mtm);
    let w = clamp_value(rect.w, MIN_SIZE, s.size.width);
    let h = clamp_value(rect.h, MIN_SIZE, s.size.height);
    let x = clamp_value(rect.x, s.origin.x, s.origin.x + s.size.width - w);
    let y = clamp_value(rect.y, s.origin.y, s.origin.y + s.size.height - h);
    Rect { x, y, w, h }
}

fn make_window(mtm: MainThreadMarker) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(200.0, 200.0)),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(false);
    window.setLevel(1000);
    window.setHidesOnDeactivate(false);
    // Keep the HUD out of any screen capture (including our own).
    window.setSharingType(objc2_app_kit::NSWindowSharingType(0));
    window
}

fn make_label(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSTextField> {
    let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
    field.setBezeled(false);
    field.setDrawsBackground(false);
    field.setEditable(false);
    field.setSelectable(false);
    field.setTextColor(Some(&NSColor::labelColor()));
    field.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    field
}

fn make_button(
    mtm: MainThreadMarker,
    title: &str,
    frame: NSRect,
    sel_name: Sel,
) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    button.setTitle(&NSString::from_str(title));
    button.setBezelStyle(objc2_app_kit::NSBezelStyle::Rounded);
    button.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    unsafe { button.setAction(Some(sel_name)) };
    button
}

fn build(mtm: MainThreadMarker, rect: Rect) -> Hud {
    let controller = HudController::new(mtm);

    // 1. outline (click-through)
    let frame_win = make_window(mtm);
    frame_win.setIgnoresMouseEvents(true);
    let frame_view = FrameView::new(mtm);
    frame_view.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(rect.w, rect.h),
    ));
    if let Some(content) = frame_win.contentView() {
        content.addSubview(&frame_view);
    }

    // 2. control bar
    let bar_win = make_window(mtm);
    let bar_view = BarView::new(mtm);
    bar_view.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(BAR_W, BAR_H),
    ));
    if let Some(content) = bar_win.contentView() {
        content.addSubview(&bar_view);
    }
    let bar_label = make_label(
        mtm,
        NSRect::new(NSPoint::new(12.0, 7.0), NSSize::new(150.0, 20.0)),
    );
    bar_view.addSubview(&bar_label);
    // 同一个选区框，既能录也能截。
    let bar_shot = make_button(
        mtm,
        "截图",
        NSRect::new(NSPoint::new(148.0, 6.0), NSSize::new(64.0, 22.0)),
        sel!(hudShot:),
    );
    bar_view.addSubview(&bar_shot);
    let bar_button = make_button(
        mtm,
        "开始录制",
        NSRect::new(NSPoint::new(218.0, 6.0), NSSize::new(104.0, 22.0)),
        sel!(hudToggle:),
    );
    bar_view.addSubview(&bar_button);
    let bar_close = make_button(
        mtm,
        "关闭",
        NSRect::new(NSPoint::new(328.0, 6.0), NSSize::new(56.0, 22.0)),
        sel!(hudClose:),
    );
    bar_view.addSubview(&bar_close);
    unsafe {
        bar_shot.setTarget(Some(&*controller));
        bar_button.setTarget(Some(&*controller));
        bar_close.setTarget(Some(&*controller));
    }

    // 3. resize grip
    let handle_win = make_window(mtm);
    let handle_view = HandleView::new(mtm);
    handle_view.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(HANDLE, HANDLE),
    ));
    if let Some(content) = handle_win.contentView() {
        content.addSubview(&handle_view);
    }

    Hud {
        rect,
        recording: false,
        elapsed: String::new(),
        pending: false,
        controller,
        frame_win,
        frame_view,
        bar_win,
        bar_label,
        bar_shot,
        bar_button,
        bar_close,
        handle_win,
    }
}

fn label_text(rect: &Rect, recording: bool, elapsed: &str, pending: bool) -> String {
    let mut text = format!("{} × {}", rect.w.round() as i64, rect.h.round() as i64);
    if recording && !elapsed.is_empty() {
        text.push_str(&format!(" · ● {elapsed}"));
    }
    if pending {
        text.push_str(" · 新区域下次生效");
    }
    text
}

/// Updates the label / buttons (cheap, safe to call from the app timer).
fn refresh_controls(hud: &mut Hud) {
    let text = label_text(&hud.rect, hud.recording, &hud.elapsed, hud.pending);
    hud.bar_label.setStringValue(&NSString::from_str(&text));
    hud.bar_button
        .setTitle(&NSString::from_str(if hud.recording {
            "停止录制"
        } else {
            "开始录制"
        }));
    hud.bar_close.setEnabled(!hud.recording);
    hud.bar_shot.setEnabled(!hud.recording);
}

/// Moves/resizes the three windows to match the current region.
fn layout(hud: &mut Hud) {
    let mtm = MainThreadMarker::new().expect("HUD must run on the main thread");
    let s = screen_frame(mtm);
    let r = hud.rect;
    hud.frame_win.setFrame_display(
        NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.w, r.h)),
        true,
    );
    hud.frame_view
        .setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(r.w, r.h)));

    let bx = clamp_value(
        r.x,
        s.origin.x + 4.0,
        s.origin.x + s.size.width - BAR_W - 4.0,
    );
    let above = r.y + r.h + 8.0;
    let by = if above + BAR_H > s.origin.y + s.size.height {
        if r.y - BAR_H - 8.0 >= s.origin.y + 4.0 {
            r.y - BAR_H - 8.0
        } else {
            (r.y + r.h - BAR_H - 8.0).max(s.origin.y + 4.0)
        }
    } else {
        above
    };
    hud.bar_win.setFrame_display(
        NSRect::new(NSPoint::new(bx, by), NSSize::new(BAR_W, BAR_H)),
        true,
    );

    let hx = clamp_value(
        r.x + r.w - HANDLE / 2.0,
        s.origin.x,
        s.origin.x + s.size.width - HANDLE,
    );
    let hy = clamp_value(
        r.y - HANDLE / 2.0,
        s.origin.y,
        s.origin.y + s.size.height - HANDLE,
    );
    hud.handle_win.setFrame_display(
        NSRect::new(NSPoint::new(hx, hy), NSSize::new(HANDLE, HANDLE)),
        true,
    );

    refresh_controls(hud);
}

fn order_front(hud: &Hud) {
    hud.frame_win.orderFront(None);
    hud.bar_win.orderFront(None);
    hud.handle_win.orderFront(None);
}

/// Shows (or moves) the region HUD. Called right after the user picked a
/// region; recording does not start until the control bar button is pressed.
pub fn show(rect: Rect) {
    let mtm = MainThreadMarker::new().expect("HUD must run on the main thread");
    let rect = clamp_rect(rect, mtm);
    let mut guard = HUD.lock().unwrap_or_else(|p| p.into_inner());
    match guard.as_mut() {
        Some(hud) => {
            hud.rect = rect;
            hud.pending = false;
            layout(hud);
            order_front(hud);
        }
        None => {
            let mut hud = build(mtm, rect);
            layout(&mut hud);
            order_front(&hud);
            *guard = Some(hud);
        }
    }
}

/// 当前框选的区域（选区框还显示着才有），⇧⌘A 可以直接截它。
pub fn current_rect() -> Option<Rect> {
    peek(|h| {
        if h.bar_win.isVisible() {
            Some(h.rect)
        } else {
            None
        }
    })
    .flatten()
}

/// Hides every HUD window without dropping it.
pub fn hide() {
    let mut guard = match HUD.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(hud) = guard.as_mut() {
        hud.frame_win.orderOut(None);
        hud.bar_win.orderOut(None);
        hud.handle_win.orderOut(None);
    }
}

/// Drops the HUD completely (used when quitting).
pub fn close() {
    let mut guard = match HUD.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(hud) = guard.take() {
        hud.frame_win.orderOut(None);
        hud.bar_win.orderOut(None);
        hud.handle_win.orderOut(None);
        let _ = hud.controller;
    }
}

/// Mirrors the recorder state into the HUD (called from the app timer).
pub fn sync(recording: bool, elapsed: Option<String>) {
    let mut guard = match HUD.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let Some(hud) = guard.as_mut() else { return };
    hud.recording = recording;
    hud.elapsed = elapsed.unwrap_or_default();
    if !recording {
        hud.pending = false;
    }
    if hud.frame_view.ivars().recording.get() != recording {
        hud.frame_view.ivars().recording.set(recording);
        hud.frame_view.setNeedsDisplay(true);
    }
    refresh_controls(hud);
}

fn move_by(dx: f64, dy: f64) {
    let mtm = match MainThreadMarker::new() {
        Some(mtm) => mtm,
        None => return,
    };
    with_hud(|hud| {
        hud.rect.x += dx;
        hud.rect.y += dy;
        hud.rect = clamp_rect(hud.rect, mtm);
        if hud.recording {
            hud.pending = true;
        }
        layout(hud);
    });
}

fn resize_to(point: NSPoint) {
    let mtm = match MainThreadMarker::new() {
        Some(mtm) => mtm,
        None => return,
    };
    with_hud(|hud| {
        let top = hud.rect.y + hud.rect.h;
        let w = (point.x - hud.rect.x).max(MIN_SIZE);
        let h = (top - point.y).max(MIN_SIZE);
        hud.rect.w = w;
        hud.rect.h = h;
        hud.rect.y = top - h;
        hud.rect = clamp_rect(hud.rect, mtm);
        if hud.recording {
            hud.pending = true;
        }
        layout(hud);
    });
}
