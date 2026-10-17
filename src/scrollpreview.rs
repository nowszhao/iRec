//! Live selection frame and stitched-image preview for scroll capture.
//!
//! Both windows are click-through and excluded from every screen capture, so
//! they can sit above the target app without showing up in the PNG.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{AnyThread, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSBezelStyle, NSBezierPath, NSButton, NSColor, NSFont, NSImage,
    NSImageScaling, NSImageView, NSPanel, NSScreen, NSTextField, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_core_foundation::CFData;
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo,
};
use objc2_foundation::{
    MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use crate::state::{Action, Rect, push_action};

const PANEL_W: f64 = 260.0;
const PANEL_H: f64 = 360.0;
const PREVIEW_W: usize = 236;
const PREVIEW_H: usize = 300;

struct SharedPreview {
    width: usize,
    height: usize,
    rgba: Arc<Vec<u8>>,
    screens: usize,
    version: u64,
}

static SHARED: Mutex<Option<SharedPreview>> = Mutex::new(None);
static VERSION: AtomicU64 = AtomicU64::new(0);

struct Preview {
    border_win: Retained<NSWindow>,
    panel_win: Retained<NSPanel>,
    image_view: Retained<NSImageView>,
    count_label: Retained<NSTextField>,
    _controller: Retained<PreviewController>,
    version: u64,
}

// SAFETY: all AppKit objects are created and accessed on the main thread;
// the worker only touches `SHARED`.
unsafe impl Send for Preview {}

static PREVIEW: Mutex<Option<Preview>> = Mutex::new(None);

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ()]
    struct PreviewController;

    unsafe impl NSObjectProtocol for PreviewController {}

    impl PreviewController {
        #[unsafe(method(previewDone:))]
        fn done(&self, _sender: &AnyObject) {
            push_action(Action::FinishScrollShot);
        }

        #[unsafe(method(previewCancel:))]
        fn cancel(&self, _sender: &AnyObject) {
            push_action(Action::CancelScrollShot);
        }
    }
);

impl PreviewController {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ()]
    struct BorderView;

    impl BorderView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            let bounds = self.bounds();
            let accent = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.62, 0.20, 1.0);
            let border = NSBezierPath::bezierPathWithRect(NSRect::new(
                NSPoint::new(bounds.origin.x + 1.5, bounds.origin.y + 1.5),
                NSSize::new(bounds.size.width - 3.0, bounds.size.height - 3.0),
            ));
            border.setLineWidth(2.0);
            accent.setStroke();
            border.stroke();

            let tick = NSBezierPath::bezierPath();
            tick.setLineWidth(4.0);
            let x0 = bounds.origin.x + 1.5;
            let y0 = bounds.origin.y + 1.5;
            let x1 = x0 + bounds.size.width - 3.0;
            let y1 = y0 + bounds.size.height - 3.0;
            let arm = 18.0f64;
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

impl BorderView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ()]
    struct PanelView;

    impl PanelView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            let panel = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                self.bounds(),
                8.0,
                8.0,
            );
            NSColor::colorWithSRGBRed_green_blue_alpha(0.055, 0.075, 0.10, 0.95).setFill();
            panel.fill();
            panel.setLineWidth(1.0);
            NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.16).setStroke();
            panel.stroke();
        }
    }
);

impl PanelView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ()]
    struct RuleView;

    impl RuleView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.12).setFill();
            NSBezierPath::fillRect(self.bounds());
        }
    }
);

fn make_window(
    mtm: MainThreadMarker,
    frame: NSRect,
    level: isize,
    shadow: bool,
) -> Retained<NSWindow> {
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
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(shadow);
    window.setLevel(level);
    window.setHidesOnDeactivate(false);
    window.setIgnoresMouseEvents(true);
    window.setSharingType(NSWindowSharingType(0));
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary,
    );
    window
}

fn make_panel(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSPanel> {
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(true);
    panel.setLevel(1001);
    panel.setHidesOnDeactivate(false);
    panel.setSharingType(NSWindowSharingType(0));
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary,
    );
    panel
}

fn make_button(
    mtm: MainThreadMarker,
    title: &str,
    frame: NSRect,
    action: Sel,
    target: &PreviewController,
    tint: &NSColor,
    bordered: bool,
) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    button.setTitle(&NSString::from_str(title));
    button.setBezelStyle(NSBezelStyle::Push);
    button.setBordered(bordered);
    button.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    button.setContentTintColor(Some(tint));
    unsafe { button.setAction(Some(action)) };
    unsafe { button.setTarget(Some(target)) };
    button
}

fn make_label(
    mtm: MainThreadMarker,
    frame: NSRect,
    size: f64,
    bold: bool,
    color: &NSColor,
) -> Retained<NSTextField> {
    let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
    field.setBezeled(false);
    field.setDrawsBackground(false);
    field.setEditable(false);
    field.setSelectable(false);
    field.setTextColor(Some(color));
    let font = if bold {
        NSFont::boldSystemFontOfSize(size)
    } else {
        NSFont::systemFontOfSize(size)
    };
    field.setFont(Some(&font));
    field
}

fn panel_frame(mtm: MainThreadMarker, rect: Rect) -> NSRect {
    let screen = NSScreen::mainScreen(mtm)
        .map(|screen| screen.frame())
        .unwrap_or(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(1440.0, 900.0),
        ));
    let gap = 14.0;
    let left = screen.origin.x + 6.0;
    let right = screen.origin.x + screen.size.width - PANEL_W - 6.0;
    let bottom = screen.origin.y + 6.0;
    // Keep the menu bar usable: the user needs it to finish or cancel.
    let top = (screen.origin.y + screen.size.height - PANEL_H - 40.0).max(bottom);
    let selection = NSRect::new(NSPoint::new(rect.x, rect.y), NSSize::new(rect.w, rect.h));
    let candidates = [
        (rect.x + rect.w + gap, rect.y + rect.h - PANEL_H),
        (rect.x - PANEL_W - gap, rect.y + rect.h - PANEL_H),
        (rect.x + rect.w + gap, rect.y),
        (rect.x - PANEL_W - gap, rect.y),
        (rect.x + (rect.w - PANEL_W) * 0.5, rect.y - PANEL_H - gap),
        (rect.x + (rect.w - PANEL_W) * 0.5, rect.y + rect.h + gap),
        (right, bottom),
    ];
    for (x, y) in candidates {
        let frame = NSRect::new(
            NSPoint::new(x.clamp(left, right), y.clamp(bottom, top)),
            NSSize::new(PANEL_W, PANEL_H),
        );
        if !intersects(frame, selection) {
            return frame;
        }
    }
    NSRect::new(NSPoint::new(right, bottom), NSSize::new(PANEL_W, PANEL_H))
}

fn intersects(a: NSRect, b: NSRect) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && a.origin.x + a.size.width > b.origin.x
        && a.origin.y < b.origin.y + b.size.height
        && a.origin.y + a.size.height > b.origin.y
}

pub fn show(mtm: MainThreadMarker, rect: Rect) {
    close(mtm);
    *SHARED.lock().unwrap_or_else(|error| error.into_inner()) = None;
    let controller = PreviewController::new(mtm);

    let border_win = make_window(
        mtm,
        NSRect::new(
            NSPoint::new(rect.x, rect.y),
            NSSize::new(rect.w.max(16.0), rect.h.max(16.0)),
        ),
        1000,
        false,
    );
    let border = BorderView::new(mtm);
    border.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(rect.w.max(16.0), rect.h.max(16.0)),
    ));
    if let Some(content) = border_win.contentView() {
        content.addSubview(&border);
    }

    let frame = panel_frame(mtm, rect);
    let panel_win = make_panel(mtm, frame);
    let panel = PanelView::new(mtm);
    panel.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(PANEL_W, PANEL_H),
    ));
    if let Some(content) = panel_win.contentView() {
        content.addSubview(&panel);
    }

    let title = make_label(
        mtm,
        NSRect::new(
            NSPoint::new(14.0, PANEL_H - 31.0),
            NSSize::new(PANEL_W - 28.0, 20.0),
        ),
        12.0,
        true,
        &NSColor::whiteColor(),
    );
    title.setStringValue(&NSString::from_str("长图预览"));
    panel.addSubview(&title);

    let rule = make_rule(
        mtm,
        NSRect::new(
            NSPoint::new(14.0, PANEL_H - 38.0),
            NSSize::new(PANEL_W - 28.0, 1.0),
        ),
    );
    panel.addSubview(&rule);

    let image_view = NSImageView::initWithFrame(
        NSImageView::alloc(mtm),
        NSRect::new(
            NSPoint::new(14.0, 42.0),
            NSSize::new(PANEL_W - 28.0, PANEL_H - 80.0),
        ),
    );
    image_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    image_view.setAnimates(false);
    panel.addSubview(&image_view);

    let count_label = make_label(
        mtm,
        NSRect::new(NSPoint::new(14.0, 10.0), NSSize::new(PANEL_W - 28.0, 18.0)),
        11.0,
        false,
        &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.62),
    );
    count_label.setStringValue(&NSString::from_str("等待第一屏…"));
    panel.addSubview(&count_label);

    let cancel = make_button(
        mtm,
        "取消",
        NSRect::new(NSPoint::new(112.0, 8.0), NSSize::new(64.0, 26.0)),
        sel!(previewCancel:),
        &controller,
        &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.62),
        false,
    );
    panel.addSubview(&cancel);
    let done = make_button(
        mtm,
        "完成",
        NSRect::new(NSPoint::new(184.0, 8.0), NSSize::new(62.0, 26.0)),
        sel!(previewDone:),
        &controller,
        &NSColor::controlAccentColor(),
        true,
    );
    panel.addSubview(&done);

    border_win.orderFrontRegardless();
    panel_win.orderFrontRegardless();
    let mut guard = PREVIEW.lock().unwrap_or_else(|error| error.into_inner());
    *guard = Some(Preview {
        border_win,
        panel_win,
        image_view,
        count_label,
        _controller: controller,
        version: 0,
    });
}

fn make_rule(mtm: MainThreadMarker, frame: NSRect) -> Retained<RuleView> {
    let view = RuleView::alloc(mtm).set_ivars(());
    let view: Retained<RuleView> = unsafe { msg_send![super(view), init] };
    view.setFrame(frame);
    view
}

pub fn publish(width: usize, height: usize, rgba: &[u8], viewport_px: usize, screens: usize) {
    if width < 2 || height < 2 || rgba.len() < width * height * 4 {
        return;
    }
    let source_h = viewport_px.clamp(2, height);
    let source_y = height - source_h;
    let scale = (PREVIEW_W as f64 / width as f64).min(PREVIEW_H as f64 / source_h as f64);
    let target_w = ((width as f64 * scale).round() as usize).clamp(2, PREVIEW_W);
    let target_h = ((source_h as f64 * scale).round() as usize).clamp(2, PREVIEW_H);
    let mut preview = vec![0u8; target_w * target_h * 4];
    for y in 0..target_h {
        let source_row = source_y + y * source_h / target_h;
        for x in 0..target_w {
            let source_x = x * width / target_w;
            let source = (source_row * width + source_x) * 4;
            let target = (y * target_w + x) * 4;
            preview[target..target + 4].copy_from_slice(&rgba[source..source + 4]);
        }
    }
    let version = VERSION.fetch_add(1, Ordering::AcqRel) + 1;
    let mut guard = SHARED.lock().unwrap_or_else(|error| error.into_inner());
    *guard = Some(SharedPreview {
        width: target_w,
        height: target_h,
        rgba: Arc::new(preview),
        screens,
        version,
    });
}

pub fn sync(_mtm: MainThreadMarker) {
    let shared = {
        let guard = SHARED.lock().unwrap_or_else(|error| error.into_inner());
        let Some(shared) = guard.as_ref() else {
            return;
        };
        SharedPreview {
            width: shared.width,
            height: shared.height,
            rgba: shared.rgba.clone(),
            screens: shared.screens,
            version: shared.version,
        }
    };
    let mut guard = PREVIEW.lock().unwrap_or_else(|error| error.into_inner());
    let Some(preview) = guard.as_mut() else {
        return;
    };
    if preview.version == shared.version {
        return;
    }
    let Some(image) = make_image(&shared) else {
        return;
    };
    preview.image_view.setImage(Some(&image));
    preview
        .count_label
        .setStringValue(&NSString::from_str(&format!(
            "已拼接 {} 屏",
            shared.screens.max(1)
        )));
    preview.version = shared.version;
}

fn make_image(shared: &SharedPreview) -> Option<Retained<NSImage>> {
    let data = unsafe { CFData::new(None, shared.rgba.as_ptr(), shared.rgba.len() as isize) }?;
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let color_space = CGColorSpace::new_device_rgb()?;
    let bitmap_info = CGBitmapInfo::from_bits_retain(
        CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
    );
    let image = unsafe {
        CGImage::new(
            shared.width,
            shared.height,
            8,
            32,
            shared.width * 4,
            Some(&color_space),
            bitmap_info,
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }?;
    Some(NSImage::initWithCGImage_size(
        NSImage::alloc(),
        &image,
        NSSize::new(shared.width as f64, shared.height as f64),
    ))
}

pub fn close(_mtm: MainThreadMarker) {
    let mut guard = PREVIEW.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(preview) = guard.take() {
        preview.border_win.orderOut(None);
        preview.panel_win.orderOut(None);
        preview.border_win.close();
        preview.panel_win.close();
    }
    *SHARED.lock().unwrap_or_else(|error| error.into_inner()) = None;
}
