//! 标注：截完图直接在 iRec 里画箭头、框图、文字，还能选中挪位置，然后写回原文件。
//!
//! 系统自带的 Markup 是注册在分享服务里的（`com.apple.MarkupUI.Markup`），
//! 但在不少机器上它既不在可用服务列表里、也拉不起窗口，所以这里自己做。
//! 形状坐标一律用「图片像素、y 朝下」，画的时候再翻一次。

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, ClassType, define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezierPath,
    NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSButton, NSCellImagePosition, NSColor,
    NSCommandKeyMask, NSEvent, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSGraphicsContext, NSImage, NSPNGFileType, NSScreen, NSStringDrawing, NSTextField, NSView,
    NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapInfo, CGColorSpace, CGContext,
    CGContextScaleCTM, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{
    MainThreadMarker, NSMutableDictionary, NSDictionary, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString,
};

use crate::state::log;

/// 工具条高度。
const BAR_H: f64 = 46.0;
const LINE_W: f64 = 3.0;
/// 命中判定的宽度（图片坐标，按图片大小缩放）。
const HIT: f64 = 14.0;

/// 四种标记色：红、黄、蓝、绿（和 macOS 系统色接近）。
const COLORS: [(f64, f64, f64); 4] = [
    (1.0, 0.23, 0.19),
    (1.0, 0.80, 0.0),
    (0.20, 0.60, 1.0),
    (0.20, 0.78, 0.35),
];

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    /// 选中并拖动已有标记。
    Select,
    Arrow,
    Rect,
    Text,
}

#[derive(Clone)]
enum Kind {
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

#[derive(Clone)]
struct Mark {
    color: usize,
    kind: Kind,
}

impl Mark {
    fn translate(&mut self, dx: f64, dy: f64) {
        match &mut self.kind {
            Kind::Arrow { from, to } => {
                from.0 += dx;
                from.1 += dy;
                to.0 += dx;
                to.1 += dy;
            }
            Kind::Rect { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
            Kind::Text { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
        }
    }

    /// 包围盒（图片坐标，y 朝下）。
    fn bounds(&self) -> (f64, f64, f64, f64) {
        match &self.kind {
            Kind::Arrow { from, to } => (
                from.0.min(to.0),
                from.1.min(to.1),
                (from.0 - to.0).abs(),
                (from.1 - to.1).abs(),
            ),
            Kind::Rect { x, y, w, h } => (*x, *y, *w, *h),
            Kind::Text { x, y, size, text } => (
                *x,
                *y - size,
                text.chars().count() as f64 * size,
                size * 1.35,
            ),
        }
    }

    /// 点中了没有（tolerance 用图片单位）。
    fn hit(&self, p: (f64, f64), tol: f64) -> bool {
        match &self.kind {
            Kind::Arrow { from, to } => distance_to_segment(p, *from, *to) <= tol,
            Kind::Rect { x, y, w, h } => {
                let inside = p.0 >= *x && p.0 <= *x + *w && p.1 >= *y && p.1 <= *y + *h;
                let near = p.0 >= *x - tol
                    && p.0 <= *x + *w + tol
                    && p.1 >= *y - tol
                    && p.1 <= *y + *h + tol;
                inside || near
            }
            Kind::Text { .. } => {
                let (x, y, w, h) = self.bounds();
                p.0 >= x - tol && p.0 <= x + w + tol && p.1 >= y - tol && p.1 <= y + h + tol
            }
        }
    }
}

fn distance_to_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let len2 = vx * vx + vy * vy;
    if len2 <= 0.0 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    let t = ((p.0 - a.0) * vx + (p.1 - a.1) * vy) / len2;
    let t = t.clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * vx, a.1 + t * vy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

struct CanvasIvars {
    image: RefCell<Option<Retained<NSImage>>>,
    marks: RefCell<Vec<Mark>>,
    tool: Cell<Tool>,
    color: Cell<usize>,
    /// 正在画的形状（起点、当前点），图片坐标。
    drag: Cell<Option<((f64, f64), (f64, f64))>>,
    /// 选中的标记。
    selected: Cell<Option<usize>>,
    /// 拖动选中标记时的上一个位置。
    last: Cell<(f64, f64)>,
    path: RefCell<Option<PathBuf>>,
    window: RefCell<Option<Retained<NSWindow>>>,
    field: RefCell<Option<Retained<NSTextField>>>,
    field_at: Cell<(f64, f64)>,
    on_done: RefCell<Option<Box<dyn FnOnce(Option<PathBuf>)>>>,
}

impl Default for CanvasIvars {
    fn default() -> Self {
        Self {
            image: RefCell::new(None),
            marks: RefCell::new(Vec::new()),
            tool: Cell::new(Tool::Select),
            color: Cell::new(0),
            drag: Cell::new(None),
            selected: Cell::new(None),
            last: Cell::new((0.0, 0.0)),
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
            match self.ivars().tool.get() {
                Tool::Select => {
                    let tol = self.tolerance();
                    let index = self
                        .ivars()
                        .marks
                        .borrow()
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, mark)| mark.hit(p, tol))
                        .map(|(index, _)| index);
                    self.ivars().selected.set(index);
                    self.ivars().last.set(p);
                }
                Tool::Text => self.begin_text(p),
                Tool::Arrow | Tool::Rect => {
                    self.ivars().selected.set(None);
                    self.ivars().drag.set(Some((p, p)));
                }
            }
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let p = self.to_image(self.convert(event.locationInWindow()));
            if let Some((from, _)) = self.ivars().drag.get() {
                self.ivars().drag.set(Some((from, p)));
            } else if let Some(index) = self.ivars().selected.get() {
                let last = self.ivars().last.get();
                let (dx, dy) = (p.0 - last.0, p.1 - last.1);
                if let Some(mark) = self.ivars().marks.borrow_mut().get_mut(index) {
                    mark.translate(dx, dy);
                }
                self.ivars().last.set(p);
            }
            self.setNeedsDisplay(true);
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
            let kind = match self.ivars().tool.get() {
                Tool::Arrow => Kind::Arrow { from, to },
                Tool::Rect => Kind::Rect {
                    x: from.0.min(to.0),
                    y: from.1.min(to.1),
                    w: (from.0 - to.0).abs(),
                    h: (from.1 - to.1).abs(),
                },
                Tool::Select | Tool::Text => return,
            };
            let mark = Mark {
                color: self.ivars().color.get(),
                kind,
            };
            let index = self.ivars().marks.borrow().len();
            self.ivars().marks.borrow_mut().push(mark);
            // 画完回到选择工具，方便立刻挪位置。
            self.ivars().tool.set(Tool::Select);
            self.ivars().selected.set(Some(index));
            self.setNeedsDisplay(true);
            self.sync_tools();
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            // Esc 放弃，⌫ 删掉选中的标记。
            if event.keyCode() == 53 {
                self.finish(None);
            } else if event.keyCode() == 51 || event.keyCode() == 117 {
                if let Some(index) = self.ivars().selected.take() {
                    let mut marks = self.ivars().marks.borrow_mut();
                    if index < marks.len() {
                        marks.remove(index);
                    }
                    self.setNeedsDisplay(true);
                }
            }
        }

        #[unsafe(method(toolClicked:))]
        fn tool_clicked(&self, sender: &AnyObject) {
            let tag: isize = unsafe { objc2::msg_send![sender, tag] };
            let tool = match tag {
                0 => Tool::Select,
                1 => Tool::Arrow,
                2 => Tool::Rect,
                3 => Tool::Text,
                _ => Tool::Select,
            };
            self.ivars().tool.set(tool);
            self.ivars().selected.set(None);
            self.setNeedsDisplay(true);
            self.sync_tools();
        }

        #[unsafe(method(colorClicked:))]
        fn color_clicked(&self, sender: &AnyObject) {
            let tag: isize = unsafe { objc2::msg_send![sender, tag] };
            if tag < 0 || tag as usize >= COLORS.len() {
                return;
            }
            let index = tag as usize;
            self.ivars().color.set(index);
            // 有选中的标记就顺手改它的颜色。
            if let Some(selected) = self.ivars().selected.get() {
                if let Some(mark) = self.ivars().marks.borrow_mut().get_mut(selected) {
                    mark.color = index;
                }
            }
            self.setNeedsDisplay(true);
            self.sync_tools();
        }

        #[unsafe(method(undoMark:))]
        fn undo_mark(&self, _sender: &AnyObject) {
            self.ivars().marks.borrow_mut().pop();
            self.ivars().selected.set(None);
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

    fn image_size(&self) -> NSSize {
        self.ivars()
            .image
            .borrow()
            .as_ref()
            .map(|image| image.size())
            .unwrap_or(NSSize::new(1.0, 1.0))
    }

    /// 图片在视图里的位置（按比例居中）。
    fn image_rect(&self) -> NSRect {
        let bounds = self.bounds();
        let size = self.image_size();
        let scale = (bounds.size.width / size.width).min(bounds.size.height / size.height);
        let w = size.width * scale;
        let h = size.height * scale;
        NSRect::new(
            NSPoint::new((bounds.size.width - w) / 2.0, (bounds.size.height - h) / 2.0),
            NSSize::new(w, h),
        )
    }

    /// 视图上的 1 点等于图片里的多少（用来换算命中宽度和字号）。
    fn unit(&self) -> f64 {
        let size = self.image_size();
        let r = self.image_rect();
        if r.size.width > 0.0 {
            size.width / r.size.width
        } else {
            1.0
        }
    }

    fn tolerance(&self) -> f64 {
        HIT * self.unit()
    }

    fn to_image(&self, point: NSPoint) -> (f64, f64) {
        let r = self.image_rect();
        let u = self.unit();
        ((point.x - r.origin.x) * u, (point.y - r.origin.y) * u)
    }

    fn to_view(&self, p: (f64, f64)) -> NSPoint {
        let r = self.image_rect();
        let u = self.unit();
        NSPoint::new(r.origin.x + p.0 / u, r.origin.y + p.1 / u)
    }

    fn paint(&self) {
        let bounds = self.bounds();
        NSColor::colorWithSRGBRed_green_blue_alpha(0.11, 0.11, 0.12, 1.0).setFill();
        unsafe { NSBezierPath::fillRect(bounds) };
        let r = self.image_rect();
        if let Some(image) = self.ivars().image.borrow().as_ref() {
            // 图片底下垫一圈阴影，免得和背景糊在一起。
            let shadow = NSShadowForMarkup::new();
            unsafe {
                shadow.set_apply();
            }
            image.drawInRect(r);
            unsafe {
                NSGraphicsContext::restoreGraphicsState_class();
            }
        }
        let marks = self.ivars().marks.borrow().clone();
        for (index, mark) in marks.iter().enumerate() {
            self.paint_mark(mark);
            if self.ivars().selected.get() == Some(index) {
                self.paint_selection(mark);
            }
        }
        if let Some((from, to)) = self.ivars().drag.get() {
            let color = self.ivars().color.get();
            match self.ivars().tool.get() {
                Tool::Arrow => {
                    self.paint_mark(&Mark {
                        color,
                        kind: Kind::Arrow { from, to },
                    });
                }
                Tool::Rect => {
                    self.paint_mark(&Mark {
                        color,
                        kind: Kind::Rect {
                            x: from.0.min(to.0),
                            y: from.1.min(to.1),
                            w: (from.0 - to.0).abs(),
                            h: (from.1 - to.1).abs(),
                        },
                    });
                }
                Tool::Select | Tool::Text => {}
            }
        }
    }

    fn paint_mark(&self, mark: &Mark) {
        let (r, g, b) = COLORS[mark.color.min(COLORS.len() - 1)];
        NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0).setStroke();
        NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0).setFill();
        let width = LINE_W * self.unit();
        match &mark.kind {
            Kind::Arrow { from, to } => {
                let a = self.to_view(*from);
                let c = self.to_view(*to);
                let path = NSBezierPath::bezierPath();
                path.setLineWidth(width);
                path.moveToPoint(a);
                path.lineToPoint(c);
                path.stroke();
                let angle = (c.y - a.y).atan2(c.x - a.x);
                let len = 12.0 + width;
                for side in [-1.0f64, 1.0f64] {
                    let t = angle + std::f64::consts::PI + side * 0.42;
                    let wing = NSBezierPath::bezierPath();
                    wing.setLineWidth(width);
                    wing.moveToPoint(c);
                    wing.lineToPoint(NSPoint::new(c.x + t.cos() * len, c.y + t.sin() * len));
                    wing.stroke();
                }
            }
            Kind::Rect { x, y, w, h } => {
                let origin = self.to_view((*x, *y));
                let rect = NSRect::new(
                    NSPoint::new(origin.x, origin.y),
                    NSSize::new(w / self.unit(), h / self.unit()),
                );
                let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, 2.0, 2.0);
                path.setLineWidth(width);
                path.stroke();
            }
            Kind::Text { x, y, size, text } => {
                let origin = self.to_view((*x, *y));
                let font = NSFont::systemFontOfSize(size / self.unit());
                let attrs: Retained<NSMutableDictionary<NSString, AnyObject>> =
                    NSMutableDictionary::new();
                unsafe {
                    let object: &AnyObject = &*font;
                    attrs.setObject_forKey(object, ProtocolObject::from_ref(NSFontAttributeName));
                    let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
                    let object: &AnyObject = &*color;
                    attrs.setObject_forKey(
                        object,
                        ProtocolObject::from_ref(NSForegroundColorAttributeName),
                    );
                }
                let string = NSString::from_str(text);
                unsafe {
                    string.drawAtPoint_withAttributes(
                        NSPoint::new(origin.x, origin.y - size / self.unit()),
                        Some(&attrs),
                    );
                }
            }
        }
    }

    /// 选中了就在外面画一圈虚线框，提示可以拖。
    fn paint_selection(&self, mark: &Mark) {
        let (x, y, w, h) = mark.bounds();
        let pad = 6.0 * self.unit();
        let a = self.to_view((x - pad, y - pad));
        let b = self.to_view((x + w + pad, y + h + pad));
        let rect = NSRect::new(
            NSPoint::new(a.x.min(b.x), a.y.min(b.y)),
            NSSize::new((a.x - b.x).abs(), (a.y - b.y).abs()),
        );
        let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, 4.0, 4.0);
        path.setLineWidth(1.0);
        let mut pattern: [f64; 2] = [4.0, 3.0];
        unsafe { path.setLineDash_count_phase(pattern.as_ptr(), 2, 0.0) };
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.85).setStroke();
        path.stroke();
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
                    NSSize::new(220.0, 30.0),
                ),
            )
        };
        let (r, g, b) = COLORS[self.ivars().color.get().min(COLORS.len() - 1)];
        field.setFont(Some(&NSFont::systemFontOfSize(18.0)));
        field.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0)));
        field.setDrawsBackground(true);
        field.setBackgroundColor(Some(
            &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.9),
        ));
        field.setBezeled(false);
        field.setPlaceholderString(Some(&NSString::from_str("输入文字，回车确认")));
        unsafe { field.setAction(Some(sel!(commitText:))) };
        unsafe { field.setTarget(Some(self)) };
        self.addSubview(&field);
        if let Some(window) = self.window() {
            unsafe { window.makeFirstResponder(Some(&field)) };
        }
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
        let at = self.ivars().field_at.get();
        let index = self.ivars().marks.borrow().len();
        self.ivars().marks.borrow_mut().push(Mark {
            color: self.ivars().color.get(),
            kind: Kind::Text {
                x: at.0,
                y: at.1,
                size: 18.0 * self.unit(),
                text,
            },
        });
        self.ivars().tool.set(Tool::Select);
        self.ivars().selected.set(Some(index));
        self.setNeedsDisplay(true);
        self.sync_tools();
    }

    /// 把标记和原图合成一张新图，覆盖写回原文件。
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
        let marks = self.ivars().marks.borrow().clone();
        match compose(&image, &marks) {
            Some(cg) => match write_png(&cg, &path) {
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

    /// 让工具条跟当前工具 / 颜色对上。
    fn sync_tools(&self) {
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            if let Some(content) = window.contentView() {
                for sub in unsafe { content.subviews() }.iter() {
                    if let Some(bar) = sub.downcast_ref::<MarkupBar>() {
                        bar.refresh();
                    }
                }
            }
        }
    }
}

/// 极简的阴影封装：画图片时垫一下。
struct NSShadowForMarkup {
    shadow: Retained<objc2_app_kit::NSShadow>,
}

impl NSShadowForMarkup {
    fn new() -> Self {
        let shadow = unsafe { objc2_app_kit::NSShadow::new() };
        unsafe {
            shadow.setShadowColor(Some(
                &NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.45),
            ));
            shadow.setShadowOffset(NSSize::new(0.0, -6.0));
            shadow.setShadowBlurRadius(18.0);
        }
        Self { shadow }
    }

    unsafe fn set_apply(&self) {
        unsafe {
            NSGraphicsContext::saveGraphicsState_class();
            self.shadow.set();
        }
    }
}

#[derive(Default)]
struct BarIvars {}

/// 工具条：图标化的工具 + 颜色 + 撤销 / 完成 / 取消。
define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = BarIvars]
    struct MarkupBar;

    impl MarkupBar {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            let bounds = self.bounds();
            NSColor::colorWithSRGBRed_green_blue_alpha(0.16, 0.16, 0.17, 1.0).setFill();
            unsafe { NSBezierPath::fillRect(bounds) };
            NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.08).setStroke();
            let line = NSBezierPath::bezierPath();
            line.setLineWidth(1.0);
            line.moveToPoint(NSPoint::new(0.0, bounds.size.height - 0.5));
            line.lineToPoint(NSPoint::new(bounds.size.width, bounds.size.height - 0.5));
            line.stroke();
        }
    }
);

impl MarkupBar {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(BarIvars::default());
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        this.setFrame(frame);
        this
    }

    fn refresh(&self) {
        // 按钮的选中态由 canvas 在切换时同步。
        self.setNeedsDisplay(true);
    }
}

/// 一个图标按钮。
fn icon_button(
    mtm: MainThreadMarker,
    symbol: &str,
    tip: &str,
    frame: NSRect,
    action: objc2::runtime::Sel,
    target: &CanvasView,
    tag: isize,
) -> Retained<NSButton> {
    let button = unsafe { NSButton::initWithFrame(NSButton::alloc(mtm), frame) };
    if let Some(image) =
        NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(symbol), None)
    {
        unsafe { image.setTemplate(true) };
        button.setImage(Some(&image));
    }
    button.setImagePosition(NSCellImagePosition::ImageOnly);
    button.setBordered(false);
    button.setToolTip(Some(&NSString::from_str(tip)));
    unsafe { button.setAction(Some(action)) };
    unsafe { button.setTarget(Some(target)) };
    button.setTag(tag);
    button
}

/// 颜色按钮：一个小圆点，选中的外面多一圈。
fn color_button(
    mtm: MainThreadMarker,
    index: usize,
    selected: bool,
    frame: NSRect,
    target: &CanvasView,
) -> Retained<NSButton> {
    let (r, g, b) = COLORS[index];
    let size = 22.0;
    let image = {
        let block = RcBlock::new(move |rect: NSRect| -> objc2::runtime::Bool {
            let dot = NSRect::new(
                NSPoint::new(rect.origin.x + 3.0, rect.origin.y + 3.0),
                NSSize::new(rect.size.width - 6.0, rect.size.height - 6.0),
            );
            let path = NSBezierPath::bezierPathWithOvalInRect(dot);
            NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0).setFill();
            path.fill();
            if selected {
                let ring = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                    NSPoint::new(rect.origin.x + 0.5, rect.origin.y + 0.5),
                    NSSize::new(rect.size.width - 1.0, rect.size.height - 1.0),
                ));
                ring.setLineWidth(1.5);
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.9).setStroke();
                ring.stroke();
            }
            objc2::runtime::Bool::YES
        });
        NSImage::imageWithSize_flipped_drawingHandler(
            NSSize::new(size, size),
            false,
            &block,
        )
    };
    let button = unsafe { NSButton::initWithFrame(NSButton::alloc(mtm), frame) };
    button.setImage(Some(&image));
    button.setImagePosition(NSCellImagePosition::ImageOnly);
    button.setBordered(false);
    unsafe { button.setAction(Some(sel!(colorClicked:))) };
    unsafe { button.setTarget(Some(target)) };
    button.setTag(index as isize);
    button
}

fn text_button(
    mtm: MainThreadMarker,
    title: &str,
    frame: NSRect,
    action: objc2::runtime::Sel,
    target: &CanvasView,
    primary: bool,
) -> Retained<NSButton> {
    let button = unsafe { NSButton::initWithFrame(NSButton::alloc(mtm), frame) };
    button.setTitle(&NSString::from_str(title));
    button.setBezelStyle(objc2_app_kit::NSBezelStyle::Rounded);
    button.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    unsafe { button.setAction(Some(action)) };
    unsafe { button.setTarget(Some(target)) };
    if primary {
        button.setKeyEquivalent(&NSString::from_str("\r"));
        unsafe { button.setKeyEquivalentModifierMask(NSCommandKeyMask) };
    }
    button
}

/// 把标记画进原图的像素里，尺寸和原图一致。
fn compose(image: &NSImage, marks: &[Mark]) -> Option<CFRetained<CGImage>> {
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
        CGBitmapContextCreate(std::ptr::null_mut(), w, h, 8, 0, Some(&space), info)
    }?;
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
    let ns = unsafe { NSGraphicsContext::graphicsContextWithCGContext_flipped(&ctx, false) };
    unsafe { NSGraphicsContext::setCurrentContext(Some(&ns)) };
    image.drawInRect(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(w as f64, h as f64),
    ));
    unsafe { CGContextScaleCTM(Some(&ctx), sx, sy) };
    paint_into(marks, size);
    unsafe { CGBitmapContextCreateImage(Some(&ctx)) }
}

/// 在图片自己的坐标系里画（上下文 y 朝上，标记 y 朝下）。
fn paint_into(marks: &[Mark], size: NSSize) {
    let flip = |p: (f64, f64)| NSPoint::new(p.0, size.height - p.1);
    for mark in marks {
        let (r, g, b) = COLORS[mark.color.min(COLORS.len() - 1)];
        NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0).setStroke();
        NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0).setFill();
        match &mark.kind {
            Kind::Arrow { from, to } => {
                let a = flip(*from);
                let c = flip(*to);
                let path = NSBezierPath::bezierPath();
                path.setLineWidth(LINE_W);
                path.moveToPoint(a);
                path.lineToPoint(c);
                path.stroke();
                let angle = (c.y - a.y).atan2(c.x - a.x);
                for side in [-1.0f64, 1.0f64] {
                    let t = angle + std::f64::consts::PI + side * 0.42;
                    let wing = NSBezierPath::bezierPath();
                    wing.setLineWidth(LINE_W);
                    wing.moveToPoint(c);
                    wing.lineToPoint(NSPoint::new(c.x + t.cos() * 14.0, c.y + t.sin() * 14.0));
                    wing.stroke();
                }
            }
            Kind::Rect { x, y, w, h } => {
                let rect = NSRect::new(
                    NSPoint::new(*x, size.height - *y - *h),
                    NSSize::new(*w, *h),
                );
                let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, 3.0, 3.0);
                path.setLineWidth(LINE_W);
                path.stroke();
            }
            Kind::Text {
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
                    let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
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
fn write_png(cg: &CGImage, path: &std::path::Path) -> Result<(), String> {
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), cg);
    let props = NSDictionary::<NSBitmapImageRepPropertyKey, AnyObject>::new();
    let data = unsafe { rep.representationUsingType_properties(NSPNGFileType, &props) }
        .ok_or_else(|| "PNG 编码失败".to_string())?;
    if !data.writeToFile_atomically(&NSString::from_str(&path.to_string_lossy()), true) {
        return Err("写文件失败".to_string());
    }
    Ok(())
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
        NSPoint::new(0.0, BAR_H),
        NSSize::new(canvas_w, canvas_h),
    ));
    content.addSubview(&canvas);

    let bar = MarkupBar::new(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(canvas_w, BAR_H)),
    );
    let tools: [(&str, &str); 4] = [
        ("cursorarrow", "选择 / 移动"),
        ("arrow.up.right", "箭头"),
        ("rectangle", "框图"),
        ("textformat", "文字"),
    ];
    for (index, (symbol, tip)) in tools.iter().enumerate() {
        let button = icon_button(
            mtm,
            symbol,
            tip,
            NSRect::new(NSPoint::new(14.0 + index as f64 * 38.0, 9.0), NSSize::new(30.0, 28.0)),
            sel!(toolClicked:),
            &canvas,
            index as isize,
        );
        bar.addSubview(&button);
    }
    for index in 0..COLORS.len() {
        bar.addSubview(&color_button(
            mtm,
            index,
            index == 0,
            NSRect::new(
                NSPoint::new(178.0 + index as f64 * 28.0, 11.0),
                NSSize::new(24.0, 24.0),
            ),
            &canvas,
        ));
    }
    bar.addSubview(&icon_button(
        mtm,
        "arrow.uturn.backward",
        "撤销",
        NSRect::new(NSPoint::new(canvas_w - 210.0, 9.0), NSSize::new(30.0, 28.0)),
        sel!(undoMark:),
        &canvas,
        0,
    ));
    bar.addSubview(&text_button(
        mtm,
        "取消",
        NSRect::new(NSPoint::new(canvas_w - 168.0, 8.0), NSSize::new(72.0, 30.0)),
        sel!(cancelEdit:),
        &canvas,
        false,
    ));
    let done = text_button(
        mtm,
        "完成",
        NSRect::new(NSPoint::new(canvas_w - 88.0, 8.0), NSSize::new(72.0, 30.0)),
        sel!(doneEdit:),
        &canvas,
        true,
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
