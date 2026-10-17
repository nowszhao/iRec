//! Full-screen overlay used to drag-select a recording region.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSBezierPath, NSColor, NSCursor, NSEvent, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSStringDrawing, NSScreen, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedStringKey, NSMutableDictionary, NSPoint,
    NSRect, NSSize, NSString,
};

use crate::hud;
use crate::scrollshot;
use crate::state::{Action, Rect, push_action};

#[derive(Default)]
struct RegionIvars {
    /// 选完直接截图，而不是留下选区框等着录。
    shot: Cell<bool>,
    scroll_shot: Cell<bool>,
    scroll_target_pid: Cell<libc::pid_t>,
    start: Cell<Option<NSPoint>>,
    current: Cell<Option<NSPoint>>,
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = RegionIvars]
    struct RegionView;

    impl RegionView {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.convert_from_window(event);
            self.ivars().start.set(Some(point));
            self.ivars().current.set(Some(point));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.ivars()
                .current
                .set(Some(self.convert_from_window(event)));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let end = self.convert_from_window(event);
            let picked = self
                .ivars()
                .start
                .get()
                .map(|start| rect_from_points(start, end))
                .map(|r| Rect {
                    x: r.origin.x,
                    y: r.origin.y,
                    w: r.size.width,
                    h: r.size.height,
                });
            self.ivars().start.set(None);
            self.ivars().current.set(None);
            let valid = picked.filter(|rect| rect.w >= 16.0 && rect.h >= 16.0);
            if self.ivars().scroll_shot.get() && valid.is_some() {
                // Hand focus back while iRec is still the active app; the
                // system can refuse the handoff after the overlay closes.
                scrollshot::activate_pid(self.mtm(), self.ivars().scroll_target_pid.get());
            }
            if let Some(window) = self.window() {
                window.close();
            }
            NSApplication::sharedApplication(self.mtm())
                .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            match valid {
                Some(rect) => {
                    if self.ivars().scroll_shot.get() {
                        push_action(Action::ScrollShotRegion(rect, self.ivars().scroll_target_pid.get()));
                    } else if self.ivars().shot.get() {
                        push_action(Action::ShotRegion(rect));
                    } else {
                        // The frame stays on screen; recording starts from its bar.
                        hud::show(rect);
                    }
                }
                None if self.ivars().scroll_shot.get() => {
                    scrollshot::activate_pid(
                        self.mtm(),
                        self.ivars().scroll_target_pid.get(),
                    );
                    push_action(Action::ScrollShotSelectionCancelled);
                }
                None => {}
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, _event: &NSEvent) {
            if self.ivars().scroll_shot.get() {
                push_action(Action::ScrollShotSelectionCancelled);
            }
            if let Some(window) = self.window() {
                window.close();
            }
            NSApplication::sharedApplication(self.mtm())
                .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            if self.ivars().scroll_shot.get() {
                scrollshot::activate_pid(self.mtm(), self.ivars().scroll_target_pid.get());
            }
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            self.draw();
        }
    }
);

impl RegionView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RegionIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn convert_from_window(&self, event: &NSEvent) -> NSPoint {
        let point = event.locationInWindow();
        self.convertPoint_fromView(point, None)
    }

    fn selection(&self) -> Option<NSRect> {
        let start = self.ivars().start.get()?;
        let current = self.ivars().current.get()?;
        Some(rect_from_points(start, current))
    }

    fn draw(&self) {
        let bounds = self.bounds();
        NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.35).setFill();
        NSBezierPath::fillRect(bounds);
        let attrs = label_attrs();

        match self.selection() {
            Some(sel) => {
                // Dim everything but the selection by filling four bands.
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(bounds.origin.x, sel.max().y),
                    NSSize::new(bounds.size.width, bounds.max().y - sel.max().y),
                ));
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(bounds.origin.x, bounds.origin.y),
                    NSSize::new(bounds.size.width, sel.origin.y - bounds.origin.y),
                ));
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(bounds.origin.x, sel.origin.y),
                    NSSize::new(sel.origin.x - bounds.origin.x, sel.size.height),
                ));
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(sel.max().x, sel.origin.y),
                    NSSize::new(bounds.max().x - sel.max().x, sel.size.height),
                ));

                let path = NSBezierPath::bezierPathWithRect(sel);
                path.setLineWidth(1.0);
                NSColor::colorWithSRGBRed_green_blue_alpha(0.20, 0.70, 1.0, 1.0).setStroke();
                path.stroke();

                let label = NSString::from_str(&format!(
                    "{} × {}",
                    sel.size.width.round() as i64,
                    sel.size.height.round() as i64
                ));
                unsafe {
                    label.drawAtPoint_withAttributes(
                        NSPoint::new(sel.origin.x + 6.0, sel.max().y + 6.0),
                        Some(&attrs),
                    );
                }
            }
            None => {
                let hint = NSString::from_str(if self.ivars().scroll_shot.get() {
                    "拖动选择滚动截屏区域 · 按任意键取消"
                } else {
                    "拖动选择录制区域 · 按任意键取消"
                });
                let size = unsafe { hint.sizeWithAttributes(Some(&attrs)) };
                unsafe {
                    hint.drawAtPoint_withAttributes(
                        NSPoint::new(
                            bounds.origin.x + bounds.size.width / 2.0 - size.width / 2.0,
                            bounds.origin.y + bounds.size.height / 2.0 - size.height / 2.0,
                        ),
                        Some(&attrs),
                    );
                }
            }
        }
    }
}

type AttrsDict = NSMutableDictionary<NSAttributedStringKey, AnyObject>;

fn label_attrs() -> Retained<AttrsDict> {
    let attrs: Retained<AttrsDict> = NSMutableDictionary::new();
    unsafe {
        let font: &AnyObject = &*NSFont::systemFontOfSize(13.0);
        attrs.setObject_forKey(font, ProtocolObject::from_ref(NSFontAttributeName));
        let color: &AnyObject = &*NSColor::whiteColor();
        attrs.setObject_forKey(color, ProtocolObject::from_ref(NSForegroundColorAttributeName));
    }
    attrs
}

fn rect_from_points(a: NSPoint, b: NSPoint) -> NSRect {
    NSRect::new(
        NSPoint::new(a.x.min(b.x), a.y.min(b.y)),
        NSSize::new((a.x - b.x).abs(), (a.y - b.y).abs()),
    )
}

/// Show the overlay on the main screen. The result arrives as an
/// 选完如果是截图模式就 push `Action::ShotRegion`，录制模式则留下选区框；取消什么都不做。
/// `shot = true` 时选完直接截图，否则留下选区框等着录。
pub fn pick_region(mtm: MainThreadMarker, shot: bool) {
    pick_region_mode(mtm, shot, false, 0);
}

pub fn pick_scroll_region(mtm: MainThreadMarker, target_pid: libc::pid_t) {
    pick_region_mode(mtm, true, true, target_pid);
}

fn pick_region_mode(
    mtm: MainThreadMarker,
    shot: bool,
    scroll_shot: bool,
    target_pid: libc::pid_t,
) {
    let app = NSApplication::sharedApplication(mtm);
    // Become a regular app briefly so the overlay can become key and take
    // keyboard events (Escape / any key cancels).
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let screen = match NSScreen::mainScreen(mtm) {
        Some(s) => s,
        None => return,
    };
    let frame = screen.frame();

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setOpaque(false);
    window.setHasShadow(false);
    window.setLevel(1000);
    window.setIgnoresMouseEvents(false);

    let view = RegionView::new(mtm);
    view.ivars().shot.set(shot);
    view.ivars().scroll_shot.set(scroll_shot);
    view.ivars().scroll_target_pid.set(target_pid);
    view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
    view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    if let Some(content) = window.contentView() {
        content.addSubview(&view);
    }

    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    NSCursor::crosshairCursor().set();

    // The window owns the view; both stay alive until it is closed.
    std::mem::forget(view);
    std::mem::forget(window);
}
