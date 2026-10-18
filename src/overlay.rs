//! Full-screen overlay used to drag-select a recording region.

use std::cell::{Cell, OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSBezelStyle, NSBezierPath, NSButton, NSColor, NSCompositingOperation, NSCursor, NSEvent,
    NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSRectFillUsingOperation,
    NSScreen, NSStringDrawing, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedStringKey, NSMutableDictionary, NSPoint, NSRect, NSSize, NSString,
};

use crate::hud;
use crate::scrollshot;
use crate::state::{Action, Rect, push_action};

thread_local! {
    static SELECTION_WINDOW: RefCell<Option<Retained<SelectionWindow>>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super = NSWindow)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ()]
    struct SelectionWindow;

    impl SelectionWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool { true }
    }
);

#[derive(Default)]
struct RegionIvars {
    /// 选完直接截图，而不是留下选区框等着录。
    shot: Cell<bool>,
    scroll_shot: Cell<bool>,
    scroll_target_pid: Cell<libc::pid_t>,
    start: Cell<Option<NSPoint>>,
    current: Cell<Option<NSPoint>>,
    confirm: OnceCell<Retained<NSButton>>,
    cancel: OnceCell<Retained<NSButton>>,
    annotate: OnceCell<Retained<NSButton>>,
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
            self.hide_actions();
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
            self.ivars().current.set(Some(self.convert_from_window(event)));
            self.show_actions();
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(confirmSelection:))]
        fn confirm_selection(&self, _: &AnyObject) { self.finish_selection(false); }

        #[unsafe(method(annotateSelection:))]
        fn annotate_selection(&self, _: &AnyObject) { self.finish_selection(true); }

        #[unsafe(method(cancelSelection:))]
        fn cancel_selection(&self, _: &AnyObject) { self.cancel(); }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            match event.keyCode() {
                53 => self.cancel(),
                36 | 76 => self.finish_selection(false),
                _ => {},
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

    fn hide_actions(&self) {
        for button in [
            self.ivars().confirm.get(),
            self.ivars().cancel.get(),
            self.ivars().annotate.get(),
        ]
        .into_iter()
        .flatten()
        {
            button.setHidden(true);
        }
    }

    fn show_actions(&self) {
        let Some(rect) = self
            .selection()
            .filter(|r| r.size.width >= 16.0 && r.size.height >= 16.0)
        else {
            self.hide_actions();
            return;
        };
        let bounds = self.bounds();
        let x = rect.origin.x.clamp(
            8.0,
            (bounds.size.width
                - if self.ivars().annotate.get().is_some() {
                    328.0
                } else {
                    220.0
                })
            .max(8.0),
        );
        let y = if rect.origin.y >= 46.0 {
            rect.origin.y - 38.0
        } else if rect.max().y + 38.0 < bounds.size.height {
            rect.max().y + 8.0
        } else {
            8.0
        };
        for (i, button) in [
            self.ivars().cancel.get(),
            self.ivars().confirm.get(),
            self.ivars().annotate.get(),
        ]
        .into_iter()
        .flatten()
        .enumerate()
        {
            button.setFrame(NSRect::new(
                NSPoint::new(x + i as f64 * 108.0, y),
                NSSize::new(100.0, 30.0),
            ));
            button.setHidden(false);
        }
    }

    fn dismiss(&self) {
        if let Some(window) = self.window() {
            window.close();
        }
        NSCursor::arrowCursor().set();
        NSApplication::sharedApplication(self.mtm())
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }

    fn cancel(&self) {
        if self.ivars().scroll_shot.get() {
            scrollshot::activate_pid(self.mtm(), self.ivars().scroll_target_pid.get());
            push_action(Action::ScrollShotSelectionCancelled);
        }
        self.dismiss();
    }

    fn finish_selection(&self, annotate: bool) {
        let Some(r) = self
            .selection()
            .filter(|r| r.size.width >= 16.0 && r.size.height >= 16.0)
        else {
            return;
        };
        let rect = Rect {
            x: r.origin.x,
            y: r.origin.y,
            w: r.size.width,
            h: r.size.height,
        };
        if self.ivars().scroll_shot.get() {
            scrollshot::activate_pid(self.mtm(), self.ivars().scroll_target_pid.get());
        }
        self.dismiss();
        if self.ivars().scroll_shot.get() {
            push_action(Action::ScrollShotRegion(
                rect,
                self.ivars().scroll_target_pid.get(),
            ));
        } else if self.ivars().shot.get() {
            push_action(if annotate {
                Action::ShotRegionAnnotated(rect)
            } else {
                Action::ShotRegion(rect)
            });
        } else {
            hud::show(rect);
        }
    }

    fn convert_from_window(&self, event: &NSEvent) -> NSPoint {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let bounds = self.bounds();
        NSPoint::new(
            point.x.clamp(bounds.origin.x, bounds.max().x),
            point.y.clamp(bounds.origin.y, bounds.max().y),
        )
    }

    fn selection(&self) -> Option<NSRect> {
        let start = self.ivars().start.get()?;
        let current = self.ivars().current.get()?;
        Some(rect_from_points(start, current))
    }

    fn draw(&self) {
        let bounds = self.bounds();
        NSRectFillUsingOperation(bounds, NSCompositingOperation::Clear);
        NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.32).setFill();
        let attrs = label_attrs();

        match self.selection() {
            Some(sel) => {
                for band in outside_rects(bounds, sel) {
                    NSBezierPath::fillRect(band);
                }

                let path = NSBezierPath::bezierPathWithRect(sel);
                path.setLineWidth(1.0);
                NSColor::controlAccentColor().setStroke();
                path.stroke();

                let label = NSString::from_str(&format!(
                    "{} × {} px · Enter 确认",
                    (sel.size.width * crate::state::with_state(|s| s.settings.scale)).round()
                        as i64,
                    (sel.size.height * crate::state::with_state(|s| s.settings.scale)).round()
                        as i64
                ));
                let size = unsafe { label.sizeWithAttributes(Some(&attrs)) };
                let x = sel
                    .origin
                    .x
                    .clamp(6.0, (bounds.size.width - size.width - 18.0).max(6.0));
                let y = if sel.max().y + 30.0 < bounds.size.height {
                    sel.max().y + 6.0
                } else {
                    (sel.max().y - 24.0).max(6.0)
                };
                NSColor::blackColor()
                    .colorWithAlphaComponent(0.75)
                    .setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    NSRect::new(
                        NSPoint::new(x, y - 3.0),
                        NSSize::new(size.width + 12.0, size.height + 6.0),
                    ),
                    5.0,
                    5.0,
                )
                .fill();
                unsafe {
                    label.drawAtPoint_withAttributes(NSPoint::new(x + 6.0, y), Some(&attrs));
                }
            }
            None => {
                NSBezierPath::fillRect(bounds);
                let hint = NSString::from_str(if self.ivars().scroll_shot.get() {
                    "拖动选择滚动截图区域 · Esc 取消"
                } else if self.ivars().shot.get() {
                    "拖动选择截图区域 · Esc 取消"
                } else {
                    "拖动选择录制区域 · Esc 取消"
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
        attrs.setObject_forKey(
            color,
            ProtocolObject::from_ref(NSForegroundColorAttributeName),
        );
    }
    attrs
}

fn rect_from_points(a: NSPoint, b: NSPoint) -> NSRect {
    NSRect::new(
        NSPoint::new(a.x.min(b.x), a.y.min(b.y)),
        NSSize::new((a.x - b.x).abs(), (a.y - b.y).abs()),
    )
}

/// Non-overlapping bands leave every pixel inside the selection untouched.
fn outside_rects(bounds: NSRect, selection: NSRect) -> [NSRect; 4] {
    [
        NSRect::new(
            NSPoint::new(bounds.origin.x, selection.max().y),
            NSSize::new(bounds.size.width, bounds.max().y - selection.max().y),
        ),
        NSRect::new(
            bounds.origin,
            NSSize::new(bounds.size.width, selection.origin.y - bounds.origin.y),
        ),
        NSRect::new(
            NSPoint::new(bounds.origin.x, selection.origin.y),
            NSSize::new(selection.origin.x - bounds.origin.x, selection.size.height),
        ),
        NSRect::new(
            NSPoint::new(selection.max().x, selection.origin.y),
            NSSize::new(bounds.max().x - selection.max().x, selection.size.height),
        ),
    ]
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

fn pick_region_mode(mtm: MainThreadMarker, shot: bool, scroll_shot: bool, target_pid: libc::pid_t) {
    if SELECTION_WINDOW.with(|w| w.borrow().as_ref().is_some_and(|w| w.isVisible())) {
        return;
    }
    crate::preview::close();
    let app = NSApplication::sharedApplication(mtm);
    // Become a regular app briefly so the overlay can become key and take
    // keyboard events (Escape / any key cancels).
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let screen = match NSScreen::mainScreen(mtm) {
        Some(s) => s,
        None => return,
    };
    let frame = screen.frame();

    let window = SelectionWindow::alloc(mtm).set_ivars(());
    let window: Retained<SelectionWindow> = unsafe {
        msg_send![super(window), initWithContentRect: frame, styleMask: NSWindowStyleMask::Borderless, backing: NSBackingStoreType::Buffered, defer: false]
    };
    window.setTitle(&NSString::from_str(if scroll_shot {
        "选择滚动截图区域"
    } else if shot {
        "选择截图区域"
    } else {
        "选择录制区域"
    }));
    unsafe { window.setReleasedWhenClosed(false) };
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setOpaque(false);
    window.setHasShadow(false);
    window.setSharingType(objc2_app_kit::NSWindowSharingType::None);
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

    for (label, action, slot) in [
        ("取消", sel!(cancelSelection:), &view.ivars().cancel),
        (
            if scroll_shot {
                "开始滚动截图"
            } else if shot {
                "截取"
            } else {
                "确认选区"
            },
            sel!(confirmSelection:),
            &view.ivars().confirm,
        ),
    ] {
        let button = NSButton::initWithFrame(NSButton::alloc(mtm), NSRect::ZERO);
        button.setTitle(&NSString::from_str(label));
        button.setBezelStyle(NSBezelStyle::Rounded);
        unsafe {
            button.setTarget(Some(&*view));
            button.setAction(Some(action));
        }
        button.setHidden(true);
        view.addSubview(&button);
        slot.set(button).ok();
    }
    if shot && !scroll_shot {
        let button = NSButton::initWithFrame(NSButton::alloc(mtm), NSRect::ZERO);
        button.setTitle(&NSString::from_str("截图并标注"));
        button.setBezelStyle(NSBezelStyle::Rounded);
        unsafe {
            button.setTarget(Some(&*view));
            button.setAction(Some(sel!(annotateSelection:)));
        }
        button.setHidden(true);
        view.addSubview(&button);
        view.ivars().annotate.set(button).ok();
    }
    window.makeFirstResponder(Some(&view));
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    NSCursor::crosshairCursor().set();

    // The window owns the view; both stay alive until it is closed.
    SELECTION_WINDOW.with(|slot| *slot.borrow_mut() = Some(window));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_drag_has_the_same_selection() {
        let a = NSPoint::new(320.0, 220.0);
        let b = NSPoint::new(40.0, 60.0);
        assert_eq!(rect_from_points(a, b), rect_from_points(b, a));
        assert_eq!(rect_from_points(a, b).size, NSSize::new(280.0, 160.0));
    }

    #[test]
    fn mask_never_covers_selection_including_screen_edges() {
        let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1000.0, 800.0));
        for selection in [
            bounds,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(250.0, 200.0)),
            NSRect::new(NSPoint::new(600.0, 300.0), NSSize::new(400.0, 500.0)),
        ] {
            let bands = outside_rects(bounds, selection);
            let area: f64 = bands.iter().map(|r| r.size.width * r.size.height).sum();
            assert_eq!(
                area + selection.size.width * selection.size.height,
                800_000.0
            );
            for r in bands {
                assert!(r.size.width >= 0.0 && r.size.height >= 0.0);
                let overlap_w = (r.max().x.min(selection.max().x)
                    - r.origin.x.max(selection.origin.x))
                .max(0.0);
                let overlap_h = (r.max().y.min(selection.max().y)
                    - r.origin.y.max(selection.origin.y))
                .max(0.0);
                assert_eq!(overlap_w * overlap_h, 0.0);
            }
        }
    }
}
