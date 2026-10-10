//! 标注：截完图直接在 iRec 里画箭头、框图、文字，然后写回原文件。
//!
//! 系统自带的 Markup 是注册在分享服务里的（`com.apple.MarkupUI.Markup`），
//! 但在不少机器上它并不出现在可用服务里、也拉不起窗口，所以这里自己做。
//! 形状坐标一律用「图片像素、y 朝下」，画的时候再翻一次。

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, ClassType, define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezierPath,
    NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSButton, NSColor, NSEvent, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSImage, NSPNGFileType, NSScreen,
    NSStringDrawing, NSTextField, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapInfo, CGColorSpace, CGContext,
    CGContextScaleCTM, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{
    MainThreadMarker, NSMutableDictionary, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString,
};

use crate::state::log;

/// 标注用的红，和 macOS 标记工具里那个接近。
const INK: (f64, f64, f64) = (1.0, 0.23, 0.19);
const LINE_W: f64 = 3.0;
/// 工具栏高度。
const BAR_H: f64 = 44.0;

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Arrow,
    Rect,
    Text,
}

#[derive(Clone)]
enum Shape {
    /// 图片像素坐标，y 朝下。
    Arrow {
        from: (f64, f64),
        to: (f64, f64),
    },
    Rect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    },
    Text {
        x: f64,
        y: f64,
        size: f64,
        text: String,
    },
}

struct CanvasIvars {
    image: RefCell<Option<Retained<NSImage>>>,
    shapes: RefCell<Vec<Shape>>,
    tool: Cell<Tool>,
    /// 正在拖的形状（起点、当前点），图片坐标。
    drag: Cell<Option<((f64, f64), (f64, f64))>>,
    path: RefCell<Option<PathBuf>>,
    window: RefCell<Option<Retained<NSWindow>>>,
    field: RefCell<Option<Retained<NSTextField>>>,
    /// 文字输入框对应的图片坐标。
    field_at: Cell<(f64, f64)>,
    on_done: RefCell<Option<Box<dyn FnOnce(Option<PathBuf>)>>>,
}

impl Default for CanvasIvars {
    fn default() -> Self {
        Self {
            image: RefCell::new(None),
            shapes: RefCell::new(Vec::new()),
            tool: Cell::new(Tool::Arrow),
            drag: Cell::new(None),
            path: RefCell::new(None),
            window: RefCell::new(None),
            field: RefCell::new(None),
            field_at: Cell::new((0.0, 0.0)),
            on_done: RefCell::new(None),
        }
    }
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = CanvasIvars]
    struct CanvasView;

    unsafe impl NSObjectProtocol for CanvasView {}

    unsafe impl NSWindowDelegate for CanvasView {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &AnyObject) {
            self.finish(None);
        }
    }

    impl CanvasView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            self.paint();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.to_image(self.convert(event.locationInWindow()));
            if self.ivars().tool.get() == Tool::Text {
                self.begin_text(p);
                return;
            }
            self.ivars().drag.set(Some((p, p)));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let p = self.to_image(self.convert(event.locationInWindow()));
            if let Some((from, _)) = self.ivars().drag.get() {
                self.ivars().drag.set(Some((from, p)));
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            let Some((from, to)) = self.ivars().drag.take() else {
                return;
            };
            if (from.0 - to.0).abs() < 4.0 && (from.1 - to.1).abs() < 4.0 {
                self.setNeedsDisplay(true);
                return;
            }
            let shape = match self.ivars().tool.get() {
                Tool::Arrow => Shape::Arrow { from, to },
                Tool::Rect => Shape::Rect {
                    x: from.0.min(to.0),
                    y: from.1.min(to.1),
                    w: (from.0 - to.0).abs(),
                    h: (from.1 - to.1).abs(),
                },
                Tool::Text => return,
            };
            self.ivars().shapes.borrow_mut().push(shape);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            // Esc 放弃；⌘Z 撤销最后一个。
            if event.keyCode() == 53 {
                self.finish(None);
            } else if event
                .modifierFlags()
                .contains(objc2_app_kit::NSEventModifierFlags::Command)
                && event
                    .charactersIgnoringModifiers()
                    .map(|c| c.to_string().eq_ignore_ascii_case("z"))
                    .unwrap_or(false)
            {
                self.ivars().shapes.borrow_mut().pop();
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(pickArrow:))]
        fn pick_arrow(&self, _sender: &AnyObject) {
            self.ivars().tool.set(Tool::Arrow);
        }

        #[unsafe(method(pickRect:))]
        fn pick_rect(&self, _sender: &AnyObject) {
            self.ivars().tool.set(Tool::Rect);
        }

        #[unsafe(method(pickText:))]
        fn pick_text(&self, _sender: &AnyObject) {
            self.ivars().tool.set(Tool::Text);
        }

        #[unsafe(method(undoShape:))]
        fn undo_shape(&self, _sender: &AnyObject) {
            self.ivars().shapes.borrow_mut().pop();
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(commitText:))]
        fn commit_text(&self, _sender: &AnyObject) {
            self.commit_text_field();
        }

        #[unsafe(method(doneEdit:))]
        fn done_edit(&self, _sender: &AnyObject) {
            self.save();
        }

        #[unsafe(method(cancelEdit:))]
        fn cancel_edit(&self, _sender: &AnyObject) {
            self.finish(None);
        }
    }
);

impl CanvasView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CanvasIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn convert(&self, window_point: NSPoint) -> NSPoint {
        self.convertPoint_fromView(window_point, None)
    }

    /// 图片在视图里的位置（按比例居中）。
    fn image_rect(&self) -> NSRect {
        let bounds = self.bounds();
        let size = self
            .ivars()
            .image
            .borrow()
            .as_ref()
            .map(|image| image.size())
            .unwrap_or(NSSize::new(1.0, 1.0));
        let scale = (bounds.size.width / size.width).min(bounds.size.height / size.height);
        let w = size.width * scale;
        let h = size.height * scale;
        NSRect::new(
            NSPoint::new((bounds.size.width - w) / 2.0, (bounds.size.height - h) / 2.0),
            NSSize::new(w, h),
        )
    }

    fn to_image(&self, point: NSPoint) -> (f64, f64) {
        let r = self.image_rect();
        let size = self
            .ivars()
            .image
            .borrow()
            .as_ref()
            .map(|image| image.size())
            .unwrap_or(NSSize::new(1.0, 1.0));
        let sx = if r.size.width > 0.0 {
            size.width / r.size.width
        } else {
            1.0
        };
        let sy = if r.size.height > 0.0 {
            size.height / r.size.height
        } else {
            1.0
        };
        ((point.x - r.origin.x) * sx, (point.y - r.origin.y) * sy)
    }

    fn to_view(&self, p: (f64, f64)) -> NSPoint {
        let r = self.image_rect();
        let size = self
            .ivars()
            .image
            .borrow()
            .as_ref()
            .map(|image| image.size())
            .unwrap_or(NSSize::new(1.0, 1.0));
        let sx = if size.width > 0.0 {
            r.size.width / size.width
        } else {
            1.0
        };
        let sy = if size.height > 0.0 {
            r.size.height / size.height
        } else {
            1.0
        };
        NSPoint::new(r.origin.x + p.0 * sx, r.origin.y + p.1 * sy)
    }

    fn paint(&self) {
        let bounds = self.bounds();
        NSColor::colorWithSRGBRed_green_blue_alpha(0.13, 0.13, 0.14, 1.0).setFill();
        unsafe { objc2_app_kit::NSBezierPath::fillRect(bounds) };
        let r = self.image_rect();
        if let Some(image) = self.ivars().image.borrow().as_ref() {
            image.drawInRect(r);
        }
        let shapes = self.ivars().shapes.borrow().clone();
        for shape in shapes.iter() {
            self.paint_shape(shape);
        }
        if let Some((from, to)) = self.ivars().drag.get() {
            match self.ivars().tool.get() {
                Tool::Arrow => self.paint_shape(&Shape::Arrow { from, to }),
                Tool::Rect => self.paint_shape(&Shape::Rect {
                    x: from.0.min(to.0),
                    y: from.1.min(to.1),
                    w: (from.0 - to.0).abs(),
                    h: (from.1 - to.1).abs(),
                }),
                Tool::Text => {}
            }
        }
    }

    fn paint_shape(&self, shape: &Shape) {
        let r = self.image_rect();
        let size = self
            .ivars()
            .image
            .borrow()
            .as_ref()
            .map(|image| image.size())
            .unwrap_or(NSSize::new(1.0, 1.0));
        let scale = (r.size.width / size.width).min(r.size.height / size.height);
        NSColor::colorWithSRGBRed_green_blue_alpha(INK.0, INK.1, INK.2, 1.0).setStroke();
        NSColor::colorWithSRGBRed_green_blue_alpha(INK.0, INK.1, INK.2, 1.0).setFill();
        match shape {
            Shape::Arrow { from, to } => {
                let a = self.to_view(*from);
                let b = self.to_view(*to);
                let path = NSBezierPath::bezierPath();
                path.setLineWidth(LINE_W);
                path.moveToPoint(a);
                path.lineToPoint(b);
                path.stroke();
                // 箭头
                let angle = (b.y - a.y).atan2(b.x - a.x);
                let len = 12.0;
                let spread = 0.42;
                for side in [-1.0f64, 1.0f64] {
                    let t = angle + std::f64::consts::PI + side * spread;
                    let wing = NSBezierPath::bezierPath();
                    wing.setLineWidth(LINE_W);
                    wing.moveToPoint(b);
                    wing.lineToPoint(NSPoint::new(b.x + t.cos() * len, b.y + t.sin() * len));
                    wing.stroke();
                }
            }
            Shape::Rect { x, y, w, h } => {
                let origin = self.to_view((*x, *y));
                let rect = NSRect::new(
                    NSPoint::new(origin.x, origin.y),
                    NSSize::new(w * scale, h * scale),
                );
                let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, 2.0, 2.0);
                path.setLineWidth(LINE_W);
                path.stroke();
            }
            Shape::Text { x, y, size, text } => {
                let origin = self.to_view((*x, *y));
                let font = NSFont::systemFontOfSize(size * scale);
                let attrs: Retained<NSMutableDictionary<NSString, AnyObject>> =
                    NSMutableDictionary::new();
                unsafe {
                    let object: &AnyObject = &*font;
                    attrs.setObject_forKey(object, ProtocolObject::from_ref(NSFontAttributeName));
                    let color =
                        NSColor::colorWithSRGBRed_green_blue_alpha(INK.0, INK.1, INK.2, 1.0);
                    let object: &AnyObject = &*color;
                    attrs.setObject_forKey(
                        object,
                        ProtocolObject::from_ref(NSForegroundColorAttributeName),
                    );
                }
                let string = NSString::from_str(text);
                unsafe {
                    string.drawAtPoint_withAttributes(origin, Some(&attrs));
                }
            }
        }
    }

    fn begin_text(&self, at: (f64, f64)) {
        self.commit_text_field();
        let mtm = self.mtm();
        let view_point = self.to_view(at);
        let field = unsafe {
            NSTextField::initWithFrame(
                NSTextField::alloc(mtm),
                NSRect::new(
                    NSPoint::new(view_point.x, view_point.y),
                    NSSize::new(200.0, 30.0),
                ),
            )
        };
        field.setFont(Some(&NSFont::systemFontOfSize(18.0)));
        field.setTextColor(Some(
            &NSColor::colorWithSRGBRed_green_blue_alpha(INK.0, INK.1, INK.2, 1.0),
        ));
        field.setDrawsBackground(true);
        field.setBackgroundColor(Some(
            &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.85),
        ));
        field.setBezeled(false);
        field.setPlaceholderString(Some(&NSString::from_str("输入文字，回车确认")));
        unsafe { field.setAction(Some(sel!(commitText:))) };
        unsafe { field.setTarget(Some(self)) };
        self.addSubview(&field);
        unsafe { self.window().map(|w| w.makeFirstResponder(Some(&field))) };
        *self.ivars().field.borrow_mut() = Some(field);
        self.ivars().field_at.set(at);
    }

    fn commit_text_field(&self) {
        let Some(field) = self.ivars().field.borrow_mut().take() else {
            return;
        };
        let text = field.stringValue().to_string();
        field.removeFromSuperview();
        if text.trim().is_empty() {
            self.setNeedsDisplay(true);
            return;
        }
        // 屏幕上的 18pt 换成图片里的像素。
        let r = self.image_rect();
        let size = self
            .ivars()
            .image
            .borrow()
            .as_ref()
            .map(|image| image.size())
            .unwrap_or(NSSize::new(1.0, 1.0));
        let scale = if r.size.width > 0.0 {
            size.width / r.size.width
        } else {
            1.0
        };
        let at = self.ivars().field_at.get();
        self.ivars().shapes.borrow_mut().push(Shape::Text {
            x: at.0,
            y: at.1,
            size: 18.0 * scale,
            text,
        });
        self.setNeedsDisplay(true);
    }

    /// 把形状和原图合成一张新图，覆盖写回原文件。
    fn save(&self) {
        self.commit_text_field();
        let Some(image) = self.ivars().image.borrow().clone() else {
            self.finish(None);
            return;
        };
        let Some(path) = self.ivars().path.borrow().clone() else {
            self.finish(None);
            return;
        };
        let size = image.size();
        let shapes = self.ivars().shapes.borrow().clone();
        match compose(&image, &shapes) {
            Some(cg) => match write_png_cg(&cg, &path) {
                Ok(()) => {
                    log(&format!("markup: 已保存 {}", path.display()));
                    self.finish(Some(path));
                }
                Err(err) => {
                    log(&format!("markup: 保存失败 {err}"));
                    self.finish(None);
                }
            },
            None => {
                log("markup: 合成失败");
                self.finish(None);
            }
        }
        let _ = size;
    }

    /// 收尾：关窗口、回调、把应用变回菜单栏常驻。
    fn finish(&self, saved: Option<PathBuf>) {
        if let Some(window) = self.ivars().window.borrow_mut().take() {
            window.orderOut(None);
        }
        let on_done = self.ivars().on_done.borrow_mut().take();
        if let Some(on_done) = on_done {
            on_done(saved);
        }
        let mtm = self.mtm();
        NSApplication::sharedApplication(mtm)
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

/// 把标注画进原图的像素里，尺寸和原图一致。
fn compose(image: &NSImage, shapes: &[Shape]) -> Option<objc2_core_foundation::CFRetained<CGImage>> {
    let source: Retained<CGImage> = unsafe {
        image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None)
    }?;
    let w = CGImage::width(Some(&source));
    let h = CGImage::height(Some(&source));
    if w == 0 || h == 0 {
        return None;
    }
    let space = CGColorSpace::new_device_rgb()?;
    let info = CGBitmapInfo::ByteOrder32Big.bits() | CGImageAlphaInfo::PremultipliedLast.0;
    let ctx = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            w,
            h,
            8,
            0,
            Some(&space),
            info,
        )
    }?;
    // 形状用的是「点」坐标，这里缩放到像素。
    let size = image.size();
    let sx = if size.width > 0.0 {
        w as f64 / size.width
    } else {
        1.0
    };
    let sy = if size.height > 0.0 {
        h as f64 / size.height
    } else {
        1.0
    };
    let ns = unsafe { objc2_app_kit::NSGraphicsContext::graphicsContextWithCGContext_flipped(&ctx, false) };
    objc2_app_kit::NSGraphicsContext::saveGraphicsState_class();
    objc2_app_kit::NSGraphicsContext::setCurrentContext(Some(&ns));
    image.drawInRect(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(w as f64, h as f64),
    ));
    unsafe { CGContextScaleCTM(Some(&ctx), sx, sy) };
    paint_into(shapes, size);
    objc2_app_kit::NSGraphicsContext::restoreGraphicsState_class();
    unsafe { CGBitmapContextCreateImage(Some(&ctx)) }
}

/// 在图片自己的坐标系里画（上下文 y 朝上，形状 y 朝下）。
fn paint_into(shapes: &[Shape], size: NSSize) {
    NSColor::colorWithSRGBRed_green_blue_alpha(INK.0, INK.1, INK.2, 1.0).setStroke();
    NSColor::colorWithSRGBRed_green_blue_alpha(INK.0, INK.1, INK.2, 1.0).setFill();
    let flip = |p: (f64, f64)| NSPoint::new(p.0, size.height - p.1);
    for shape in shapes {
        match shape {
            Shape::Arrow { from, to } => {
                let a = flip(*from);
                let b = flip(*to);
                let path = NSBezierPath::bezierPath();
                path.setLineWidth(LINE_W);
                path.moveToPoint(a);
                path.lineToPoint(b);
                path.stroke();
                let angle = (b.y - a.y).atan2(b.x - a.x);
                let len = 14.0;
                for side in [-1.0f64, 1.0f64] {
                    let t = angle + std::f64::consts::PI + side * 0.42;
                    let wing = NSBezierPath::bezierPath();
                    wing.setLineWidth(LINE_W);
                    wing.moveToPoint(b);
                    wing.lineToPoint(NSPoint::new(b.x + t.cos() * len, b.y + t.sin() * len));
                    wing.stroke();
                }
            }
            Shape::Rect { x, y, w, h } => {
                let rect = NSRect::new(
                    NSPoint::new(*x, size.height - *y - *h),
                    NSSize::new(*w, *h),
                );
                let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, 3.0, 3.0);
                path.setLineWidth(LINE_W);
                path.stroke();
            }
            Shape::Text {
                x,
                y,
                size: font_size,
                text,
            } => {
                let attrs: Retained<NSMutableDictionary<NSString, AnyObject>> =
                    NSMutableDictionary::new();
                let font = NSFont::systemFontOfSize(*font_size);
                unsafe {
                    let object: &AnyObject = &*font;
                    attrs.setObject_forKey(object, ProtocolObject::from_ref(NSFontAttributeName));
                    let color = NSColor::colorWithSRGBRed_green_blue_alpha(INK.0, INK.1, INK.2, 1.0);
                    let object: &AnyObject = &*color;
                    attrs.setObject_forKey(
                        object,
                        ProtocolObject::from_ref(NSForegroundColorAttributeName),
                    );
                }
                let string = NSString::from_str(text);
                unsafe {
                    string.drawAtPoint_withAttributes(
                        NSPoint::new(*x, size.height - *y - *font_size),
                        Some(&attrs),
                    );
                }
            }
        }
    }
}

/// CGImage → PNG 文件。
fn write_png_cg(cg: &CGImage, path: &std::path::Path) -> Result<(), String> {
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), cg);
    let props = objc2_foundation::NSDictionary::<NSBitmapImageRepPropertyKey, AnyObject>::new();
    let data = unsafe { rep.representationUsingType_properties(NSPNGFileType, &props) }
        .ok_or_else(|| "PNG 编码失败".to_string())?;
    if !data.writeToFile_atomically(&NSString::from_str(&path.to_string_lossy()), true) {
        return Err("写文件失败".to_string());
    }
    Ok(())
}

/// NSImage → PNG 文件。
fn write_png(image: &NSImage, path: &std::path::Path) -> Result<(), String> {
    let cg: Option<Retained<CGImage>> = unsafe {
        image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None)
    };
    let Some(cg) = cg else {
        return Err("取不到图像".to_string());
    };
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg);
    let props = objc2_foundation::NSDictionary::<NSBitmapImageRepPropertyKey, AnyObject>::new();
    let data = unsafe { rep.representationUsingType_properties(NSPNGFileType, &props) }
        .ok_or_else(|| "PNG 编码失败".to_string())?;
    if !data.writeToFile_atomically(&NSString::from_str(&path.to_string_lossy()), true) {
        return Err("写文件失败".to_string());
    }
    Ok(())
}

fn button(
    mtm: MainThreadMarker,
    title: &str,
    frame: NSRect,
    action: objc2::runtime::Sel,
    target: &CanvasView,
) -> Retained<NSButton> {
    let button = unsafe { NSButton::initWithFrame(NSButton::alloc(mtm), frame) };
    button.setTitle(&NSString::from_str(title));
    button.setBezelStyle(objc2_app_kit::NSBezelStyle::Rounded);
    button.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    unsafe { button.setAction(Some(action)) };
    unsafe { button.setTarget(Some(target)) };
    button
}

/// 打开标注窗口。`on_saved` 在写回原文件后调用（取消则不调用）。
pub fn edit(path: PathBuf, on_saved: impl FnOnce(PathBuf) + 'static) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let Some(image) = NSImage::initWithContentsOfFile(
        NSImage::alloc(),
        &NSString::from_str(&path.to_string_lossy()),
    ) else {
        log("markup: 读不出这张图");
        return;
    };
    let size = image.size();
    if size.width <= 0.0 || size.height <= 0.0 {
        return;
    }
    let screen = NSScreen::mainScreen(mtm).map(|s| s.visibleFrame());
    let (max_w, max_h) = match screen {
        Some(f) => (f.size.width * 0.82, f.size.height * 0.82 - BAR_H),
        None => (1200.0, 760.0),
    };
    let scale = (max_w / size.width).min(max_h / size.height).min(1.0);
    let canvas_h = (size.height * scale).round();
    let canvas_w = (size.width * scale).round();

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(canvas_w, canvas_h + BAR_H),
            ),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("标注"));
    let Some(content) = window.contentView() else { return };

    let canvas = CanvasView::new(mtm);
    canvas.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(canvas_w, canvas_h),
    ));
    content.addSubview(&canvas);

    let bar = unsafe {
        NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, canvas_h), NSSize::new(canvas_w, BAR_H)),
        )
    };
    let y = 8.0;
    bar.addSubview(&button(
        mtm,
        "箭头",
        NSRect::new(NSPoint::new(12.0, y), NSSize::new(64.0, 28.0)),
        sel!(pickArrow:),
        &canvas,
    ));
    bar.addSubview(&button(
        mtm,
        "框图",
        NSRect::new(NSPoint::new(84.0, y), NSSize::new(64.0, 28.0)),
        sel!(pickRect:),
        &canvas,
    ));
    bar.addSubview(&button(
        mtm,
        "文字",
        NSRect::new(NSPoint::new(156.0, y), NSSize::new(64.0, 28.0)),
        sel!(pickText:),
        &canvas,
    ));
    bar.addSubview(&button(
        mtm,
        "撤销",
        NSRect::new(NSPoint::new(228.0, y), NSSize::new(64.0, 28.0)),
        sel!(undoShape:),
        &canvas,
    ));
    bar.addSubview(&button(
        mtm,
        "取消",
        NSRect::new(NSPoint::new(canvas_w - 156.0, y), NSSize::new(64.0, 28.0)),
        sel!(cancelEdit:),
        &canvas,
    ));
    let done = button(
        mtm,
        "完成",
        NSRect::new(NSPoint::new(canvas_w - 84.0, y), NSSize::new(64.0, 28.0)),
        sel!(doneEdit:),
        &canvas,
    );
    bar.addSubview(&done);
    content.addSubview(&bar);

    *canvas.ivars().image.borrow_mut() = Some(image);
    *canvas.ivars().path.borrow_mut() = Some(path);
    canvas
        .ivars()
        .on_done
        .borrow_mut()
        .replace(Box::new(move |saved| {
            if let Some(saved) = saved {
                on_saved(saved);
            }
        }));
    *canvas.ivars().window.borrow_mut() = Some(window.clone());
    unsafe { window.setDelegate(Some(ProtocolObject::from_ref(&*canvas))) };

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    unsafe { app.activateIgnoringOtherApps(true) };
    window.center();
    window.makeKeyAndOrderFront(None);
}
