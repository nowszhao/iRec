//! Live selection frame and stitched-image preview for scroll capture.
//!
//! Both windows are click-through and excluded from every screen capture, so
//! they can sit above the target app without showing up in the PNG.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSBezelStyle, NSBezierPath, NSBorderType, NSButton, NSColor, NSEvent,
    NSFont, NSImage, NSImageScaling, NSImageView, NSPanel, NSScreen, NSScrollView, NSScrollerStyle,
    NSTextField, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowSharingType,
    NSWindowStyleMask,
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
const CROP_SCREEN_FRACTION: f64 = 0.9;
const PREVIEW_W: usize = 236;
const PREVIEW_H: usize = 300;
const MIN_CROP_HEIGHT: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CropRange {
    top: usize,
    bottom: usize,
}

impl CropRange {
    fn full() -> Self {
        Self { top: 0, bottom: 0 }
    }

    fn drag_top(&mut self, candidate: usize, height: usize, minimum: usize) {
        self.top = candidate.min(height.saturating_sub(self.bottom).saturating_sub(minimum));
    }

    fn drag_bottom(&mut self, candidate: usize, height: usize, minimum: usize) {
        self.bottom = candidate.min(height.saturating_sub(self.top).saturating_sub(minimum));
    }
}

struct CropImage {
    width: usize,
    height: usize,
    pixels_per_point: f64,
    rgba: Arc<Vec<u8>>,
    range: CropRange,
    output_dir: std::path::PathBuf,
}

static CROP_IMAGE: Mutex<Option<CropImage>> = Mutex::new(None);

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
    panel_view: Retained<PanelView>,
    image_view: Retained<NSImageView>,
    crop_scroll_view: Retained<NSScrollView>,
    crop_view: Retained<CropView>,
    title_label: Retained<NSTextField>,
    rule_view: Retained<RuleView>,
    cancel_button: Retained<NSButton>,
    done_button: Retained<NSButton>,
    count_label: Retained<NSTextField>,
    _controller: Retained<PreviewController>,
    version: u64,
    crop_mode: bool,
    crop_saving: bool,
}

// SAFETY: all AppKit objects are created and accessed on the main thread;
// the worker only touches `SHARED`.
unsafe impl Send for Preview {}

static PREVIEW: Mutex<Option<Preview>> = Mutex::new(None);

#[derive(Clone, Copy)]
enum CropEdge {
    Top,
    Bottom,
}

#[derive(Default)]
struct CropViewIvars {
    drag: Cell<Option<CropEdge>>,
    image: std::cell::RefCell<Option<Retained<NSImage>>>,
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = CropViewIvars]
    struct CropView;

    impl CropView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool { false }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let Some((image_rect, range, height)) = self.crop_geometry() else { return; };
            let top_y = image_rect.origin.y + image_rect.size.height * (1.0 - range.top as f64 / height as f64);
            let bottom_y = image_rect.origin.y + image_rect.size.height * range.bottom as f64 / height as f64;
            let top_distance = (point.y - top_y).abs();
            let bottom_distance = (point.y - bottom_y).abs();
            if top_distance.min(bottom_distance) > 16.0 {
                return;
            }
            self.ivars().drag.set(Some(if top_distance <= bottom_distance { CropEdge::Top } else { CropEdge::Bottom }));
            self.drag_to(point.y);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            self.drag_to(point.y);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) { self.ivars().drag.set(None); }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            let Some(image) = self.ivars().image.borrow().as_ref().cloned() else {
                return;
            };
            self.draw_crop_overlay(&image);
        }
    }
);

impl CropView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CropViewIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn set_crop_image(&self, image: Retained<NSImage>) {
        *self.ivars().image.borrow_mut() = Some(image);
    }

    fn crop_geometry(&self) -> Option<(NSRect, CropRange, usize)> {
        let guard = CROP_IMAGE.lock().unwrap_or_else(|error| error.into_inner());
        let crop = guard.as_ref()?;
        // Map source pixels back to the logical points used during capture.
        // The crop image itself is never downsampled; Retina displays therefore
        // show the source at its native capture density.
        let pixels_per_point = crop.pixels_per_point.max(1.0);
        let image_rect = NSRect::new(
            NSPoint::new(0.0, 0.0),
            crop_image_size_points(crop.width, crop.height, pixels_per_point),
        );
        Some((image_rect, crop.range, crop.height))
    }

    fn draw_crop_overlay(&self, image: &NSImage) {
        let Some((image_rect, range, height)) = self.crop_geometry() else {
            return;
        };
        if height == 0 {
            return;
        }
        image.drawInRect(image_rect);
        let (top_y, bottom_y) = crop_overlay_geometry(image_rect, range, height);
        NSColor::colorWithSRGBRed_green_blue_alpha(0.02, 0.03, 0.05, 0.72).setFill();
        NSBezierPath::fillRect(NSRect::new(
            NSPoint::new(image_rect.origin.x, image_rect.origin.y),
            NSSize::new(
                image_rect.size.width,
                (bottom_y - image_rect.origin.y).max(0.0),
            ),
        ));
        NSBezierPath::fillRect(NSRect::new(
            NSPoint::new(image_rect.origin.x, top_y),
            NSSize::new(
                image_rect.size.width,
                (image_rect.origin.y + image_rect.size.height - top_y).max(0.0),
            ),
        ));
        let lines = NSBezierPath::bezierPath();
        lines.setLineWidth(2.0);
        NSColor::controlAccentColor().setStroke();
        for y in [top_y, bottom_y] {
            lines.moveToPoint(NSPoint::new(image_rect.origin.x, y));
            lines.lineToPoint(NSPoint::new(image_rect.origin.x + image_rect.size.width, y));
        }
        lines.stroke();
        NSColor::controlAccentColor().setFill();
        for y in [top_y, bottom_y] {
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(
                    image_rect.origin.x + (image_rect.size.width - 32.0) / 2.0,
                    y - 3.0,
                ),
                NSSize::new(32.0, 6.0),
            ));
        }
    }

    fn drag_to(&self, y: f64) {
        let Some((image_rect, _, height)) = self.crop_geometry() else {
            return;
        };
        if height == 0 {
            return;
        }
        let unit = ((y - image_rect.origin.y) / image_rect.size.height.max(1.0)).clamp(0.0, 1.0);
        let row_from_bottom = (unit * height as f64).round() as usize;
        let minimum = MIN_CROP_HEIGHT.min(height);
        let mut guard = CROP_IMAGE.lock().unwrap_or_else(|error| error.into_inner());
        let Some(crop) = guard.as_mut() else {
            return;
        };
        match self.ivars().drag.get() {
            Some(CropEdge::Top) => {
                crop.range
                    .drag_top(height.saturating_sub(row_from_bottom), height, minimum)
            }
            Some(CropEdge::Bottom) => crop.range.drag_bottom(row_from_bottom, height, minimum),
            None => return,
        }
        drop(guard);
        self.setNeedsDisplay(true);
    }
}

fn crop_action_mode() -> Option<bool> {
    PREVIEW
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .filter(|preview| preview.crop_mode)
        .map(|preview| preview.crop_saving)
}

fn crop_overlay_geometry(image_rect: NSRect, range: CropRange, height: usize) -> (f64, f64) {
    if height == 0 {
        return (image_rect.origin.y, image_rect.origin.y);
    }
    let top_y =
        image_rect.origin.y + image_rect.size.height * (1.0 - range.top as f64 / height as f64);
    let bottom_y =
        image_rect.origin.y + image_rect.size.height * range.bottom as f64 / height as f64;
    (top_y, bottom_y)
}

fn crop_image_size_points(width: usize, height: usize, pixels_per_point: f64) -> NSSize {
    let density = pixels_per_point.max(1.0);
    NSSize::new(width as f64 / density, height as f64 / density)
}

/// Publish the completed stitched image for crop adjustment.
pub fn publish_crop_image(
    width: usize,
    height: usize,
    pixels_per_point: f64,
    rgba: Vec<u8>,
    output_dir: std::path::PathBuf,
) -> Result<(), String> {
    if width < 2 || height < 2 || rgba.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err("长图尺寸无效，无法打开裁剪预览".to_string());
    }
    *CROP_IMAGE.lock().unwrap_or_else(|error| error.into_inner()) = Some(CropImage {
        width,
        height,
        pixels_per_point: pixels_per_point.clamp(1.0, 2.0),
        rgba: Arc::new(rgba),
        range: CropRange::full(),
        output_dir,
    });
    Ok(())
}

/// Return a copy of the selected rows in the stitched image.
pub fn cropped_rgba() -> Result<(usize, usize, Vec<u8>), String> {
    let guard = CROP_IMAGE.lock().unwrap_or_else(|error| error.into_inner());
    let crop = guard
        .as_ref()
        .ok_or_else(|| "长图裁剪数据已失效".to_string())?;
    crop_rgba_rows(crop.width, crop.height, &crop.rgba, crop.range)
}

pub fn crop_output_dir() -> Option<std::path::PathBuf> {
    CROP_IMAGE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|crop| crop.output_dir.clone())
}

fn crop_rgba_rows(
    width: usize,
    height: usize,
    rgba: &[u8],
    range: CropRange,
) -> Result<(usize, usize, Vec<u8>), String> {
    if width < 2 || rgba.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err("长图裁剪数据无效".to_string());
    }
    let top = range.top.min(height);
    let bottom = range.bottom.min(height.saturating_sub(top));
    let cropped_height = height.saturating_sub(top + bottom);
    if cropped_height < 2 {
        return Err("裁剪范围太小".to_string());
    }
    let start = top * width * 4;
    let end = (top + cropped_height) * width * 4;
    Ok((width, cropped_height, rgba[start..end].to_vec()))
}

pub fn show_crop(mtm: MainThreadMarker) -> bool {
    let Some((width, height, pixels_per_point, rgba)) = CROP_IMAGE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|crop| {
            (
                crop.width,
                crop.height,
                crop.pixels_per_point.max(1.0),
                crop.rgba.clone(),
            )
        })
    else {
        return false;
    };
    let Some(image) = make_rgba_image_with_size(
        width,
        height,
        &rgba,
        crop_image_size_points(width, height, pixels_per_point),
    ) else {
        return false;
    };
    let mut guard = PREVIEW.lock().unwrap_or_else(|error| error.into_inner());
    let Some(preview) = guard.as_mut() else {
        return false;
    };
    let Some(screen) = preview
        .panel_win
        .screen()
        .or_else(|| NSScreen::mainScreen(mtm))
    else {
        return false;
    };
    let visible = screen.visibleFrame();
    let crop_width = (visible.size.width * CROP_SCREEN_FRACTION).min(1440.0);
    let crop_height = (visible.size.height * CROP_SCREEN_FRACTION).min(980.0);
    let crop_frame = NSRect::new(
        NSPoint::new(
            visible.origin.x + (visible.size.width - crop_width) / 2.0,
            visible.origin.y + (visible.size.height - crop_height) / 2.0,
        ),
        NSSize::new(crop_width, crop_height),
    );
    preview.panel_win.setFrame_display(crop_frame, true);
    preview.panel_view.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(crop_width, crop_height),
    ));
    preview.title_label.setFrame(NSRect::new(
        NSPoint::new(18.0, crop_height - 34.0),
        NSSize::new(crop_width - 36.0, 22.0),
    ));
    preview.rule_view.setFrame(NSRect::new(
        NSPoint::new(18.0, crop_height - 42.0),
        NSSize::new(crop_width - 36.0, 1.0),
    ));
    preview.crop_scroll_view.setFrame(NSRect::new(
        NSPoint::new(14.0, 50.0),
        NSSize::new(crop_width - 28.0, crop_height - 100.0),
    ));
    preview.crop_view.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        crop_image_size_points(width, height, pixels_per_point),
    ));
    preview.crop_view.set_crop_image(image);
    preview
        .crop_scroll_view
        .setDocumentView(Some(&preview.crop_view));
    preview.crop_scroll_view.setHasVerticalScroller(true);
    preview.crop_scroll_view.setHasHorizontalScroller(true);
    preview.crop_scroll_view.setAutohidesScrollers(true);
    preview
        .crop_scroll_view
        .setBorderType(NSBorderType::LineBorder);
    preview
        .crop_scroll_view
        .setScrollerStyle(NSScrollerStyle::Overlay);
    let clip = preview.crop_scroll_view.contentView();
    let document_height = height as f64 / pixels_per_point;
    let top_y = (document_height - clip.bounds().size.height).max(0.0);
    clip.scrollToPoint(NSPoint::new(0.0, top_y));
    preview.crop_scroll_view.reflectScrolledClipView(&clip);
    preview
        .title_label
        .setStringValue(&NSString::from_str("长图裁剪 · 高清预览"));
    preview.count_label.setFrame(NSRect::new(
        NSPoint::new(18.0, 14.0),
        NSSize::new(crop_width - 230.0, 20.0),
    ));
    preview.cancel_button.setFrame(NSRect::new(
        NSPoint::new(crop_width - 174.0, 10.0),
        NSSize::new(70.0, 28.0),
    ));
    preview.done_button.setFrame(NSRect::new(
        NSPoint::new(crop_width - 94.0, 10.0),
        NSSize::new(78.0, 28.0),
    ));
    preview.crop_mode = true;
    preview.image_view.setHidden(true);
    preview.crop_scroll_view.setHidden(false);
    preview.crop_view.setNeedsDisplay(true);
    preview.count_label.setStringValue(&NSString::from_str(
        "高清原图 · 滚动查看，拖动蓝色上下边界裁剪",
    ));
    preview.cancel_button.setTitle(&NSString::from_str("放弃"));
    preview.done_button.setTitle(&NSString::from_str("保存"));
    preview.crop_saving = false;
    preview.done_button.setEnabled(true);
    preview.cancel_button.setEnabled(true);
    preview.border_win.orderOut(None);
    preview.panel_win.orderFrontRegardless();
    true
}

pub fn clear_crop_image() {
    *CROP_IMAGE.lock().unwrap_or_else(|error| error.into_inner()) = None;
}

pub fn set_crop_saving(saving: bool) {
    if let Some(preview) = PREVIEW
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_mut()
    {
        preview.crop_saving = saving;
        preview.done_button.setEnabled(!saving);
        preview.cancel_button.setEnabled(!saving);
        preview
            .count_label
            .setStringValue(&NSString::from_str(if saving {
                "正在保存长图…"
            } else {
                "保存失败，可重试或放弃"
            }));
    }
}

pub fn crop_is_saving() -> bool {
    crop_action_mode() == Some(true)
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ()]
    struct PreviewController;

    unsafe impl NSObjectProtocol for PreviewController {}

    impl PreviewController {
        #[unsafe(method(previewDone:))]
        fn done(&self, _sender: &AnyObject) {
            match crop_action_mode() {
                Some(true) => return,
                Some(false) => push_action(Action::ConfirmScrollShotCrop),
                None => push_action(Action::FinishScrollShot),
            }
        }

        #[unsafe(method(previewCancel:))]
        fn cancel(&self, _sender: &AnyObject) {
            if crop_action_mode().is_some() {
                push_action(Action::DiscardScrollShotCrop);
            } else {
                push_action(Action::CancelScrollShot);
            }
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
            NSColor::windowBackgroundColor().setFill();
            panel.fill();
            panel.setLineWidth(1.0);
            NSColor::separatorColor().setStroke();
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
            NSColor::separatorColor().setFill();
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
    clear_crop_image();
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
        &NSColor::labelColor(),
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

    let crop_scroll_view = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        NSRect::new(
            NSPoint::new(14.0, 42.0),
            NSSize::new(PANEL_W - 28.0, PANEL_H - 80.0),
        ),
    );
    crop_scroll_view.setHasVerticalScroller(true);
    crop_scroll_view.setHasHorizontalScroller(true);
    crop_scroll_view.setAutohidesScrollers(true);
    crop_scroll_view.setBorderType(NSBorderType::LineBorder);
    crop_scroll_view.setScrollerStyle(NSScrollerStyle::Overlay);
    crop_scroll_view.setHidden(true);
    panel.addSubview(&crop_scroll_view);

    let crop_view = CropView::new(mtm);
    crop_view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)));

    let count_label = make_label(
        mtm,
        NSRect::new(NSPoint::new(14.0, 10.0), NSSize::new(PANEL_W - 28.0, 18.0)),
        11.0,
        false,
        &NSColor::secondaryLabelColor(),
    );
    count_label.setStringValue(&NSString::from_str("等待第一屏…"));
    panel.addSubview(&count_label);

    let cancel = make_button(
        mtm,
        "取消",
        NSRect::new(NSPoint::new(112.0, 8.0), NSSize::new(64.0, 26.0)),
        sel!(previewCancel:),
        &controller,
        &NSColor::secondaryLabelColor(),
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
        panel_view: panel,
        image_view,
        crop_scroll_view,
        crop_view,
        title_label: title,
        rule_view: rule,
        cancel_button: cancel,
        done_button: done,
        count_label,
        _controller: controller,
        version: 0,
        crop_mode: false,
        crop_saving: false,
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
    if preview.crop_mode {
        return;
    }
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
    make_rgba_image(shared.width, shared.height, &shared.rgba)
}

fn make_rgba_image(width: usize, height: usize, rgba: &[u8]) -> Option<Retained<NSImage>> {
    make_rgba_image_with_size(
        width,
        height,
        rgba,
        NSSize::new(width as f64, height as f64),
    )
}

fn make_rgba_image_with_size(
    width: usize,
    height: usize,
    rgba: &[u8],
    image_size: NSSize,
) -> Option<Retained<NSImage>> {
    let data = unsafe { CFData::new(None, rgba.as_ptr(), rgba.len() as isize) }?;
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let color_space = CGColorSpace::new_device_rgb()?;
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
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }?;
    Some(NSImage::initWithCGImage_size(
        NSImage::alloc(),
        &image,
        image_size,
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
    clear_crop_image();
}

#[cfg(test)]
mod crop_tests {
    use super::{
        CropRange, MIN_CROP_HEIGHT, crop_image_size_points, crop_overlay_geometry, crop_rgba_rows,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    #[test]
    fn dragging_crop_edges_keeps_a_valid_selection() {
        let mut crop = CropRange::full();
        crop.drag_top(80, 100, MIN_CROP_HEIGHT.min(100));
        assert_eq!(crop.top, 80);
        crop.drag_bottom(90, 100, MIN_CROP_HEIGHT.min(100));
        assert_eq!(crop.bottom, 18);
        assert!(crop.top + crop.bottom + MIN_CROP_HEIGHT <= 100);
    }

    #[test]
    fn crop_range_preserves_untrimmed_edges_by_default() {
        let crop = CropRange::full();
        assert_eq!(crop, CropRange { top: 0, bottom: 0 });
    }

    #[test]
    fn crop_edges_map_to_the_matching_image_boundaries() {
        let image_rect = NSRect::new(NSPoint::new(10.0, 20.0), NSSize::new(200.0, 300.0));
        let (top, bottom) = crop_overlay_geometry(
            image_rect,
            CropRange {
                top: 25,
                bottom: 10,
            },
            100,
        );
        assert!((top - 245.0).abs() < 0.01);
        assert!((bottom - 50.0).abs() < 0.01);
    }

    #[test]
    fn high_resolution_preview_maps_source_pixels_to_capture_points() {
        let size = crop_image_size_points(2400, 12000, 2.0);
        assert_eq!(size.width, 1200.0);
        assert_eq!(size.height, 6000.0);
    }

    #[test]
    fn cropping_uses_the_selected_pixel_rows() {
        let width = 2;
        let height = 5;
        let rgba: Vec<u8> = (0..height)
            .flat_map(|row| std::iter::repeat_n(row as u8, width * 4))
            .collect();
        let (cropped_width, cropped_height, cropped) =
            crop_rgba_rows(width, height, &rgba, CropRange { top: 1, bottom: 2 }).unwrap();
        assert_eq!((cropped_width, cropped_height), (2, 2));
        assert!(cropped.iter().all(|pixel| *pixel == 1 || *pixel == 2));
    }
}
