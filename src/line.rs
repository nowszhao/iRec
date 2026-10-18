//! 晾衣绳：录完的视频挂在菜单栏下面的一根绳子上。
//!
//! 动画的创意来自 Tendedero（<https://github.com/alejandrobujan/tendedero>），
//! 使用紧凑绳子、原比例缩略图和一次性入场动画。
//! 单击原生预览，双击默认应用打开，拖拽交付文件，右键管理。
//! 只在 iRec 状态图标附近悬停时展开；遵循系统减少动态效果设置。
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
    NSAccessibility, NSAccessibilityButtonRole, NSAccessibilityElement, NSAccessibilityGroupRole,
    NSBackingStoreType, NSBezierPath, NSColor, NSCursor, NSDragOperation, NSDraggingContext,
    NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent, NSFloatingWindowLevel, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext, NSImage, NSMenu,
    NSMenuItem, NSPanel, NSPasteboardWriting, NSScreen, NSShadow, NSStringDrawing, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_av_foundation::{AVAssetImageGenerator, AVURLAsset};
use objc2_core_foundation::CGSize;
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
const CARD_W: f64 = 168.0;
const SPACING: f64 = CARD_W + 30.0;
/// 绳子窗口高度：照片 + 夹子 + 上下留白。
const PANEL_H: f64 = 320.0;
/// 绳子两端离窗口顶部的距离。
const ROPE_TOP: f64 = 10.0;
/// 夹子顶边在绳子上方多少。
/// 缩略图与卡片之间的留白。
const INSET: f64 = 3.0;
const RADIUS: f64 = 12.0;
/// 卡片顶边与夹子之间的距离。
const CARD_TOP: f64 = 8.0;
/// 绳上最多挂几张，多了最旧那张自己掉下去。
const MAX_ITEMS: usize = 8;
/// 收起时整条绳子往上藏多少。
const HIDDEN: f64 = PANEL_H + 12.0;

// MARK: - 动画时长

const FLIGHT_DUR: f64 = 0.36;
const FALL_DUR: f64 = 0.20;
const FALL_DISTANCE: f64 = 16.0;
/// 飞行途中往上拱起的高度。
const ARC: f64 = 12.0;
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
/// 单次轻摆，快速停稳。
const SWING: Spring = Spring::stiffness(55.0, 10.0);
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

/// Curvature is relative to the visible rope, never the full display width.
fn rope_span(width: f64, count: usize) -> (f64, f64) {
    let length = (count.max(1) as f64 * SPACING + 104.0)
        .max(420.0)
        .min((width - 32.0).max(1.0));
    ((width - length) / 2.0, length)
}

fn sag(length: f64) -> f64 {
    (length * 0.085).clamp(28.0, 64.0)
}

fn rope_y(x: f64, width: f64, count: usize) -> f64 {
    let (left, length) = rope_span(width, count);
    let f = ((x - left) / length).clamp(0.0, 1.0);
    ROPE_TOP + 4.0 * sag(length) * f * (1.0 - f)
}

fn hanging_style(id: u64) -> (f64, f64) {
    let i = id.saturating_sub(1) as usize % 4;
    ([24.0, 42.0, 30.0, 46.0][i], [-1.8, 1.4, -1.0, 2.0][i])
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
    /// 录屏（画播放键）还是截图。
    video: bool,
    duration: Option<String>,
    /// Short suspension wires give captures a staggered, hanging rhythm.
    hanger: f64,
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
    /// 操作反馈（「已显示」之类）和它出现的时刻。
    bubble: Option<(String, Instant)>,
}

impl Peg {
    fn top_offset(&self) -> f64 {
        self.hanger + CARD_TOP
    }

    fn card_w(&self) -> f64 {
        CARD_W
    }

    fn card_h(&self) -> f64 {
        PHOTO_MAX_H + INSET * 2.0 + 24.0
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
    enabled: bool,
    hover_enabled: bool,
    busy: bool,
    reduced_motion: bool,
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
    tooltip: Option<(u64, Hit)>,
    anchor: Option<NSRect>,
    pending_click: Option<(u64, Instant)>,
    context_id: Option<u64>,
    focused: Option<u64>,
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
            enabled: true,
            hover_enabled: true,
            busy: false,
            reduced_motion: false,
            pointer: None,
            next_id: 1,
            screen: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
            panel_top: 0.0,
            press: None,
            annotate_icon: None,
            dragging: None,
            hand: false,
            clickable: false,
            tooltip: None,
            anchor: None,
            pending_click: None,
            context_id: None,
            focused: None,
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
    duration: Option<String>,
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
    /// 悬停不抢焦点；主动展开时支持键盘。
    #[unsafe(super = NSPanel)]
    #[thread_kind = MainThreadOnly]
    #[ivars = PanelIvars]
    struct LinePanel;

    impl LinePanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
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
                self.ivars().state.borrow_mut().pending_click = None;
                self.start_drag(index, event);
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let press = self.ivars().state.borrow_mut().press.take();
            let Some((id, on, _)) = press else { return };
            let index = self.ivars().state.borrow().items.iter().position(|i| i.id == id);
            let Some(index) = index else { return };
            match on {
                Hit::Close => { self.take_off(index, false); },
                Hit::Annotate => self.annotate(index),
                Hit::Card => {
                    if event.clickCount() >= 2 {
                        self.ivars().state.borrow_mut().pending_click = None;
                        crate::preview::close();
                        self.open(index);
                    } else {
                        self.ivars().state.borrow_mut().pending_click = Some((id, Instant::now()));
                    }
                }
            }
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool { true }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let mut st = self.ivars().state.borrow_mut();
            let live: Vec<u64> = st.items.iter().filter(|p| p.fall.is_none()).map(|p| p.id).collect();
            if event.keyCode() == 53 { st.pinned = false; st.revealed = false; return; }
            if live.is_empty() { return; }
            let current = live.iter().position(|id| Some(*id) == st.focused).unwrap_or(live.len() - 1);
            match event.keyCode() {
                123 => st.focused = Some(live[current.saturating_sub(1)]),
                124 => st.focused = Some(live[(current + 1).min(live.len() - 1)]),
                36 | 49 | 76 => {
                    let index = st.items.iter().position(|p| p.id == live[current]);
                    drop(st);
                    if let Some(index) = index { self.preview(index); }
                },
                _ => {},
            }
            self.setNeedsDisplay(true);
        }

        #[unsafe(method_id(accessibilityChildren))]
        fn accessibility_children(&self) -> Retained<NSArray<AnyObject>> {
            let st = self.ivars().state.borrow();
            let mut children: Vec<Retained<AnyObject>> = Vec::new();
            for item in st.items.iter().filter(|p| st.slide >= 0.6 && !p.flying && p.fall.is_none()) {
                let child = CaptureAccessibility::alloc(self.mtm()).set_ivars(item.id);
                let child: Retained<CaptureAccessibility> = unsafe { msg_send![super(child), init] };
                let label = format!("{}，{}，按下以预览", if item.video { "录屏" } else { "截图" }, item.path.file_name().unwrap_or_default().to_string_lossy());
                unsafe { child.setAccessibilityRole(Some(NSAccessibilityButtonRole)); }
                child.setAccessibilityLabel(Some(&NSString::from_str(&label)));
                let rect = flip(st.height, item.x - item.card_w() / 2.0, rope_y(item.x, st.width, st.items.len()) + item.top_offset(), item.card_w(), item.card_h());
                if let Some(panel) = panel_target() { child.setAccessibilityFrame(panel.convertRectToScreen(rect)); }
                unsafe { child.setAccessibilityParent(Some(self)); }
                children.push(Retained::into_super(Retained::into_super(child)).into());
            }
            NSArray::from_retained_slice(&children)
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let Some((index, _)) = self.hit(point) else { return };
            let id = self.ivars().state.borrow().items[index].id;
            self.ivars().state.borrow_mut().context_id = Some(id);
            let menu = NSMenu::new(self.mtm());
            for (label, tag) in [("预览", 0), ("用默认应用打开", 1), ("在访达中显示", 2), ("从晾衣绳移除", 3), ("移到废纸篓", 4)] {
                if tag == 3 { menu.addItem(&NSMenuItem::separatorItem(self.mtm())); }
                let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(self.mtm()), &NSString::from_str(label), Some(sel!(cardAction:)), &NSString::new()) };
                item.setTag(tag);
                unsafe { item.setTarget(Some(self)); }
                menu.addItem(&item);
            }
            NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
            self.ivars().state.borrow_mut().context_id = None;
        }

        #[unsafe(method(cardAction:))]
        fn card_action(&self, sender: &NSMenuItem) {
            let index = { let st = self.ivars().state.borrow(); st.items.iter().position(|p| Some(p.id) == st.context_id) };
            if let Some(index) = index {
                match sender.tag() { 0 => self.preview(index), 1 => self.open(index), 2 => self.reveal_in_finder(index), 3 => { self.take_off(index, false); }, 4 => { self.take_off(index, true); }, _ => {} }
            }
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &AnyObject) {
            self.step();
        }
    }
);

define_class!(
    #[unsafe(super = NSAccessibilityElement)]
    #[thread_kind = MainThreadOnly]
    #[ivars = u64]
    struct CaptureAccessibility;

    impl CaptureAccessibility {
        #[unsafe(method(accessibilityPerformPress))]
        fn press(&self) -> bool {
            if let Some(view) = line_view() {
                let index = view.ivars().state.borrow().items.iter().position(|p| p.id == *self.ivars());
                if let Some(index) = index { view.preview(index); true } else { false }
            } else { false }
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
            let ay = rope_y(item.x, st.width, st.items.len()) + item.drop;
            // 卡片是绕夹子顶边转的，先把指针转回去再判矩形。
            let (dx, dy2) = (px - item.x, py - ay);
            let a = (if st.reduced_motion {
                0.0
            } else {
                item.swing + item.tilt
            })
            .to_radians();
            let (cos, sin) = (a.cos(), a.sin());
            let (lx, ly) = (dx * cos + dy2 * sin + item.x, -dx * sin + dy2 * cos + ay);
            let left = item.x - item.card_w() / 2.0;
            let top = ay + item.top_offset();
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

        let click = {
            let mut st = self.ivars().state.borrow_mut();
            if st.pending_click.is_some_and(|(_, at)| {
                now.duration_since(at).as_secs_f64() >= NSEvent::doubleClickInterval()
            }) {
                st.pending_click.take().and_then(|(id, _)| {
                    st.items.iter().position(|p| p.id == id && p.fall.is_none())
                })
            } else {
                None
            }
        };
        if let Some(index) = click {
            self.preview(index);
        }
        self.drain_pending();
        self.update_reveal(now);
        let over_card = self.update_hover();

        let mut st = self.ivars().state.borrow_mut();
        let width = st.width;
        let previous_slide = st.slide;
        let had_bubbles = st.items.iter().any(|p| p.bubble.is_some());
        let count = st.items.len();
        let reduced = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
        st.reduced_motion = reduced;
        let pointer_x = st.pointer.map(|p| p.x);
        let mut gone = Vec::new();
        for (index, item) in st.items.iter_mut().enumerate() {
            let target = layout_x(index, count, width);
            let (dx, dx_v) = LAYOUT.step(item.x - target, item.x_v, dt);
            item.x = target + dx;
            item.x_v = dx_v;
            let target_swing = if item.hover && !reduced {
                pointer_x
                    .map(|x| ((x - item.x) / CARD_W * 5.0).clamp(-2.5, 2.5))
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            let (swing, swing_v) = SWING.step(item.swing - target_swing, item.swing_v, dt);
            item.swing = target_swing + swing;
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
            if reduced {
                item.x = target;
                item.x_v = 0.0;
                item.swing = 0.0;
                item.swing_v = 0.0;
                item.drop = 0.0;
                item.arrived = true;
            }
            if let Some(t) = item.fall {
                let t = t + dt;
                item.fall = Some(t);
                if t >= FALL_DUR || reduced {
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
        if reduced {
            st.slide = target;
            st.slide_v = 0.0;
        }

        // 一次性入场；减少动态效果时直接到达。
        let mut landed = false;
        if st.flight.active {
            st.flight.t += dt;
            if st.flight.t >= FLIGHT_DUR || reduced {
                st.flight.active = false;
                landed = true;
            }
        }
        drop(st);
        if landed {
            if let Some(item) = self.ivars().state.borrow_mut().items.last_mut() {
                item.flying = false;
                item.arrived = true;
                if !reduced {
                    nudge(&mut item.swing_v, 2.5);
                }
            }
        }

        let st = self.ivars().state.borrow();
        let visible = st.slide > 0.001 || st.revealed;
        let flying = st.flight.active;
        let animating = (previous_slide - st.slide).abs() > 0.0001
            || st.slide_v.abs() > 0.001
            || !gone.is_empty()
            || had_bubbles
            || st.items.iter().any(|p| {
                p.fall.is_some()
                    || p.x_v.abs() > 0.01
                    || p.swing_v.abs() > 0.01
                    || p.drop_v.abs() > 0.01
                    || (p.hover_mix - if p.hover { 1.0 } else { 0.0 }).abs() > 0.001
            });
        drop(st);
        if (visible && animating) || flying {
            self.setNeedsDisplay(true);
        }
        // 绳子横贯屏幕：只有指针压在卡片上时窗口才收点击，其余地方一律穿过去。
        let want_clicks = visible && over_card;
        let mut st = self.ivars().state.borrow_mut();
        let clicks_changed = want_clicks != st.clickable;
        st.clickable = want_clicks;
        drop(st);
        if clicks_changed {
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
        self.place();
        let full_w = CGImage::width(Some(&pending.image)) as f64;
        let full_h = CGImage::height(Some(&pending.image)) as f64;
        let image = NSImage::initWithCGImage_size(
            NSImage::alloc(),
            &pending.image,
            NSSize::new(full_w.max(1.0), full_h.max(1.0)),
        );
        let mut st = self.ivars().state.borrow_mut();
        let id = st.next_id;
        st.next_id += 1;
        let (hanger, tilt) = hanging_style(id);
        let start_x = layout_x(st.items.len(), st.items.len() + 1, st.width);
        // Finish any previous arrival before replacing the shared flight.
        for item in &mut st.items {
            item.flying = false;
            item.arrived = true;
        }
        // 新的一律挂在最右边。
        st.items.push(Peg {
            id,
            path: pending.path,
            image: image.clone(),
            video: pending.video,
            duration: pending.duration,
            hanger,
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
            bubble: None,
        });
        if st.items.len() > MAX_ITEMS {
            if let Some(oldest) = st.items.iter_mut().find(|i| i.fall.is_none()) {
                oldest.fall = Some(0.0);
            }
        }
        let card = NSSize::new(CARD_W, PHOTO_MAX_H + INSET * 2.0 + 24.0);
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
            st.width = frame.size.width;
            st.height = frame.size.height;
            st.panel_top = max_y(&screen.frame()) - max_y(&visible);
        }
        if let Some(panel) = panel_target() {
            if !rect_eq(panel.frame(), frame) {
                panel.setFrame_display(frame, true);
            }
        }
        if let Some(panel) = flight_targets().map(|(p, _)| p) {
            let f = screen.frame();
            if !rect_eq(panel.frame(), f) {
                panel.setFrame_display(f, true);
            }
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
        let in_menu = point_in(mouse, band);
        let in_band = st.anchor.is_some_and(|a| {
            point_in(
                mouse,
                NSRect::new(
                    NSPoint::new(a.origin.x - 10.0, a.origin.y),
                    NSSize::new(a.size.width + 20.0, a.size.height),
                ),
            )
        });
        if !in_band {
            st.suppressed = false;
        }
        if NSEvent::pressedMouseButtons() != 0 && in_menu {
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
        if !st.revealed && !st.items.is_empty() && st.hover_enabled && in_band && !st.suppressed {
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
        if st.revealed
            && !st.pinned
            && !peeking
            && st.dragging.is_none()
            && st.pending_click.is_none()
            && st.context_id.is_none()
            && !crate::preview::is_visible()
        {
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
        let tip = hit.map(|(_, part)| match part {
            Hit::Close => "从晾衣绳移除（保留文件）",
            Hit::Annotate => "标注截图",
            Hit::Card => "单击预览 · 双击打开 · 拖拽使用 · 右键更多操作",
        });
        let tooltip = hit.map(|(index, part)| (st.items[index].id, part));
        if tooltip != st.tooltip {
            st.tooltip = tooltip;
            self.setToolTip(tip.map(NSString::from_str).as_deref());
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
            let ay = rope_y(item.x, st.width, st.items.len()) + item.drop;
            let card = flip(
                st.height,
                item.x - item.card_w() / 2.0,
                ay + item.top_offset(),
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
            let index = self
                .ivars()
                .state
                .borrow()
                .items
                .iter()
                .position(|i| i.id == id);
            if let Some(index) = index {
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
        let mut st = self.ivars().state.borrow_mut();
        if let Some(item) = st.items.iter_mut().find(|i| i.path == path) {
            item.image = image;
            item.video = !is_image_path(&item.path);
            item.bubble = Some(("已保存".to_string(), Instant::now()));
        }
    }

    /// 卡片下面显示操作反馈。
    fn note(&self, index: usize, text: &str) {
        let mut st = self.ivars().state.borrow_mut();
        if let Some(item) = st.items.get_mut(index) {
            item.bubble = Some((text.to_string(), Instant::now()));
        }
    }

    /// Removing a card and trashing its file are separate, explicit actions.
    fn take_off(&self, index: usize, trash: bool) -> bool {
        let Some(path) = self.path_at(index) else {
            return false;
        };
        if trash {
            let url = NSURL::fileURLWithPath_isDirectory(
                &NSString::from_str(&path.to_string_lossy()),
                false,
            );
            if objc2_foundation::NSFileManager::defaultManager()
                .trashItemAtURL_resultingItemURL_error(&url, None)
                .is_err()
            {
                self.note(index, "无法移到废纸篓，文件仍保留");
                return false;
            }
        }
        if let Some(item) = self.ivars().state.borrow_mut().items.get_mut(index) {
            item.fall = Some(0.0);
            item.hover = false;
        }
        true
    }

    fn preview(&self, index: usize) {
        let Some(path) = self.path_at(index) else {
            return;
        };
        if !path.exists() {
            self.note(index, "文件已不在原处");
            return;
        }
        let (video, anchor) = {
            let st = self.ivars().state.borrow();
            let item = &st.items[index];
            let rect = flip(
                st.height,
                item.x - item.card_w() / 2.0,
                rope_y(item.x, st.width, st.items.len()) + item.top_offset(),
                item.card_w(),
                item.card_h(),
            );
            (
                item.video,
                panel_target()
                    .map(|p| p.convertRectToScreen(rect))
                    .unwrap_or(rect),
            )
        };
        crate::preview::show(path, video, anchor);
    }

    fn open(&self, index: usize) {
        let path = self.path_at(index);
        let Some(path) = path else { return };
        if !path.exists() {
            self.note(index, "文件已不在原处");
            return;
        }
        let url =
            NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), false);
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
            crate::preview::close();
            st.pending_click = None;
            st.revealed = false;
            st.pinned = false;
            st.peek_until = None;
        }
    }

    fn set_busy(&self, busy: bool) {
        let mut st = self.ivars().state.borrow_mut();
        st.busy = busy;
        if busy {
            crate::preview::close();
            st.pending_click = None;
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
            st.focused = st.items.last().map(|p| p.id);
            drop(st);
            if let Some(panel) = panel_target() {
                panel.makeKeyAndOrderFront(None);
                panel.makeFirstResponder(Some(self));
            }
        }
    }

    fn clear(&self) {
        crate::preview::close();
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
            NSPoint::new(r.x - screen.origin.x, sh - (r.y - screen.origin.y) - r.h),
            NSSize::new(r.w.max(2.0), r.h.max(2.0)),
        ),
        None => NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(screen.size.width.max(2.0), sh),
        ),
    };
    let index = st.items.len().saturating_sub(1);
    let x = layout_x(index, st.items.len().max(1), st.width);
    let offset = st.items.last().map(Peg::top_offset).unwrap_or(32.0);
    let top = rope_y(x, st.width, st.items.len()) + offset;
    let to_top = NSRect::new(NSPoint::new(x - card.width / 2.0, st.panel_top + top), card);
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
    draw_rope(w, h, st.items.len());
    if st.items.is_empty() {
        draw_hint(w, h);
    }
    for item in st.items.iter() {
        if item.flying {
            continue;
        }
        let fall = item.fall.map(|t| (t / FALL_DUR).clamp(0.0, 1.0).powi(3));
        let ay =
            rope_y(item.x, w, st.items.len()) + item.drop + FALL_DISTANCE * fall.unwrap_or(0.0);
        let alpha =
            ((item.x.min(w - item.x) / 90.0).min(1.0)).max(0.0) * (1.0 - fall.unwrap_or(0.0));
        if alpha <= 0.01 {
            continue;
        }
        let card = flip(
            h,
            item.x - item.card_w() / 2.0,
            ay + item.top_offset(),
            item.card_w(),
            item.card_h(),
        );
        let theta = if st.reduced_motion {
            0.0
        } else {
            item.swing + item.tilt
        };
        CGContext::save_g_state(Some(&ctx));
        CGContext::set_alpha(Some(&ctx), alpha);
        CGContext::translate_ctm(Some(&ctx), item.x, h - ay);
        CGContext::rotate_ctm(Some(&ctx), -theta.to_radians());
        CGContext::translate_ctm(Some(&ctx), -item.x, -(h - ay));
        draw_suspension(card, NSPoint::new(item.x, h - ay), 1.0);
        draw_card(
            Some(&ctx),
            card,
            Some(&item.image),
            RADIUS,
            INSET,
            1.0,
            item.hover_mix,
        );
        if st.focused == Some(item.id) && panel_target().is_some_and(|p| p.isKeyWindow()) {
            NSColor::keyboardFocusIndicatorColor().setStroke();
            let ring = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                inset_rect(card, -2.0),
                RADIUS + 2.0,
                RADIUS + 2.0,
            );
            ring.setLineWidth(2.0);
            ring.stroke();
        }
        draw_clip(Some(&ctx), card, 1.0);
        let long = item.image.size().height > item.image.size().width * 2.0;
        let label = if item.video {
            "录屏"
        } else if long {
            "长截图"
        } else {
            "截图"
        };
        let label = NSString::from_str(label);
        let attrs = text_attrs(11.0, false);
        unsafe {
            label.drawAtPoint_withAttributes(
                NSPoint::new(card.origin.x + 8.0, card.origin.y + 5.0),
                Some(&attrs),
            );
        }
        if let Some(duration) = &item.duration {
            let text = NSString::from_str(duration);
            let size = unsafe { text.sizeWithAttributes(Some(&attrs)) };
            unsafe {
                text.drawAtPoint_withAttributes(
                    NSPoint::new(card.max().x - size.width - 8.0, card.origin.y + 5.0),
                    Some(&attrs),
                );
            }
        }
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

fn draw_rope(w: f64, h: f64, count: usize) {
    if w <= 0.0 {
        return;
    }
    let (left, length) = rope_span(w, count);
    for i in 0..64 {
        let x = left + length * i as f64 / 64.0;
        let next = left + length * (i + 1) as f64 / 64.0;
        let alpha = end_fade((i as f64 + 0.5) / 64.0) * 0.45;
        NSColor::secondaryLabelColor()
            .colorWithAlphaComponent(alpha)
            .setStroke();
        let path = NSBezierPath::bezierPath();
        path.setLineWidth(1.1);
        path.moveToPoint(NSPoint::new(x, h - rope_y(x, w, count)));
        path.lineToPoint(NSPoint::new(next, h - rope_y(next, w, count)));
        path.stroke();
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
        0.12 + 0.04 * lift,
    )));
    shadow.setShadowOffset(NSSize::new(0.0, -5.0 - 3.0 * lift));
    shadow.setShadowBlurRadius(8.0 + 2.0 * lift);
    shadow.set();
    NSColor::controlBackgroundColor().setFill();
    path.fill();
    CGContext::restore_g_state(ctx);

    if let Some(image) = image {
        let mut inner = inset_rect(card, inset);
        inner.origin.y += 24.0 * chrome;
        inner.size.height = (inner.size.height - 24.0 * chrome).max(1.0);
        let r = (radius - inset).max(0.0);
        let inner_path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(inner, r, r);
        CGContext::save_g_state(ctx);
        inner_path.addClip();
        let size = image.size();
        let scale =
            (inner.size.width / size.width.max(1.0)).min(inner.size.height / size.height.max(1.0));
        let fitted = if chrome >= 0.999 {
            fit(size)
        } else {
            NSSize::new(size.width * scale, size.height * scale)
        };
        image.drawInRect(NSRect::new(
            NSPoint::new(
                inner.origin.x + (inner.size.width - fitted.width) / 2.0,
                inner.origin.y + (inner.size.height - fitted.height) / 2.0,
            ),
            fitted,
        ));
        CGContext::restore_g_state(ctx);
        inner_path.setLineWidth(0.5);
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.18 * chrome).setStroke();
        inner_path.stroke();
    }

    path.setLineWidth(0.75);
    NSColor::labelColor()
        .colorWithAlphaComponent(0.12 * chrome)
        .setStroke();
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

/// A fine suspension wire and small eyelet, matching the hanging-card metaphor.
fn draw_suspension(card: NSRect, pivot: NSPoint, chrome: f64) {
    let wire = NSBezierPath::bezierPath();
    wire.setLineWidth(0.8);
    wire.moveToPoint(pivot);
    wire.lineToPoint(NSPoint::new(pivot.x, card.max().y + 5.0));
    NSColor::secondaryLabelColor()
        .colorWithAlphaComponent(0.36 * chrome)
        .setStroke();
    wire.stroke();
    let anchor = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(pivot.x - 2.5, pivot.y - 2.5),
        NSSize::new(5.0, 5.0),
    ));
    NSColor::tertiaryLabelColor()
        .colorWithAlphaComponent(chrome * 0.35)
        .setFill();
    anchor.fill();
}

fn draw_clip(_ctx: Option<&CGContext>, card: NSRect, chrome: f64) {
    let cx = card.origin.x + card.size.width / 2.0;
    let ring = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(cx - 3.5, card.max().y + 1.0),
        NSSize::new(7.0, 7.0),
    ));
    ring.setLineWidth(1.4);
    NSColor::secondaryLabelColor()
        .colorWithAlphaComponent(chrome * 0.5)
        .setStroke();
    ring.stroke();
    let clasp = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
        NSRect::new(
            NSPoint::new(cx - 6.0, card.max().y - 2.5),
            NSSize::new(12.0, 5.0),
        ),
        2.0,
        2.0,
    );
    NSColor::secondaryLabelColor()
        .colorWithAlphaComponent(chrome * 0.5)
        .setFill();
    clasp.fill();
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
            NSColor::labelColor()
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
    let text = NSString::from_str("截图与录屏会出现在这里");
    let attrs = text_attrs(12.0, false);
    let size = unsafe { text.sizeWithAttributes(Some(&attrs)) };
    let bw = size.width + 24.0;
    let bh = size.height + 12.0;
    let r = NSRect::new(
        NSPoint::new(w / 2.0 - bw / 2.0, h - rope_y(w / 2.0, w, 0) - 34.0 - bh),
        NSSize::new(bw, bh),
    );
    let capsule = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, bh / 2.0, bh / 2.0);
    NSColor::controlBackgroundColor().setFill();
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
    view.setAccessibilityElement(true);
    unsafe {
        view.setAccessibilityRole(Some(NSAccessibilityGroupRole));
    }
    view.setAccessibilityLabel(Some(&NSString::from_str("最近捕获，晾衣绳")));
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
    let video = !is_image_path(&path);
    std::thread::spawn(move || match first_frame(&path) {
        Ok(image) => {
            PENDING
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Pending {
                    image,
                    duration: video.then(|| video_duration(&path)).flatten(),
                    path,
                    region,
                    video,
                });
        }
        Err(err) => crate::state::log(&format!("line: 缩略图生成失败：{err}")),
    });
}

fn is_image_path(path: &std::path::Path) -> bool {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "heic" | "tif" | "tiff" | "gif" | "webp"
    )
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
            duration: None,
        });
}

#[allow(deprecated)]
fn first_frame(path: &std::path::Path) -> Result<Retained<CGImage>, String> {
    if is_image_path(path) {
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

/// The status item supplies the only hover trigger, in global AppKit coordinates.
pub fn set_anchor(anchor: NSRect) {
    if let Some(view) = line_view() {
        view.ivars().state.borrow_mut().anchor = Some(anchor);
    }
}

pub fn remove_path(path: &std::path::Path, trash: bool) -> bool {
    let Some(view) = line_view() else {
        return false;
    };
    let index = view
        .ivars()
        .state
        .borrow()
        .items
        .iter()
        .position(|i| i.path == path);
    let Some(index) = index else { return false };
    view.take_off(index, trash)
}

#[allow(deprecated)]
fn video_duration(path: &std::path::Path) -> Option<String> {
    let url =
        NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), false);
    let asset = unsafe { AVURLAsset::assetWithURL(&url) };
    let time: CMTime = unsafe { msg_send![&*asset, duration] };
    if time.timescale <= 0 || time.value < 0 {
        return None;
    }
    let seconds = time.value as u64 / time.timescale as u64;
    Some(format!("{:02}:{:02}", seconds / 60, seconds % 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_rope_keeps_its_sag_on_wide_screens() {
        for screen in [800.0, 1440.0, 3456.0] {
            for count in [1, 3, 8] {
                let (left, length) = rope_span(screen, count);
                assert!((rope_y(left, screen, count) - ROPE_TOP).abs() < 0.001);
                assert!((rope_y(left + length, screen, count) - ROPE_TOP).abs() < 0.001);
                let center = rope_y(screen / 2.0, screen, count);
                assert!(center - ROPE_TOP >= 28.0);
                for index in 0..count {
                    let (hanger, _) = hanging_style(index as u64 + 1);
                    let bottom = rope_y(layout_x(index, count, screen), screen, count)
                        + hanger
                        + CARD_TOP
                        + PHOTO_MAX_H
                        + 30.0;
                    assert!(
                        bottom + 30.0 < PANEL_H,
                        "card feedback must fit within the shelf"
                    );
                }
            }
        }
    }

    #[test]
    fn portrait_and_panorama_thumbnails_preserve_the_entire_aspect_ratio() {
        for original in [
            NSSize::new(800.0, 20_000.0),
            NSSize::new(10_000.0, 100.0),
            NSSize::new(1200.0, 800.0),
        ] {
            let fitted = fit(original);
            assert!(fitted.width <= CARD_W && fitted.height <= PHOTO_MAX_H);
            assert!(
                (fitted.width / fitted.height - original.width / original.height).abs() < 0.000001
            );
        }
    }

    #[test]
    fn flight_lands_on_card_center_and_starts_at_capture_coordinates() {
        let mut state = LineState::default();
        state.screen = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1440.0, 900.0));
        state.width = 1440.0;
        state.panel_top = 25.0;
        let card = NSSize::new(CARD_W, 134.0);
        let region = Rect {
            x: 100.0,
            y: 200.0,
            w: 640.0,
            h: 360.0,
        };
        let (from, to) = flight_rects(&state, Some(region), card);
        assert_eq!(from.origin, NSPoint::new(100.0, 200.0));
        assert_eq!(from.size, NSSize::new(640.0, 360.0));
        assert_eq!(to.origin.x + card.width / 2.0, 720.0);
    }
}
