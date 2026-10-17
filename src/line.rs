//! 晾衣绳：录完的视频挂在菜单栏下面的一根绳子上。
//!
//! 动画的创意来自 Tendedero（<https://github.com/alejandrobujan/tendedero>），
//! 全部用 AppKit 手绘 + 弹簧积分复刻：
//!
//! * 绳子是一条两端淡出的抛物线，卡片用夹子夹在上面；
//! * 每张卡片带一点随机倾角，落绳时先甩一下再慢慢停住（阻尼弹簧）；
//! * 隔一阵来一阵风，整排卡片随机先后、随机幅度地轻轻晃；
//! * 新录完的视频从录制的那块区域「飞」上来，边飞边长出玻璃边框和夹子；
//! * 取下时卡片一边转一边往下掉；
//! * 绳子默认收在屏幕上方，指针在菜单栏停留 0.25 秒才滑下来，离开 0.5 秒自己收回。
//!
//! 所有坐标在模块内部统一成「顶空间」：原点在绳子窗口左上角，y 向下；
//! 画的时候再翻成 AppKit 的视图坐标（y 向上）。

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSColor, NSCursor, NSDragOperation, NSDraggingContext,
    NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent, NSEventModifierFlags,
    NSFloatingWindowLevel, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSGradient,
    NSGraphicsContext, NSImage, NSPanel, NSPasteboardWriting, NSScreen, NSShadow, NSStringDrawing,
    NSView, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_av_foundation::{AVAssetImageGenerator, AVURLAsset};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGContext;
use objc2_core_graphics::CGImage;
use objc2_core_media::CMTime;
use objc2_foundation::{
    MainThreadMarker, NSArray, NSMutableDictionary, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString, NSTimer, NSURL,
};

use crate::state::Rect;

// MARK: - 尺寸（与 Tendedero 的 Layout 一致）

/// 照片区最大高度；卡片宽度；两张卡片中心之间的距离。
const PHOTO_MAX_H: f64 = 104.0;
const CARD_W: f64 = 150.0;
const SPACING: f64 = CARD_W + 24.0;
/// 绳子窗口高度：照片 + 夹子 + 上下留白。
const PANEL_H: f64 = PHOTO_MAX_H + 106.0;
/// 绳子两端离窗口顶部的距离。
const ROPE_TOP: f64 = 10.0;
/// 夹子顶边在绳子上方多少。
const PIN_ABOVE: f64 = 9.5;
/// 照片外面那圈玻璃的厚度。
const INSET: f64 = 4.0;
const RADIUS: f64 = 16.0;
/// 夹子高 26，其中 12 压在卡片上，所以卡片顶边在夹子顶边下方 14。
const CARD_TOP: f64 = 14.0;
/// 绳上最多挂几张，多了最旧那张自己掉下去。
const MAX_ITEMS: usize = 8;
/// 收起时整条绳子往上藏多少。
const HIDDEN: f64 = PANEL_H + 12.0;

// MARK: - 动画时长

const FLIGHT_DUR: f64 = 0.65;
const FALL_DUR: f64 = 0.55;
const FALL_DISTANCE: f64 = 520.0;
/// 飞行途中往上拱起的高度。
const ARC: f64 = 30.0;
/// 指针在菜单栏停留多久把绳子放下来。
const REVEAL_DELAY: f64 = 0.25;
/// 指针离开多久把绳子收回去。
const RETRACT_DELAY: f64 = 0.5;
/// 录完一段视频后绳子露脸多久。
const PEEK: f64 = 2.5;

// MARK: - 弹簧

#[derive(Clone, Copy)]
struct Spring {
    k: f64,
    c: f64,
}

impl Spring {
    /// SwiftUI 的 `spring(response:dampingFraction:)`。
    const fn response(response: f64, damping: f64) -> Self {
        let w = std::f64::consts::TAU / response;
        Self {
            k: w * w,
            c: 2.0 * damping * w,
        }
    }

    /// SwiftUI 的 `interpolatingSpring(stiffness:damping:)`。
    const fn stiffness(k: f64, damping: f64) -> Self {
        Self { k, c: damping }
    }

    /// 半隐式欧拉，60 fps 下稳定。
    fn step(self, x: f64, v: f64, dt: f64) -> (f64, f64) {
        let v = v + (-self.k * x - self.c * v) * dt;
        (x + v * dt, v)
    }
}

/// 卡片落绳（从上方 46 点落下）。
const ARRIVE: Spring = Spring::response(0.42, 0.72);
/// 摆动：欠阻尼得很厉害，所以会来回晃很久。
const SWING: Spring = Spring::stiffness(46.0, 2.6);
/// 卡片在绳上的横向挪动。
const LAYOUT: Spring = Spring::response(0.55, 0.78);
/// 绳子滑下来 / 收回去。
const REVEAL: Spring = Spring::response(0.42, 0.82);
const HIDE: Spring = Spring::response(0.35, 1.0);

/// 给一次摆动一个初速度，幅度约等于 `degrees` 度。
fn nudge(swing_v: &mut f64, degrees: f64) {
    *swing_v += degrees * SWING.k.sqrt();
}

// MARK: - 绳子的形状

fn sag(width: f64) -> f64 {
    (width * 0.018).min(30.0)
}

/// 绳子在 x 处的高度（顶空间）。
fn rope_y(x: f64, width: f64) -> f64 {
    if width <= 0.0 {
        return ROPE_TOP;
    }
    let f = x / width;
    ROPE_TOP + 4.0 * sag(width) * f * (1.0 - f)
}

/// 第 index 张卡片挂在哪儿：居中排开，太挤就互相靠近。
fn layout_x(index: usize, count: usize, width: f64) -> f64 {
    let gaps = count.saturating_sub(1) as f64;
    if gaps < 1.0 {
        return width / 2.0;
    }
    let step = SPACING.min(((width - 200.0).max(0.0) / gaps).max(60.0));
    width / 2.0 - gaps * step / 2.0 + index as f64 * step
}

// MARK: - 一张卡片

/// 指针落在卡片的哪儿。
#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Card,
    Close,
    Annotate,
}

struct Peg {
    id: u64,
    path: PathBuf,
    image: Retained<NSImage>,
    /// 照片区大小（不含玻璃边）。
    photo: NSSize,
    /// 录屏（画播放键）还是截图。
    video: bool,
    /// 静止时的倾角，随机 ±2.5°，看着像手夹上去的。
    tilt: f64,
    swing: f64,
    swing_v: f64,
    /// 落绳时从 -46 开始弹到 0。
    drop: f64,
    drop_v: f64,
    x: f64,
    x_v: f64,
    arrived: bool,
    /// 还在飞过来，卡片先藏着。
    flying: bool,
    /// 开始掉下去之后经过的时间。
    fall: Option<f64>,
    hover: bool,
    hover_mix: f64,
    /// 一阵风吹过来的时间和幅度。
    gust: Option<(Instant, f64)>,
    /// 操作反馈（「已显示」之类）和它出现的时刻。
    bubble: Option<(String, Instant)>,
}

impl Peg {
    fn card_w(&self) -> f64 {
        self.photo.width + INSET * 2.0
    }

    fn card_h(&self) -> f64 {
        self.photo.height + INSET * 2.0
    }
}

/// 正在飞的那张卡片。
#[derive(Default)]
struct Flight {
    active: bool,
    image: Option<Retained<NSImage>>,
    /// 起飞处（录的那块区域）与目标卡片的框，都在飞行窗口的视图坐标里。
    from: NSRect,
    to: NSRect,
    tilt: f64,
    t: f64,
}

struct LineState {
    items: Vec<Peg>,
    flight: Flight,
    revealed: bool,
    /// 手动放下来的（菜单栏「显示晾衣绳」），指针走开也不收。
    pinned: bool,
    /// 0 = 收在屏幕上方，1 = 完全放下来。
    slide: f64,
    slide_v: f64,
    peek_until: Option<Instant>,
    /// 指针在菜单栏停留的起始时刻。
    dwell: Option<Instant>,
    /// 指针离开绳子区域的起始时刻。
    away: Option<Instant>,
    /// 刚在菜单栏点过，别急着放下来。
    suppressed: bool,
    width: f64,
    height: f64,
    last: Instant,
    breeze_at: Instant,
    enabled: bool,
    hover_enabled: bool,
    busy: bool,
    pointer: Option<NSPoint>,
    next_id: u64,
    /// 绳子所在的屏幕（AppKit 全局坐标）。
    screen: NSRect,
    /// 绳子窗口顶边离屏幕顶边多远（顶空间），飞行目标要用到。
    panel_top: f64,
    /// 按下但还没决定是点击还是拖动：卡片 id、是否按在叉上、按下的位置。
    press: Option<(u64, Hit, NSPoint)>,
    /// 「标注」按钮用的铅笔图标（SF Symbol），建一次就留着。
    annotate_icon: Option<Retained<NSImage>>,
    /// 正在被拖走的那张卡片。
    dragging: Option<u64>,
    /// 小手光标是否已经设上（只在变化时改，别去抢别的应用的光标）。
    hand: bool,
    /// 窗口现在是否接收点击（只有指针压在卡片上时才收，其余时候点击穿过去）。
    clickable: bool,
}

impl Default for LineState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            flight: Flight::default(),
            revealed: false,
            pinned: false,
            slide: 0.0,
            slide_v: 0.0,
            peek_until: None,
            dwell: None,
            away: None,
            suppressed: false,
            width: 0.0,
            height: PANEL_H,
            last: Instant::now(),
            breeze_at: Instant::now() + Duration::from_secs(6),
            enabled: true,
            hover_enabled: true,
            busy: false,
            pointer: None,
            next_id: 1,
            screen: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
            panel_top: 0.0,
            press: None,
            annotate_icon: None,
            dragging: None,
            hand: false,
            clickable: false,
        }
    }
}

#[derive(Default)]
struct LineIvars {
    state: RefCell<LineState>,
}

#[derive(Default)]
struct FlightIvars {}

#[derive(Default)]
struct PanelIvars {}

static LINE: Mutex<Option<Line>> = Mutex::new(None);

/// 已经录好、等着挂上去的视频。缩略图在后台线程生成，主线程取走。
struct Pending {
    image: Retained<CGImage>,
    path: PathBuf,
    region: Option<Rect>,
    /// 录屏（有播放键）还是截图（没有）。
    video: bool,
}

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());

struct Line {
    panel: Retained<LinePanel>,
    view: Retained<LineView>,
    flight_panel: Retained<LinePanel>,
    flight_view: Retained<FlightView>,
}

// SAFETY: 两个窗口、两个视图都只在主线程创建和使用。
unsafe impl Send for Line {}

define_class!(
    /// 绳子窗口：永远不抢焦点，也不接收键盘。
    #[unsafe(super = NSPanel)]
    #[thread_kind = MainThreadOnly]
    #[ivars = PanelIvars]
    struct LinePanel;

    impl LinePanel {
        #[unsafe(method(canBecomeKey))]
        fn can_become_key(&self) -> bool {
            false
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }
    }
);

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = LineIvars]
    struct LineView;

    unsafe impl NSObjectProtocol for LineView {}

    /// 卡片可以被拖出去：拖进别的 App 是送一份，拖进文件夹文件就留在那儿。
    unsafe impl NSDraggingSource for LineView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn dragging_mask(
            &self,
            _session: &NSDraggingSession,
            context: NSDraggingContext,
        ) -> NSDragOperation {
            // App 拿副本，Finder 会移动，Dock 的废纸篓要 delete。
            if context == NSDraggingContext::OutsideApplication {
                NSDragOperation::Copy | NSDragOperation::Move | NSDragOperation::Delete
            } else {
                NSDragOperation::empty()
            }
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn dragging_ended(
            &self,
            _session: &NSDraggingSession,
            _point: NSPoint,
            operation: NSDragOperation,
        ) {
            self.drag_ended(operation);
        }
    }

    impl LineView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            draw_line(self);
        }

        /// 绳子是横贯屏幕的一条，空白处的点击必须穿过去。
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<LineView>> {
            let take = {
                let st = self.ivars().state.borrow();
                st.enabled && st.slide >= 0.6
            };
            if take { self.hit(point).map(|_| self.retain()) } else { None }
        }

        /// 按下先记着：是点击还是拖动，等抬起或移动了才知道。
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let hit = self.hit(point);
            let mut st = self.ivars().state.borrow_mut();
            st.press = match hit {
                Some((index, on)) => st.items.get(index).map(|item| (item.id, on, point)),
                None => None,
            };
        }

        /// 拖出去：卡片变成真正的文件，交给 AppKit 的拖放会话。
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let press = self.ivars().state.borrow().press;
            let Some((id, on, start)) = press else { return };
            if on != Hit::Card {
                return;
            }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            if (point.x - start.x).hypot(point.y - start.y) < 4.0 {
                return;
            }
            let index = self.ivars().state.borrow().items.iter().position(|i| i.id == id);
            if let Some(index) = index {
                self.ivars().state.borrow_mut().press = None;
                self.start_drag(index, event);
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let press = self.ivars().state.borrow_mut().press.take();
            let Some((id, on, _)) = press else { return };
            let index = self.ivars().state.borrow().items.iter().position(|i| i.id == id);
            let Some(index) = index else { return };
            let option = event.modifierFlags().contains(NSEventModifierFlags::Option);
            match on {
                Hit::Close => self.take_off(index, option),
                Hit::Annotate => self.annotate(index),
                Hit::Card => {
                    if event.clickCount() >= 2 {
                        self.open(index);
                    } else {
                        self.reveal_in_finder(index);
                    }
                }
            }
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &AnyObject) {
            self.step();
        }
    }
);

define_class!(
    /// 飞行窗口：铺满整块屏幕，只为画那一张正在飞上来的卡片。
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = FlightIvars]
    struct FlightView;

    impl FlightView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            draw_flight();
        }
    }
);

impl FlightView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(FlightIvars::default());
        unsafe { msg_send![super(this), init] }
    }
}

impl LineView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(LineIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    /// 指针落在哪张卡片上（以及落在叉上还是标注上）。
    fn hit(&self, point: NSPoint) -> Option<(usize, Hit)> {
        let st = self.ivars().state.borrow();
        let dy = HIDDEN * (1.0 - st.slide);
        let (px, py) = (point.x, st.height - (point.y - dy));
        for (index, item) in st.items.iter().enumerate().rev() {
            if item.flying || item.fall.is_some() {
                continue;
            }
            let ay = rope_y(item.x, st.width) - PIN_ABOVE + item.drop;
            // 卡片是绕夹子顶边转的，先把指针转回去再判矩形。
            let (dx, dy2) = (px - item.x, py - ay);
            let a = (item.swing + item.tilt).to_radians();
            let (cos, sin) = (a.cos(), a.sin());
            let (lx, ly) = (dx * cos + dy2 * sin + item.x, -dx * sin + dy2 * cos + ay);
            let left = item.x - item.card_w() / 2.0;
            let top = ay + CARD_TOP;
            let inside =
                lx >= left && lx <= left + item.card_w() && ly >= top && ly <= top + item.card_h();
            if !inside {
                continue;
            }
            // 叉画在左上角、标注画在右上角，命中范围都比画出来的大一圈，好点。
            let hit = 26.0;
            let hovered = item.hover_mix > 0.5;
            if hovered
                && ly >= top - 3.0
                && ly <= top + 3.0 + hit
                && lx >= left - 3.0
                && lx <= left + 3.0 + hit
            {
                return Some((index, Hit::Close));
            }
            let right = left + item.card_w();
            if hovered
                && !item.video
                && ly >= top - 3.0
                && ly <= top + 3.0 + hit
                && lx >= right - 3.0 - hit
                && lx <= right + 3.0
            {
                return Some((index, Hit::Annotate));
            }
            return Some((index, Hit::Card));
        }
        None
    }

    fn step(&self) {
        let now = Instant::now();
        let mut st = self.ivars().state.borrow_mut();
        let dt = (now - st.last).as_secs_f64().clamp(0.0, 0.05);
        st.last = now;
        st.width = self.bounds().size.width;
        st.height = self.bounds().size.height;
        drop(st);

        self.drain_pending();
        self.update_reveal(now);
        let over_card = self.update_hover();

        let mut st = self.ivars().state.borrow_mut();
        let width = st.width;
        let count = st.items.len();
        if now >= st.breeze_at && !st.items.is_empty() && st.dragging.is_none() {
            for item in st.items.iter_mut() {
                if item.fall.is_some() {
                    continue;
                }
                let delay = pseudo(0.0, 0.35);
                let degrees = pseudo(1.6, 3.4);
                item.gust = Some((now + Duration::from_secs_f64(delay), degrees));
            }
            st.breeze_at = now + Duration::from_secs_f64(pseudo(7.0, 12.0));
        }
        let mut gone = Vec::new();
        for (index, item) in st.items.iter_mut().enumerate() {
            if let Some((at, degrees)) = item.gust {
                if now >= at {
                    nudge(&mut item.swing_v, degrees);
                    item.gust = None;
                }
            }
            let target = layout_x(index, count, width);
            let (dx, dx_v) = LAYOUT.step(item.x - target, item.x_v, dt);
            item.x = target + dx;
            item.x_v = dx_v;
            let (swing, swing_v) = SWING.step(item.swing, item.swing_v, dt);
            item.swing = swing;
            item.swing_v = swing_v;
            if !item.arrived {
                let (drop, drop_v) = ARRIVE.step(item.drop, item.drop_v, dt);
                item.drop = drop;
                item.drop_v = drop_v;
                if drop.abs() < 0.05 && drop_v.abs() < 0.5 {
                    item.drop = 0.0;
                    item.drop_v = 0.0;
                    item.arrived = true;
                }
            }
            let hovered = item.hover;
            let mix = item.hover_mix
                + ((if hovered { 1.0 } else { 0.0 }) - item.hover_mix) * (dt * 12.0).min(1.0);
            item.hover_mix = mix;
            if let Some(t) = item.fall {
                let t = t + dt;
                item.fall = Some(t);
                if t >= FALL_DUR {
                    gone.push(item.id);
                }
            }
            if let Some((_, at)) = item.bubble {
                if now.duration_since(at) > Duration::from_secs_f64(1.4) {
                    item.bubble = None;
                }
            }
        }
        if count > MAX_ITEMS {
            let live = st.items.iter().filter(|i| i.fall.is_none()).count();
            if live > MAX_ITEMS {
                if let Some(oldest) = st.items.iter_mut().find(|i| i.fall.is_none()) {
                    oldest.fall = Some(0.0);
                }
            }
        }
        st.items.retain(|i| !gone.contains(&i.id));
        let target = if st.revealed { 1.0 } else { 0.0 };
        let spring = if st.revealed { REVEAL } else { HIDE };
        let (slide, slide_v) = spring.step(st.slide - target, st.slide_v, dt);
        st.slide = target + slide;
        st.slide_v = slide_v;

        // 飞行：0.65 秒飞到绳子上的位置，落下时轻轻晃一下。
        let mut landed = false;
        if st.flight.active {
            st.flight.t += dt;
            if st.flight.t >= FLIGHT_DUR {
                st.flight.active = false;
                landed = true;
            }
        }
        drop(st);
        if landed {
            if let Some(item) = self.ivars().state.borrow_mut().items.last_mut() {
                item.flying = false;
                item.arrived = true;
                nudge(&mut item.swing_v, 2.2);
            }
        }

        let st = self.ivars().state.borrow();
        let visible = st.slide > 0.001 || st.revealed;
        let flying = st.flight.active;
        drop(st);
        if visible || flying {
            self.setNeedsDisplay(true);
        }
        // 绳子横贯屏幕：只有指针压在卡片上时窗口才收点击，其余地方一律穿过去。
        let want_clicks = visible && over_card;
        let mut st = self.ivars().state.borrow_mut();
        if want_clicks != st.clickable {
            st.clickable = want_clicks;
            drop(st);
            if let Some(panel) = panel_target() {
                panel.setIgnoresMouseEvents(!want_clicks);
            }
        }
        if flying {
            if let Some((panel, view)) = flight_targets() {
                view.setNeedsDisplay(true);
                if !panel.isVisible() {
                    panel.orderFrontRegardless();
                }
            }
        } else if let Some(panel) = flight_targets().map(|(p, _)| p) {
            if panel.isVisible() {
                panel.orderOut(None);
            }
        }
        if visible {
            if let Some(panel) = panel_target() {
                if !panel.isVisible() {
                    panel.orderFrontRegardless();
                }
            }
        } else if let Some(panel) = panel_target() {
            if panel.isVisible() {
                panel.orderOut(None);
            }
        }
    }

    /// 后台生成好的缩略图在这里变成卡片，并从录制的那块区域飞上来。
    fn drain_pending(&self) {
        let pending: Vec<Pending> =
            std::mem::take(&mut PENDING.lock().unwrap_or_else(|e| e.into_inner()));
        for item in pending {
            self.hang(item);
        }
    }

    fn hang(&self, pending: Pending) {
        if !self.ivars().state.borrow().enabled {
            return;
        }
        let full_w = CGImage::width(Some(&pending.image)) as f64;
        let full_h = CGImage::height(Some(&pending.image)) as f64;
        let card_ratio = (CARD_W - 14.0) / PHOTO_MAX_H;
        let crop_h = (full_w / card_ratio).min(full_h);
        let cropped = (crop_h + 0.5 < full_h).then(|| {
            CGImage::with_image_in_rect(
                Some(&pending.image),
                CGRect::new(
                    CGPoint::new(0.0, 0.0),
                    CGSize::new(full_w, crop_h),
                ),
            )
        });
        let source: &CGImage = cropped
            .as_ref()
            .and_then(|image| image.as_deref())
            .unwrap_or(&pending.image);
        let px = full_w / 2.0;
        let py = crop_h / 2.0;
        let image = NSImage::initWithCGImage_size(
            NSImage::alloc(),
            source,
            NSSize::new(px.max(1.0), py.max(1.0)),
        );
        let photo = fit(NSSize::new(px.max(1.0), py.max(1.0)));
        let tilt = pseudo(-2.5, 2.5);
        let mut st = self.ivars().state.borrow_mut();
        let id = st.next_id;
        st.next_id += 1;
        let start_x = layout_x(st.items.len(), st.items.len() + 1, st.width);
        // 新的一律挂在最右边。
        st.items.push(Peg {
            id,
            path: pending.path,
            image: image.clone(),
            photo,
            video: pending.video,
            tilt,
            swing: 0.0,
            swing_v: 0.0,
            drop: 0.0,
            drop_v: 0.0,
            x: start_x,
            x_v: 0.0,
            arrived: false,
            flying: true,
            fall: None,
            hover: false,
            hover_mix: 0.0,
            gust: None,
            bubble: None,
        });
        if st.items.len() > MAX_ITEMS {
            if let Some(oldest) = st.items.iter_mut().find(|i| i.fall.is_none()) {
                oldest.fall = Some(0.0);
            }
        }
        let card = NSSize::new(photo.width + INSET * 2.0, photo.height + INSET * 2.0);
        let (from, to) = flight_rects(&st, pending.region, card);
        st.flight = Flight {
            active: true,
            image: Some(image),
            from,
            to,
            tilt,
            t: 0.0,
        };
        if st.enabled {
            st.revealed = true;
            st.peek_until = Some(Instant::now() + Duration::from_secs_f64(PEEK));
            st.away = None;
        }
        drop(st);
        // 绳子可能还没放到这块屏幕上，先摆好再飞。
        self.place();
    }

    /// 把绳子窗口摆到指针所在那块屏幕的顶上。
    fn place(&self) {
        let mtm = self.mtm();
        let Some(screen) = screen_under_pointer(mtm) else {
            return;
        };
        let visible = screen.visibleFrame();
        let frame = NSRect::new(
            NSPoint::new(visible.origin.x, max_y(&visible) - PANEL_H),
            NSSize::new(visible.size.width, PANEL_H),
        );
        {
            let mut st = self.ivars().state.borrow_mut();
            st.screen = screen.frame();
            st.panel_top = max_y(&screen.frame()) - max_y(&visible);
        }
        if let Some(panel) = panel_target() {
            if rect_eq(panel.frame(), frame) {
                return;
            }
            panel.setFrame_display(frame, true);
        }
        if let Some(panel) = flight_targets().map(|(p, _)| p) {
            let f = screen.frame();
            if rect_eq(panel.frame(), f) {
                return;
            }
            panel.setFrame_display(f, true);
        }
    }

    fn update_reveal(&self, now: Instant) {
        let mtm = self.mtm();
        let mouse = NSEvent::mouseLocation();
        let Some(screen) = screen_under_pointer(mtm) else {
            return;
        };
        let frame = screen.frame();
        let band = menu_bar_band(&screen);
        let mut st = self.ivars().state.borrow_mut();
        st.screen = frame;
        if let Some(panel) = panel_target() {
            let p = panel.frame();
            st.pointer = Some(NSPoint::new(mouse.x - p.origin.x, mouse.y - p.origin.y));
        } else {
            st.pointer = None;
        }
        let in_band = point_in(mouse, band);
        if !in_band {
            st.suppressed = false;
        }
        if NSEvent::pressedMouseButtons() != 0 && in_band {
            // 在菜单栏里点了一下：收起来，等指针离开再恢复正常。
            st.suppressed = true;
            st.revealed = false;
            st.pinned = false;
            st.peek_until = None;
            st.dwell = None;
        }
        if !st.enabled || st.busy {
            st.revealed = false;
            st.peek_until = None;
            st.dwell = None;
            return;
        }
        let peeking = st.peek_until.map(|until| now < until).unwrap_or(false);
        if !st.revealed && st.hover_enabled && in_band && !st.suppressed {
            let since = st.dwell.unwrap_or(now);
            st.dwell = Some(since);
            if now.duration_since(since) >= Duration::from_secs_f64(REVEAL_DELAY) {
                st.dwell = None;
                st.revealed = true;
                st.away = None;
                drop(st);
                self.place();
                return;
            }
        } else {
            st.dwell = None;
        }
        if st.revealed && !st.pinned && !peeking && st.dragging.is_none() {
            let mut zone = NSRect::new(
                NSPoint::new(band.origin.x, 0.0),
                NSSize::new(band.size.width, 0.0),
            );
            zone.origin.y = panel_target()
                .map(|p| p.frame().origin.y)
                .unwrap_or(max_y(&frame) - PANEL_H);
            zone.size.height = max_y(&frame) - zone.origin.y;
            if point_in(mouse, zone) {
                st.away = None;
            } else {
                let since = st.away.unwrap_or(now);
                st.away = Some(since);
                if now.duration_since(since) >= Duration::from_secs_f64(RETRACT_DELAY) {
                    st.revealed = false;
                    st.away = None;
                }
            }
        }
    }

    /// 指针悬停：卡片轻轻抬起、叉子和标注浮现，光标变成小手。返回指针是否压在某张卡片上。
    fn update_hover(&self) -> bool {
        let pointer = {
            let st = self.ivars().state.borrow();
            if st.slide < 0.6 { None } else { st.pointer }
        };
        let hit = pointer.and_then(|p| self.hit(p));
        let mut st = self.ivars().state.borrow_mut();
        for (index, item) in st.items.iter_mut().enumerate() {
            item.hover = matches!(hit, Some((i, _)) if i == index);
        }
        let hand = hit.is_some();
        if hand != st.hand {
            st.hand = hand;
            if hand {
                NSCursor::pointingHandCursor().set();
            } else {
                NSCursor::arrowCursor().set();
            }
        }
        hit.is_some()
    }

    /// 开始一次拖放：把文件 URL 交给系统，拖动预览用这张卡片的画面。
    fn start_drag(&self, index: usize, event: &NSEvent) {
        let (id, path, image, frame) = {
            let st = self.ivars().state.borrow();
            let Some(item) = st.items.get(index) else {
                return;
            };
            if item.flying || item.fall.is_some() {
                return;
            }
            let ay = rope_y(item.x, st.width) - PIN_ABOVE + item.drop;
            let card = flip(
                st.height,
                item.x - item.card_w() / 2.0,
                ay + CARD_TOP,
                item.card_w(),
                item.card_h(),
            );
            // 预览保持画面比例，居中铺在卡片里。
            let size = item.image.size();
            let (w, h) = if size.width > 0.0 && size.height > 0.0 {
                let scale = (card.size.width / size.width).min(card.size.height / size.height);
                (size.width * scale, size.height * scale)
            } else {
                (card.size.width, card.size.height)
            };
            let frame = NSRect::new(
                NSPoint::new(
                    card.origin.x + (card.size.width - w) / 2.0,
                    card.origin.y + (card.size.height - h) / 2.0,
                ),
                NSSize::new(w, h),
            );
            (item.id, item.path.clone(), item.image.clone(), frame)
        };
        let url =
            NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), false);
        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*url);
        let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
        unsafe { item.setDraggingFrame_contents(frame, Some(&*image as &AnyObject)) };
        let items = NSArray::from_retained_slice(&[item]);
        // 没人接收时卡片飞回绳上。
        let session = self.beginDraggingSessionWithItems_event_source(
            &items,
            event,
            ProtocolObject::from_ref(self),
        );
        session.setAnimatesToStartingPositionsOnCancelOrFail(true);
        self.ivars().state.borrow_mut().dragging = Some(id);
    }

    /// 拖放结束。卡片只在手动取下或清空时才离开绳子，所以拖进文件夹、对话框
    /// 之后它照样挂着——那是把文件送出去，不是丢掉。唯一例外是拖到 Dock 的
    /// 废纸篓：那是明确要删，文件挪进废纸篓、卡片下绳。
    fn drag_ended(&self, operation: NSDragOperation) {
        let Some(id) = self.ivars().state.borrow_mut().dragging.take() else {
            return;
        };
        if operation.contains(NSDragOperation::Delete) {
            if let Some(index) = self
                .ivars()
                .state
                .borrow()
                .items
                .iter()
                .position(|i| i.id == id)
            {
                self.take_off(index, true);
            }
        }
    }

    /// 把这张卡片交给系统标注，改完把新的缩略图换上去。
    fn annotate(&self, index: usize) {
        let path = self.path_at(index);
        let Some(path) = path else { return };
        crate::markup::edit(path, |path| {
            crate::line::refresh(path);
        });
    }

    /// 文件被改过（比如标注完）：重新读一次图，换掉卡片上的缩略图。
    fn refresh_image(&self, path: PathBuf) {
        let Some(image) = NSImage::initWithContentsOfFile(
            NSImage::alloc(),
            &NSString::from_str(&path.to_string_lossy()),
        ) else {
            return;
        };
        let photo = fit(image.size());
        let mut st = self.ivars().state.borrow_mut();
        if let Some(item) = st.items.iter_mut().find(|i| i.path == path) {
            item.image = image;
            item.photo = photo;
            item.bubble = Some(("已保存".to_string(), Instant::now()));
            nudge(&mut item.swing_v, 3.0);
        }
    }

    /// 卡片下面冒一句话，顺便让它轻轻晃一下。
    fn note(&self, index: usize, text: &str) {
        let mut st = self.ivars().state.borrow_mut();
        if let Some(item) = st.items.get_mut(index) {
            item.bubble = Some((text.to_string(), Instant::now()));
            nudge(&mut item.swing_v, 3.0);
        }
    }

    /// 叉：从绳上取下（按住 ⌥ 连文件一起丢进废纸篓）。
    fn take_off(&self, index: usize, trash: bool) {
        let path = {
            let mut st = self.ivars().state.borrow_mut();
            let Some(item) = st.items.get_mut(index) else {
                return;
            };
            if item.fall.is_some() {
                return;
            }
            item.fall = Some(0.0);
            item.hover = false;
            item.path.clone()
        };
        if trash {
            let url = NSURL::fileURLWithPath_isDirectory(
                &NSString::from_str(&path.to_string_lossy()),
                false,
            );
            let _ = objc2_foundation::NSFileManager::defaultManager()
                .trashItemAtURL_resultingItemURL_error(&url, None);
        }
    }

    fn open(&self, index: usize) {
        let path = self.path_at(index);
        let Some(path) = path else { return };
        if !path.exists() {
            self.note(index, "文件已不在原处");
            return;
        }
        let url = NSURL::fileURLWithPath_isDirectory(
            &NSString::from_str(&path.to_string_lossy()),
            false,
        );
        let _ = NSWorkspace::sharedWorkspace().openURL(&url);
    }

    fn reveal_in_finder(&self, index: usize) {
        let path = self.path_at(index);
        let Some(path) = path else { return };
        if !path.exists() {
            self.note(index, "文件已不在原处");
            return;
        }
        let url =
            NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), false);
        NSWorkspace::sharedWorkspace()
            .activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
        let mut st = self.ivars().state.borrow_mut();
        if let Some(item) = st.items.get_mut(index) {
            item.bubble = Some(("已显示".to_string(), Instant::now()));
            nudge(&mut item.swing_v, 3.0);
        }
    }

    fn path_at(&self, index: usize) -> Option<PathBuf> {
        let st = self.ivars().state.borrow();
        st.items.get(index).map(|i| i.path.clone())
    }

    fn set_enabled(&self, enabled: bool, hover: bool) {
        let mut st = self.ivars().state.borrow_mut();
        st.enabled = enabled;
        st.hover_enabled = hover;
        if !enabled {
            st.revealed = false;
            st.pinned = false;
            st.peek_until = None;
        }
    }

    fn set_busy(&self, busy: bool) {
        let mut st = self.ivars().state.borrow_mut();
        st.busy = busy;
        if busy {
            st.revealed = false;
            st.pinned = false;
            st.peek_until = None;
        }
    }

    fn toggle_pinned(&self) {
        let mut st = self.ivars().state.borrow_mut();
        st.pinned = !st.pinned;
        st.revealed = st.pinned;
        st.away = None;
        st.peek_until = None;
        if st.pinned {
            st.dwell = None;
        }
    }

    fn clear(&self) {
        let mut st = self.ivars().state.borrow_mut();
        for item in st.items.iter_mut() {
            if item.fall.is_none() {
                item.fall = Some(0.0);
            }
        }
    }
}

// MARK: - 小工具

fn max_y(r: &NSRect) -> f64 {
    r.origin.y + r.size.height
}

fn min_x(r: &NSRect) -> f64 {
    r.origin.x
}

fn max_x(r: &NSRect) -> f64 {
    r.origin.x + r.size.width
}

fn point_in(p: NSPoint, r: NSRect) -> bool {
    p.x >= min_x(&r) && p.x <= max_x(&r) && p.y >= r.origin.y && p.y <= max_y(&r)
}

fn rect_eq(a: NSRect, b: NSRect) -> bool {
    (a.origin.x - b.origin.x).abs() < 0.5
        && (a.origin.y - b.origin.y).abs() < 0.5
        && (a.size.width - b.size.width).abs() < 0.5
        && (a.size.height - b.size.height).abs() < 0.5
}

/// 顶空间的矩形翻成视图坐标（y 向上）。
fn flip(height: f64, x: f64, top: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, height - top - h), NSSize::new(w, h))
}

fn inset_rect(r: NSRect, d: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(r.origin.x + d, r.origin.y + d),
        NSSize::new(r.size.width - d * 2.0, r.size.height - d * 2.0),
    )
}

/// 照片按比例塞进卡片，四周留白 hug 住它（Tendedero 的 photoSize）。
fn fit(size: NSSize) -> NSSize {
    let max_w = CARD_W - 14.0;
    let max_h = PHOTO_MAX_H;
    if size.width <= 0.0 || size.height <= 0.0 {
        return NSSize::new(max_w, max_h);
    }
    let scale = (max_w / size.width).min(max_h / size.height);
    NSSize::new(size.width * scale, size.height * scale)
}

/// 没有随机数库，用时间搓一个够用的伪随机数。
fn pseudo(from: f64, to: f64) -> f64 {
    let nanos = Instant::now().elapsed().subsec_nanos();
    let unit = ((nanos % 100_000) as f64) / 100_000.0;
    from + (to - from) * unit
}

fn ease_in_out_cubic(x: f64) -> f64 {
    if x < 0.5 {
        4.0 * x * x * x
    } else {
        1.0 - (-2.0 * x + 2.0).powi(3) / 2.0
    }
}

fn smooth(from: f64, to: f64, x: f64) -> f64 {
    let t = ((x - from) / (to - from)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 绳子两端渐渐淡出，看着像从屏幕外伸过来。
fn end_fade(t: f64) -> f64 {
    let e = 0.08;
    ((t / e).min((1.0 - t) / e)).clamp(0.0, 1.0)
}

fn current_ctx() -> Option<Retained<CGContext>> {
    NSGraphicsContext::currentContext().map(|c| c.CGContext())
}

fn screen_under_pointer(mtm: MainThreadMarker) -> Option<Retained<NSScreen>> {
    let mouse = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    for i in 0..screens.count() {
        let screen = screens.objectAtIndex(i);
        if point_in(mouse, screen.frame()) {
            return Some(screen);
        }
    }
    NSScreen::mainScreen(mtm)
}

/// 菜单栏那一条；菜单栏自动隐藏时退回一个大概的厚度。
fn menu_bar_band(screen: &NSScreen) -> NSRect {
    let f = screen.frame();
    let v = screen.visibleFrame();
    let mut h = max_y(&f) - max_y(&v);
    if h < 1.0 {
        h = 25.0;
    }
    NSRect::new(
        NSPoint::new(f.origin.x, max_y(&f) - h),
        NSSize::new(f.size.width, h),
    )
}

// MARK: - 飞行

/// 起飞处（录的那块区域）和目标卡片，都换算成飞行窗口的视图坐标。
fn flight_rects(st: &LineState, region: Option<Rect>, card: NSSize) -> (NSRect, NSRect) {
    let screen = st.screen;
    let sh = screen.size.height.max(1.0);
    let from_top = match region {
        Some(r) => NSRect::new(
            NSPoint::new(r.x, r.y),
            NSSize::new(r.w.max(2.0), r.h.max(2.0)),
        ),
        None => NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(screen.size.width.max(2.0), sh),
        ),
    };
    let index = st.items.len().saturating_sub(1);
    let x = layout_x(index, st.items.len().max(1), st.width);
    let top = rope_y(x, st.width) - PIN_ABOVE + CARD_TOP;
    let to_top = NSRect::new(
        NSPoint::new(x + (min_x(&screen) * 0.0), st.panel_top + top),
        card,
    );
    (
        flip(
            sh,
            from_top.origin.x,
            from_top.origin.y,
            from_top.size.width,
            from_top.size.height,
        ),
        flip(
            sh,
            to_top.origin.x,
            to_top.origin.y,
            card.width,
            card.height,
        ),
    )
}

fn draw_flight() {
    let Some(ctx) = current_ctx() else { return };
    let Some(line) = line_view() else { return };
    let st = line.ivars().state.borrow();
    let f = &st.flight;
    if !f.active {
        return;
    }
    let Some(image) = f.image.as_ref() else {
        return;
    };
    let k = ease_in_out_cubic((f.t / FLIGHT_DUR).clamp(0.0, 1.0));
    let lerp = |a: f64, b: f64| a + (b - a) * k;
    let w = lerp(f.from.size.width, f.to.size.width);
    let h = lerp(f.from.size.height, f.to.size.height);
    let x = lerp(f.from.origin.x, f.to.origin.x);
    let y = lerp(f.from.origin.y, f.to.origin.y) + ARC * (std::f64::consts::PI * k).sin();
    let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
    let radius = RADIUS * k;
    let inset = INSET * k;
    let chrome = smooth(0.35, 1.0, k);
    let angle = -f.tilt * k;
    CGContext::save_g_state(Some(&ctx));
    CGContext::translate_ctm(Some(&ctx), rect.origin.x + w / 2.0, rect.origin.y + h);
    CGContext::rotate_ctm(Some(&ctx), angle.to_radians());
    CGContext::translate_ctm(Some(&ctx), -(rect.origin.x + w / 2.0), -(rect.origin.y + h));
    draw_card(Some(&ctx), rect, Some(image), radius, inset, chrome, 0.0);
    if chrome > 0.01 {
        draw_clip(Some(&ctx), rect, chrome);
    }
    CGContext::restore_g_state(Some(&ctx));
}

// MARK: - 画

fn draw_line(view: &LineView) {
    let Some(ctx) = current_ctx() else { return };
    let st = view.ivars().state.borrow();
    let (w, h) = (st.width, st.height);
    let dy = HIDDEN * (1.0 - st.slide);
    CGContext::save_g_state(Some(&ctx));
    CGContext::translate_ctm(Some(&ctx), 0.0, dy);
    draw_rope(w, h);
    if st.items.is_empty() {
        draw_hint(w, h);
    }
    for item in st.items.iter() {
        if item.flying {
            continue;
        }
        let fall = item.fall.map(|t| (t / FALL_DUR).clamp(0.0, 1.0).powi(3));
        let ay = rope_y(item.x, w) - PIN_ABOVE + item.drop + FALL_DISTANCE * fall.unwrap_or(0.0);
        let extra_tilt = fall.map(|e| (item.tilt * 7.0 + 20.0) * e).unwrap_or(0.0);
        let alpha =
            ((item.x.min(w - item.x) / 90.0).min(1.0)).max(0.0) * (1.0 - fall.unwrap_or(0.0));
        if alpha <= 0.01 {
            continue;
        }
        let card = flip(
            h,
            item.x - item.card_w() / 2.0,
            ay + CARD_TOP,
            item.card_w(),
            item.card_h(),
        );
        let theta = item.swing + item.tilt + extra_tilt;
        let scale = 1.0 + 0.035 * item.hover_mix;
        CGContext::save_g_state(Some(&ctx));
        CGContext::set_alpha(Some(&ctx), alpha);
        CGContext::translate_ctm(Some(&ctx), item.x, h - ay);
        CGContext::rotate_ctm(Some(&ctx), -theta.to_radians());
        CGContext::scale_ctm(Some(&ctx), scale, scale);
        CGContext::translate_ctm(Some(&ctx), -item.x, -(h - ay));
        draw_card(
            Some(&ctx),
            card,
            Some(&item.image),
            RADIUS,
            INSET,
            1.0,
            item.hover_mix,
        );
        draw_clip(Some(&ctx), card, 1.0);
        if item.video {
            draw_play(card, 1.0);
        }
        if item.hover_mix > 0.01 {
            draw_close(card, item.hover_mix);
            if !item.video {
                draw_annotate(card, item.hover_mix, st.annotate_icon.as_deref());
            }
        }
        if let Some((text, _)) = item.bubble.as_ref() {
            draw_bubble(card, text);
        }
        CGContext::restore_g_state(Some(&ctx));
    }
    CGContext::restore_g_state(Some(&ctx));
}

fn draw_rope(w: f64, h: f64) {
    if w <= 0.0 {
        return;
    }
    let steps = 72usize;
    let ctrl = ROPE_TOP + 2.0 * sag(w);
    let (x0, x2) = (-20.0, w + 20.0);
    let point = |t: f64| -> (f64, f64) {
        let u = 1.0 - t;
        (
            u * u * x0 + 2.0 * u * t * (w / 2.0) + t * t * x2,
            u * u * ROPE_TOP + 2.0 * u * t * ctrl + t * t * ROPE_TOP,
        )
    };
    let mut pts = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        pts.push(point(i as f64 / steps as f64));
    }
    // 三遍：投影、绳子本体、上面那道高光。
    let passes: [(f64, f64, f64, f64); 3] = [
        (0.22, 1.4, -1.2, 0.0),
        (1.0, 1.2, 0.0, 0.55),
        (0.45, 0.4, 0.35, 1.0),
    ];
    for (alpha, width, shift, gray) in passes {
        for i in 0..steps {
            let (ax, ay) = pts[i];
            let (bx, by) = pts[i + 1];
            let a = alpha * end_fade(ax / w).min(end_fade(bx / w));
            if a <= 0.004 {
                continue;
            }
            let path = NSBezierPath::bezierPath();
            path.setLineWidth(width);
            path.moveToPoint(NSPoint::new(ax, h - ay + shift));
            path.lineToPoint(NSPoint::new(bx, h - by + shift));
            if gray > 0.9 {
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, a).setStroke();
            } else {
                NSColor::colorWithSRGBRed_green_blue_alpha(gray, gray, gray, a).setStroke();
            }
            path.stroke();
        }
    }
}

/// 玻璃卡片：先填一块带投影的底，再把画面裁进去，最后描一圈高光边。
fn draw_card(
    ctx: Option<&CGContext>,
    card: NSRect,
    image: Option<&NSImage>,
    radius: f64,
    inset: f64,
    chrome: f64,
    lift: f64,
) {
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(card, radius, radius);
    CGContext::save_g_state(ctx);
    let shadow = NSShadow::new();
    shadow.setShadowColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.0,
        0.0,
        0.0,
        0.18 + 0.08 * lift,
    )));
    shadow.setShadowOffset(NSSize::new(0.0, -5.0 - 3.0 * lift));
    shadow.setShadowBlurRadius(10.0 + 4.0 * lift);
    shadow.set();
    NSColor::colorWithSRGBRed_green_blue_alpha(0.97, 0.98, 1.0, 0.72).setFill();
    path.fill();
    CGContext::restore_g_state(ctx);

    if let Some(image) = image {
        let inner = inset_rect(card, inset);
        let r = (radius - inset).max(0.0);
        let inner_path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(inner, r, r);
        CGContext::save_g_state(ctx);
        inner_path.addClip();
        image.drawInRect(inner);
        CGContext::restore_g_state(ctx);
        inner_path.setLineWidth(0.5);
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.18 * chrome).setStroke();
        inner_path.stroke();
    }

    path.setLineWidth(0.75);
    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.55 * chrome).setStroke();
    path.stroke();
    let outer = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
        inset_rect(card, -0.5),
        radius + 0.5,
        radius + 0.5,
    );
    outer.setLineWidth(0.5);
    NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.10 * chrome).setStroke();
    outer.stroke();
}

/// 夹子：一枚拉丝金属的小夹子，顶上有一道夹着绳子的缝。
fn draw_clip(ctx: Option<&CGContext>, card: NSRect, chrome: f64) {
    let size = NSSize::new(9.0, 26.0);
    let r = NSRect::new(
        NSPoint::new(
            card.origin.x + card.size.width / 2.0 - size.width / 2.0,
            card.origin.y + card.size.height - 12.0,
        ),
        size,
    );
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, 3.5, 3.5);
    CGContext::save_g_state(ctx);
    path.addClip();
    let metal = NSGradient::initWithStartingColor_endingColor(
        NSGradient::alloc(),
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.70, 0.70, 0.72, chrome),
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.93, 0.93, 0.95, chrome),
    );
    if let Some(metal) = metal {
        metal.drawInRect_angle(r, 0.0);
    }
    CGContext::restore_g_state(ctx);
    path.setLineWidth(0.6);
    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.85 * chrome).setStroke();
    path.stroke();
    let slot = NSRect::new(
        NSPoint::new(r.origin.x + 2.0, r.origin.y + size.height - 8.5 - 1.4),
        NSSize::new(5.0, 1.4),
    );
    let slot_path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(slot, 0.7, 0.7);
    NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.32 * chrome).setFill();
    slot_path.fill();
}

/// 视频都是「第一帧 + 一个播放键」。
fn draw_play(card: NSRect, chrome: f64) {
    let size = 28.0;
    let r = NSRect::new(
        NSPoint::new(
            card.origin.x + (card.size.width - size) / 2.0,
            card.origin.y + (card.size.height - size) / 2.0,
        ),
        NSSize::new(size, size),
    );
    let circle = NSBezierPath::bezierPathWithOvalInRect(r);
    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.92 * chrome).setFill();
    circle.fill();
    circle.setLineWidth(0.5);
    NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.10 * chrome).setStroke();
    circle.stroke();
    let (cx, cy) = (r.origin.x + size / 2.0 + 1.2, r.origin.y + size / 2.0);
    let triangle = NSBezierPath::bezierPath();
    triangle.moveToPoint(NSPoint::new(cx - 3.0, cy - 5.0));
    triangle.lineToPoint(NSPoint::new(cx + 5.0, cy));
    triangle.lineToPoint(NSPoint::new(cx - 3.0, cy + 5.0));
    triangle.closePath();
    NSColor::colorWithSRGBRed_green_blue_alpha(0.10, 0.11, 0.13, 0.88 * chrome).setFill();
    triangle.fill();
}

fn draw_close(card: NSRect, mix: f64) {
    let size = 20.0 * (0.6 + 0.4 * mix);
    let r = NSRect::new(
        NSPoint::new(
            card.origin.x + 3.0,
            card.origin.y + card.size.height - 3.0 - size,
        ),
        NSSize::new(size, size),
    );
    let circle = NSBezierPath::bezierPathWithOvalInRect(r);
    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.92 * mix).setFill();
    circle.fill();
    circle.setLineWidth(0.5);
    NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.12 * mix).setStroke();
    circle.stroke();
    let m = size * 0.32;
    let (cx, cy) = (r.origin.x + size / 2.0, r.origin.y + size / 2.0);
    let cross = NSBezierPath::bezierPath();
    cross.setLineWidth(1.6);
    cross.moveToPoint(NSPoint::new(cx - m, cy - m));
    cross.lineToPoint(NSPoint::new(cx + m, cy + m));
    cross.moveToPoint(NSPoint::new(cx + m, cy - m));
    cross.lineToPoint(NSPoint::new(cx - m, cy + m));
    NSColor::colorWithSRGBRed_green_blue_alpha(0.15, 0.16, 0.18, 0.85 * mix).setStroke();
    cross.stroke();
}

fn text_attrs(size: f64, white: bool) -> Retained<NSMutableDictionary<NSString, AnyObject>> {
    let attrs: Retained<NSMutableDictionary<NSString, AnyObject>> = NSMutableDictionary::new();
    unsafe {
        let font = NSFont::systemFontOfSize(size);
        let object: &AnyObject = &*font;
        attrs.setObject_forKey(object, ProtocolObject::from_ref(NSFontAttributeName));
        let color = if white {
            NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.95)
        } else {
            NSColor::colorWithSRGBRed_green_blue_alpha(0.08, 0.09, 0.11, 0.9)
        };
        let object: &AnyObject = &*color;
        attrs.setObject_forKey(
            object,
            ProtocolObject::from_ref(NSForegroundColorAttributeName),
        );
    }
    attrs
}

/// 右上角的「标注」按钮：一支铅笔，点了就打开系统标注。
fn draw_annotate(card: NSRect, mix: f64, icon: Option<&NSImage>) {
    let size = 20.0 * (0.6 + 0.4 * mix);
    let r = NSRect::new(
        NSPoint::new(
            card.origin.x + card.size.width - 3.0 - size,
            card.origin.y + card.size.height - 3.0 - size,
        ),
        NSSize::new(size, size),
    );
    let circle = NSBezierPath::bezierPathWithOvalInRect(r);
    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.92 * mix).setFill();
    circle.fill();
    circle.setLineWidth(0.5);
    NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.12 * mix).setStroke();
    circle.stroke();
    if let Some(icon) = icon {
        icon.drawInRect(inset_rect(r, size * 0.26));
    }
}

fn draw_bubble(card: NSRect, text: &str) {
    let string = NSString::from_str(text);
    let attrs = text_attrs(11.0, true);
    let size = unsafe { string.sizeWithAttributes(Some(&attrs)) };
    let w = size.width + 20.0;
    let h = size.height + 10.0;
    let r = NSRect::new(
        NSPoint::new(
            card.origin.x + (card.size.width - w) / 2.0,
            card.origin.y - 16.0 - h,
        ),
        NSSize::new(w, h),
    );
    let capsule = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, h / 2.0, h / 2.0);
    NSColor::colorWithSRGBRed_green_blue_alpha(0.10, 0.11, 0.13, 0.78).setFill();
    capsule.fill();
    unsafe {
        string.drawAtPoint_withAttributes(
            NSPoint::new(r.origin.x + 10.0, r.origin.y + 5.0),
            Some(&attrs),
        );
    }
}

fn draw_hint(w: f64, h: f64) {
    let text = NSString::from_str("录完一段视频，它会挂在这里");
    let attrs = text_attrs(12.0, false);
    let size = unsafe { text.sizeWithAttributes(Some(&attrs)) };
    let bw = size.width + 24.0;
    let bh = size.height + 12.0;
    let r = NSRect::new(
        NSPoint::new(w / 2.0 - bw / 2.0, h - rope_y(w / 2.0, w) - 34.0 - bh),
        NSSize::new(bw, bh),
    );
    let capsule = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, bh / 2.0, bh / 2.0);
    NSColor::colorWithSRGBRed_green_blue_alpha(0.92, 0.93, 0.95, 0.72).setFill();
    capsule.fill();
    unsafe {
        text.drawAtPoint_withAttributes(
            NSPoint::new(r.origin.x + 12.0, r.origin.y + 6.0),
            Some(&attrs),
        );
    }
}

// MARK: - 对外接口

fn with<R>(f: impl FnOnce(&Line) -> R) -> Option<R> {
    let guard = LINE.lock().ok()?;
    guard.as_ref().map(f)
}

fn panel_target() -> Option<Retained<LinePanel>> {
    with(|l| l.panel.clone())
}

fn flight_targets() -> Option<(Retained<LinePanel>, Retained<FlightView>)> {
    with(|l| (l.flight_panel.clone(), l.flight_view.clone()))
}

fn line_view() -> Option<Retained<LineView>> {
    with(|l| l.view.clone())
}

fn make_panel(mtm: MainThreadMarker) -> Retained<LinePanel> {
    let panel: Retained<LinePanel> = unsafe {
        msg_send![
            super(LinePanel::alloc(mtm).set_ivars(PanelIvars::default())),
            initWithContentRect: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, PANEL_H)),
            styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ]
    };
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(false);
    panel.setLevel(NSFloatingWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel.setHidesOnDeactivate(false);
    // 绳子不进录制画面（sharingType None）；鼠标默认穿透，由 hitTest 决定哪里能点。
    panel.setSharingType(objc2_app_kit::NSWindowSharingType(0));
    panel.setIgnoresMouseEvents(true);
    unsafe { panel.setReleasedWhenClosed(false) };
    panel
}

/// 建好两根窗口（绳子 + 飞行），并且挂上 60 fps 的驱动。
pub fn init(mtm: MainThreadMarker) {
    let panel = make_panel(mtm);
    let view = LineView::new(mtm);
    view.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(1440.0, PANEL_H),
    ));
    panel.setContentView(Some(&view));

    let flight_panel = make_panel(mtm);
    flight_panel.setLevel(NSFloatingWindowLevel + 1);
    let flight_view = FlightView::new(mtm);
    flight_view.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(1440.0, 900.0),
    ));
    flight_panel.setContentView(Some(&flight_view));

    view.ivars().state.borrow_mut().annotate_icon =
        NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("pencil"),
            None,
        );

    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            1.0 / 60.0,
            &view,
            sel!(tick:),
            None,
            true,
        )
    };
    std::mem::forget(timer);

    *LINE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Line {
        panel,
        view: view.clone(),
        flight_panel,
        flight_view,
    });
    view.place();
}

/// 录完一段：后台取第一帧，主线程把它挂上去。
pub fn hang_later(path: PathBuf, region: Option<Rect>) {
    std::thread::spawn(move || match first_frame(&path) {
        Ok(image) => {
            PENDING
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Pending {
                    image,
                    path,
                    region,
                    video: true,
                });
        }
        Err(err) => crate::state::log(&format!("line: 缩略图生成失败：{err}")),
    });
}

/// 文件被改过之后刷新缩略图（主线程）。
pub fn refresh(path: PathBuf) {
    if let Some(view) = line_view() {
        view.refresh_image(path);
    }
}

/// 截图已经拿到图像了，直接挂上去（可以在任何线程调用）。
pub fn hang_image(image: Retained<CGImage>, path: PathBuf, region: Option<Rect>) {
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Pending {
            image,
            path,
            region,
            video: false,
        });
}

#[allow(deprecated)]
fn first_frame(path: &std::path::Path) -> Result<Retained<CGImage>, String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "heic" | "tif" | "tiff"
    ) {
        let image = NSImage::initWithContentsOfFile(
            NSImage::alloc(),
            &NSString::from_str(&path.to_string_lossy()),
        )
        .ok_or_else(|| "无法读取图片".to_string())?;
        return unsafe {
            image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None)
        }
        .ok_or_else(|| "无法生成图片缩略图".to_string());
    }
    let url =
        NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), false);
    let asset = unsafe { AVURLAsset::assetWithURL(&url) };
    let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset) };
    unsafe {
        generator.setAppliesPreferredTrackTransform(true);
        generator.setMaximumSize(CGSize {
            width: (CARD_W - 14.0) * 2.0,
            height: PHOTO_MAX_H * 2.0,
        });
        generator
            .copyCGImageAtTime_actualTime_error(CMTime::new(1, 30), std::ptr::null_mut())
            .map_err(|e| e.to_string())
    }
}

pub fn apply_settings(enabled: bool, hover: bool) {
    if let Some(view) = line_view() {
        view.set_enabled(enabled, hover);
    }
}

/// 录制中把绳子收起来，免得挡着（窗口本身也不会被录进去）。
pub fn set_busy(busy: bool) {
    if let Some(view) = line_view() {
        view.set_busy(busy);
    }
}

pub fn toggle_pinned() {
    if let Some(view) = line_view() {
        view.toggle_pinned();
    }
}

pub fn clear() {
    if let Some(view) = line_view() {
        view.clear();
    }
}

/// （启用，指针悬停即放下，当前是否放下）
pub fn status() -> (bool, bool, bool) {
    match line_view() {
        Some(view) => {
            let st = view.ivars().state.borrow();
            (st.enabled, st.hover_enabled, st.revealed || st.pinned)
        }
        None => (true, true, false),
    }
}
