//! Drag-only setup for Screen & System Audio Recording and Accessibility.
//! The real running .app bundle is dragged into System Settings. Only a system
//! permission check may advance the guide; accepting a drop never grants access.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityGroupRole, NSApplication, NSBackingStoreType, NSBezelStyle,
    NSBezierPath, NSButton, NSColor, NSDragOperation, NSDraggingContext, NSDraggingItem,
    NSDraggingSession, NSDraggingSource, NSEvent, NSFloatingWindowLevel, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSImage, NSPanel, NSPasteboardWriting,
    NSRunningApplication, NSScreen, NSStringDrawing, NSTextField, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSBundle, NSDictionary, NSMutableDictionary, NSNumber, NSObject,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer, NSURL, NSUserDefaults,
};

use crate::state::{Action, persist, with_state};

const W: f64 = 560.0;
const H: f64 = 400.0;
const GUIDE_W: f64 = 340.0;
const GUIDE_H: f64 = 278.0;
const SETTINGS_ID: &str = "com.apple.systempreferences";
const RESUME_KEY: &str = "iRec.permissionGuide.resumePane";

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Screen,
    Accessibility,
}

impl Pane {
    fn title(self) -> &'static str {
        match self {
            Self::Screen => "录屏与系统录音",
            Self::Accessibility => "辅助功能",
        }
    }

    fn url(self) -> &'static str {
        match self {
            Self::Screen => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            Self::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
        }
    }

    fn resume_value(self) -> isize {
        match self {
            Self::Screen => 1,
            Self::Accessibility => 2,
        }
    }

    fn from_resume(value: isize) -> Option<Self> {
        match value {
            1 => Some(Self::Screen),
            2 => Some(Self::Accessibility),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Permissions {
    screen: bool,
    access: bool,
}

impl Permissions {
    fn current() -> Self {
        Self {
            screen: screen_recording(),
            access: accessibility(),
        }
    }
    fn granted(self, pane: Pane) -> bool {
        match pane {
            Pane::Screen => self.screen,
            Pane::Accessibility => self.access,
        }
    }
    fn next(self) -> Option<Pane> {
        if !self.screen {
            Some(Pane::Screen)
        } else if !self.access {
            Some(Pane::Accessibility)
        } else {
            None
        }
    }

    fn missing_for_capture(self, _action: &Action) -> Option<Pane> {
        if !self.screen {
            Some(Pane::Screen)
        } else {
            None
        }
    }
}

fn starts_capture(action: &Action, recording: bool) -> bool {
    match action {
        Action::ToggleFull | Action::PickRegion | Action::StartRegion(_) => !recording,
        Action::ShotFull
        | Action::PickShot
        | Action::ShotRegion(_)
        | Action::ShotRegionAnnotated(_)
        | Action::PickScrollShot
        | Action::ScrollShotRegion(..) => true,
        _ => false,
    }
}

/// Route missing permissions into the drag guide before a capture API could
/// display the former system request prompt. Stopping/cancelling always works.
pub fn prepare_capture(mtm: MainThreadMarker, action: &Action, recording: bool) -> bool {
    if !starts_capture(action, recording) {
        return true;
    }
    if let Some(pane) = Permissions::current().missing_for_capture(action) {
        crate::hud::hide();
        show(mtm);
        show_pane(pane);
        false
    } else {
        true
    }
}

#[derive(Clone, Copy)]
struct GuideState {
    pane: Pane,
    initially_granted: bool,
    dragging: bool,
    dropped: bool,
    opened_at: Instant,
}

impl GuideState {
    fn newly_granted(self, permissions: Permissions) -> bool {
        !self.dragging && !self.initially_granted && permissions.granted(self.pane)
    }
}

// Keep System Settings active while dragging, including when it is in another
// Space. The helper can receive mouse/key focus without activating the app.
define_class!(
    #[unsafe(super = NSPanel)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ()]
    struct PermissionPanel;
    impl PermissionPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool { true }
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool { false }
    }
);

struct Ui {
    window: Retained<NSWindow>,
    screen_status: Retained<NSTextField>,
    access_status: Retained<NSTextField>,
    primary: Retained<NSButton>,
    note: Retained<NSTextField>,
    helper: Retained<PermissionPanel>,
    helper_title: Retained<NSTextField>,
    helper_step: Retained<NSTextField>,
    helper_note: Retained<NSTextField>,
    drag_view: Retained<AppDragView>,
    guide: Option<GuideState>,
    initially_screen_granted: bool,
    _controller: Retained<Controller>,
    _timer: Retained<NSTimer>,
}

// SAFETY: all AppKit access is confined to the main thread. Never call AppKit
// while holding UI: window notifications and dragging can re-enter this module.
unsafe impl Send for Ui {}
static UI: Mutex<Option<Ui>> = Mutex::new(None);

pub fn screen_recording() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}
pub fn accessibility() -> bool {
    unsafe { AXIsProcessTrusted() }
}

pub fn maybe_show_onboarding(mtm: MainThreadMarker) {
    let defaults = NSUserDefaults::standardUserDefaults();
    let key = NSString::from_str(RESUME_KEY);
    let resume = Pane::from_resume(defaults.integerForKey(&key));
    defaults.removeObjectForKey(&key);
    if let Some(pane) = resume {
        show(mtm);
        let permissions = Permissions::current();
        if let Some(next) = permissions.next() {
            show_pane(if permissions.granted(pane) {
                next
            } else {
                pane
            });
        }
        return;
    }
    if Permissions::current().next().is_none() {
        return;
    }
    if with_state(|state| state.settings.permissions_prompted) {
        return;
    }
    let settings = with_state(|state| {
        state.settings.permissions_prompted = true;
        state.settings.clone()
    });
    persist(&settings);
    show(mtm);
}

pub fn show(mtm: MainThreadMarker) {
    let existing = UI
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|ui| (ui.window.clone(), ui.helper.clone()));
    if let Some((window, helper)) = existing {
        UI.lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .unwrap()
            .guide = None;
        helper.orderOut(None);
        refresh();
        window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        return;
    }

    let permissions = Permissions::current();
    let controller = Controller::new(mtm);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0.0, 0.0, W, H),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("iRec 权限"));
    let content = window.contentView().expect("permissions content");
    add_label(
        &content,
        mtm,
        "让 iRec 准备就绪",
        rect(28.0, 340.0, 500.0, 30.0),
        22.0,
        true,
    );
    add_label(
        &content,
        mtm,
        "把 iRec 图标拖入对应的权限列表，即可开始设置。",
        rect(28.0, 310.0, 500.0, 22.0),
        12.0,
        false,
    );
    let screen_status = permission_row(
        &content,
        mtm,
        &controller,
        Pane::Screen,
        210.0,
        "用于截图、录屏和录制系统声音",
        sel!(openScreen:),
    );
    let access_status = permission_row(
        &content,
        mtm,
        &controller,
        Pane::Accessibility,
        112.0,
        "用于自动滚动长截图",
        sel!(openAccessibility:),
    );
    let note = add_label(
        &content,
        mtm,
        "",
        rect(28.0, 69.0, 500.0, 28.0),
        11.0,
        false,
    );
    let close = make_button(
        mtm,
        "稍后",
        rect(294.0, 24.0, 72.0, 32.0),
        sel!(closeGuide:),
        &controller,
        false,
    );
    content.addSubview(&close);
    let primary = make_button(
        mtm,
        "开始拖拽设置",
        rect(376.0, 24.0, 156.0, 32.0),
        sel!(beginGuide:),
        &controller,
        true,
    );
    content.addSubview(&primary);

    let helper: Retained<PermissionPanel> = unsafe {
        msg_send![
            super(PermissionPanel::alloc(mtm).set_ivars(())),
            initWithContentRect: rect(0.0, 0.0, GUIDE_W, GUIDE_H),
            styleMask: NSWindowStyleMask::Titled | NSWindowStyleMask::NonactivatingPanel,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ]
    };
    unsafe { helper.setReleasedWhenClosed(false) };
    helper.setTitle(&NSString::from_str("iRec · 拖拽授权"));
    helper.setLevel(NSFloatingWindowLevel);
    helper.setHidesOnDeactivate(false);
    helper.setBecomesKeyOnlyIfNeeded(false);
    helper.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    let body = helper.contentView().expect("drag guide content");
    let helper_title = add_label(&body, mtm, "", rect(20.0, 235.0, 300.0, 24.0), 17.0, true);
    let helper_step = add_label(&body, mtm, "", rect(20.0, 205.0, 300.0, 24.0), 11.0, false);
    let drag_view = AppDragView::new(mtm);
    drag_view.setFrame(rect(20.0, 104.0, 300.0, 92.0));
    body.addSubview(&drag_view);
    let helper_note = add_label(&body, mtm, "", rect(20.0, 61.0, 300.0, 34.0), 11.0, false);
    for (title, x, width, action) in [
        ("返回", 16.0, 56.0, sel!(back:)),
        ("显示设置", 80.0, 92.0, sel!(revealSettings:)),
        ("重启后检查", 182.0, 140.0, sel!(restartGuide:)),
    ] {
        body.addSubview(&make_button(
            mtm,
            title,
            rect(x, 18.0, width, 30.0),
            action,
            &controller,
            false,
        ));
    }
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.6,
            &controller,
            sel!(tick:),
            None,
            true,
        )
    };
    *UI.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ui {
        window: window.clone(),
        screen_status,
        access_status,
        primary,
        note,
        helper,
        helper_title,
        helper_step,
        helper_note,
        drag_view,
        guide: None,
        initially_screen_granted: permissions.screen,
        _controller: controller,
        _timer: timer,
    });
    refresh();
    window.center();
    window.makeKeyAndOrderFront(None);
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

fn permission_row(
    content: &NSView,
    mtm: MainThreadMarker,
    controller: &Controller,
    pane: Pane,
    y: f64,
    description: &str,
    action: objc2::runtime::Sel,
) -> Retained<NSTextField> {
    add_label(
        content,
        mtm,
        pane.title(),
        rect(28.0, y + 54.0, 300.0, 23.0),
        15.0,
        true,
    );
    add_label(
        content,
        mtm,
        description,
        rect(28.0, y + 28.0, 330.0, 20.0),
        12.0,
        false,
    );
    let status = add_label(
        content,
        mtm,
        "",
        rect(28.0, y + 6.0, 280.0, 18.0),
        11.0,
        false,
    );
    content.addSubview(&make_button(
        mtm,
        "拖拽设置…",
        rect(412.0, y + 30.0, 120.0, 32.0),
        action,
        controller,
        false,
    ));
    status
}

fn show_pane(pane: Pane) {
    let granted = Permissions::current().granted(pane);
    let handles = {
        let mut guard = UI.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_mut().map(|ui| {
            ui.guide = Some(GuideState {
                pane,
                initially_granted: granted,
                dragging: false,
                dropped: false,
                opened_at: Instant::now(),
            });
            (ui.window.clone(), ui.helper.clone(), ui.drag_view.clone())
        })
    };
    let Some((window, helper, drag)) = handles else {
        return;
    };
    window.orderOut(None);
    helper.setIgnoresMouseEvents(false);
    helper.setAlphaValue(1.0);
    drag.ivars().dragging.set(false);
    // Opening the pane does not trigger a permission request or add an app.
    open_url(pane.url());
    refresh();
}

fn refresh() {
    let permissions = Permissions::current();
    let snapshot = UI
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|ui| {
            (
                ui.screen_status.clone(),
                ui.access_status.clone(),
                ui.primary.clone(),
                ui.note.clone(),
                ui.helper.clone(),
                ui.helper_title.clone(),
                ui.helper_step.clone(),
                ui.helper_note.clone(),
                ui.guide,
                ui.initially_screen_granted,
                ui.drag_view.ivars().bundle_url.is_some(),
            )
        });
    let Some((
        screen,
        access,
        primary,
        note,
        helper,
        title,
        step,
        hint,
        guide,
        initial_screen,
        bundle_available,
    )) = snapshot
    else {
        return;
    };
    for (label, granted) in [(&screen, permissions.screen), (&access, permissions.access)] {
        label.setStringValue(&NSString::from_str(if granted {
            "已开启"
        } else {
            "待设置"
        }));
        let color = if granted {
            NSColor::systemGreenColor()
        } else {
            NSColor::secondaryLabelColor()
        };
        label.setTextColor(Some(&color));
    }
    primary.setTitle(&NSString::from_str(if permissions.next().is_some() {
        "开始拖拽设置"
    } else if !initial_screen {
        "重启并完成"
    } else {
        "完成"
    }));
    note.setStringValue(&NSString::from_str(if permissions.next().is_none() {
        "权限已就绪。"
    } else {
        "两项分别拖入一次；系统要求的开关与验证仍需你确认。"
    }));
    let Some(guide) = guide else { return };
    if guide.dragging {
        return;
    }
    if guide.newly_granted(permissions) {
        if let Some(next) = permissions.next() {
            show_pane(next);
        } else if let Some(mtm) = MainThreadMarker::new() {
            show(mtm);
        }
        return;
    }
    title.setStringValue(&NSString::from_str(guide.pane.title()));
    step.setStringValue(&NSString::from_str(match guide.pane {
        Pane::Screen => "第 1 项，共 2 项 · 请拖入上方的录屏列表",
        Pane::Accessibility => "第 2 项，共 2 项 · 拖入「辅助功能」应用列表",
    }));
    hint.setStringValue(&NSString::from_str(if !bundle_available {
        "请从 iRec.app 启动应用，再使用拖拽设置。"
    } else if permissions.granted(guide.pane) {
        "此项权限已开启，无需重复添加。"
    } else if guide.dropped {
        "请确认列表中的 iRec 开关；系统检测到授权后会自动继续。"
    } else {
        "将上方 iRec 拖入系统列表，再确认开关。已在列表中可直接确认。"
    }));
    follow_settings(&helper, guide.opened_at);
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn add_label(
    parent: &NSView,
    mtm: MainThreadMarker,
    text: &str,
    frame: NSRect,
    size: f64,
    bold: bool,
) -> Retained<NSTextField> {
    let field = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    field.setFrame(frame);
    let font = if bold {
        NSFont::boldSystemFontOfSize(size)
    } else {
        NSFont::systemFontOfSize(size)
    };
    let color = if bold {
        NSColor::labelColor()
    } else {
        NSColor::secondaryLabelColor()
    };
    field.setFont(Some(&font));
    field.setTextColor(Some(&color));
    parent.addSubview(&field);
    field
}

fn make_button(
    mtm: MainThreadMarker,
    title: &str,
    frame: NSRect,
    action: objc2::runtime::Sel,
    target: &Controller,
    primary: bool,
) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    button.setTitle(&NSString::from_str(title));
    button.setBezelStyle(NSBezelStyle::Push);
    if primary {
        button.setKeyEquivalent(&NSString::from_str("\r"));
    }
    unsafe {
        button.setAction(Some(action));
        button.setTarget(Some(target));
    }
    button
}

fn app_bundle_path(path: &Path) -> Option<PathBuf> {
    (path.extension().is_some_and(|e| e == "app") && path.join("Contents/Info.plist").is_file())
        .then(|| path.to_path_buf())
}

struct DragIvars {
    bundle_url: Option<Retained<NSURL>>,
    icon: Option<Retained<NSImage>>,
    dragging: Cell<bool>,
}

impl AppDragView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let bundle = NSBundle::mainBundle();
        let path = PathBuf::from(bundle.bundlePath().to_string());
        let valid = app_bundle_path(&path).is_some();
        let this = Self::alloc(mtm).set_ivars(DragIvars {
            bundle_url: valid.then(|| bundle.bundleURL()),
            icon: valid.then(|| NSWorkspace::sharedWorkspace().iconForFile(&bundle.bundlePath())),
            dragging: Cell::new(false),
        });
        let view: Retained<Self> = unsafe { msg_send![super(this), init] };
        view.setAccessibilityElement(true);
        unsafe {
            view.setAccessibilityRole(Some(NSAccessibilityGroupRole));
        }
        view.setAccessibilityLabel(Some(&NSString::from_str(
            "iRec 应用图标，拖入系统设置的权限列表",
        )));
        view.setToolTip(Some(&NSString::from_str(
            "按住并拖动 iRec，到系统设置中的应用列表松开",
        )));
        view
    }
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = DragIvars]
    struct AppDragView;

    unsafe impl NSObjectProtocol for AppDragView {}
    unsafe impl NSDraggingSource for AppDragView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn mask(&self, _session: &NSDraggingSession, context: NSDraggingContext) -> NSDragOperation {
            if context == NSDraggingContext::OutsideApplication { NSDragOperation::Copy | NSDragOperation::Generic }
            else { NSDragOperation::empty() }
        }

        #[unsafe(method(draggingSession:willBeginAtPoint:))]
        fn will_begin(&self, _session: &NSDraggingSession, _point: NSPoint) {
            let helper = UI.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                .map(|ui| ui.helper.clone());
            // Wait until AppKit has accepted the initial mouse event before
            // making the source panel transparent to the drop destination.
            if let Some(helper) = helper {
                helper.setIgnoresMouseEvents(true);
                helper.setAlphaValue(0.30);
            }
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, operation: NSDragOperation) {
            self.ivars().dragging.set(false);
            let helper = {
                let mut guard = UI.lock().unwrap_or_else(|e| e.into_inner());
                guard.as_mut().map(|ui| {
                    if let Some(guide) = ui.guide.as_mut() {
                        guide.dragging = false;
                        // A receiver accepting the bundle is NOT proof of authorization.
                        guide.dropped = !operation.is_empty();
                    }
                    ui.helper.clone()
                })
            };
            if let Some(helper) = helper {
                helper.setIgnoresMouseEvents(false);
                helper.setAlphaValue(1.0);
            }
            refresh();
        }
    }

    impl AppDragView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool { true }
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool { false }
        // AppKit owns tracking from the initial mouse-down, including fast
        // drags that leave this small panel before the next mouse event.
        #[unsafe(method(mouseDown:))]
        fn down(&self, event: &NSEvent) {
            if self.ivars().dragging.get() { return; }
            let Some(url) = self.ivars().bundle_url.as_ref() else { return };
            let Some(icon) = self.ivars().icon.as_ref() else { return };
            let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::<dyn NSPasteboardWriting>::from_ref(&**url));
            unsafe { item.setDraggingFrame_contents(rect(20.0, 18.0, 56.0, 56.0), Some(&**icon as &AnyObject)); }
            self.ivars().dragging.set(true);
            {
                let mut guard = UI.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ui) = guard.as_mut() {
                    if let Some(guide) = ui.guide.as_mut() { guide.dragging = true; }
                }
            }
            let session = self.beginDraggingSessionWithItems_event_source(
                &NSArray::from_retained_slice(&[item]), event, ProtocolObject::from_ref(self));
            session.setAnimatesToStartingPositionsOnCancelOrFail(true);
        }
        #[unsafe(method(drawRect:))]
        fn draw(&self, _dirty: NSRect) {
            let card = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(1.0,1.0,self.bounds().size.width-2.0,90.0),12.0,12.0);
            NSColor::controlBackgroundColor().setFill(); card.fill();
            NSColor::separatorColor().setStroke(); card.setLineWidth(1.0); card.stroke();
            if let Some(icon) = self.ivars().icon.as_ref() { icon.drawInRect(rect(20.0,18.0,56.0,56.0)); }
            draw_text("iRec", NSPoint::new(94.0,50.0), 18.0, true);
            draw_text("拖到权限列表 →", NSPoint::new(94.0,27.0), 12.0, false);
        }
    }
);

fn draw_text(text: &str, point: NSPoint, size: f64, bold: bool) {
    let attrs: Retained<NSMutableDictionary<NSString, AnyObject>> = NSMutableDictionary::new();
    let font = if bold {
        NSFont::boldSystemFontOfSize(size)
    } else {
        NSFont::systemFontOfSize(size)
    };
    let color = if bold {
        NSColor::labelColor()
    } else {
        NSColor::secondaryLabelColor()
    };
    unsafe {
        attrs.setObject_forKey(
            &*font as &AnyObject,
            ProtocolObject::from_ref(NSFontAttributeName),
        );
        attrs.setObject_forKey(
            &*color as &AnyObject,
            ProtocolObject::from_ref(NSForegroundColorAttributeName),
        );
        NSString::from_str(text).drawAtPoint_withAttributes(point, Some(&attrs));
    }
}

// Window bounds and owner PID are public WindowServer metadata; tracking does
// not require the very Accessibility permission the user is trying to enable.
fn settings_frame(mtm: MainThreadMarker) -> Option<NSRect> {
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(
        SETTINGS_ID,
    ));
    let pid = apps.firstObject()?.processIdentifier();
    let windows = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        0,
    )?;
    // CFArray/CFDictionary and NSArray/NSDictionary are toll-free bridged.
    let windows: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(std::ptr::from_ref(&*windows).cast()) };
    let top = NSScreen::screens(mtm).firstObject()?.frame().max().y;
    windows.iter().find_map(|window| {
        let number = |key: &str| {
            window
                .objectForKey(&NSString::from_str(key))?
                .downcast_ref::<NSNumber>()
                .map(|n| n.intValue())
        };
        if number("kCGWindowOwnerPID")? != pid || number("kCGWindowLayer")? != 0 {
            return None;
        }
        let bounds = window.objectForKey(&NSString::from_str("kCGWindowBounds"))?;
        let bounds = bounds.downcast_ref::<NSDictionary>()?;
        let value = |key: &str| {
            bounds
                .objectForKey(&NSString::from_str(key))
                .and_then(|n| n.downcast_ref::<NSNumber>().map(|n| n.doubleValue()))
        };
        let (x, y, w, h) = (value("X")?, value("Y")?, value("Width")?, value("Height")?);
        (w >= 300.0 && h >= 200.0).then(|| rect(x, top - y - h, w, h))
    })
}

fn helper_frame(settings: NSRect, visible: NSRect, size: NSSize) -> NSRect {
    let gap = 12.0;
    let right = settings.max().x + gap;
    let left = settings.origin.x - gap - size.width;
    let x = if right + size.width <= visible.max().x {
        right
    } else if left >= visible.origin.x {
        left
    } else {
        visible.origin.x + gap
    };
    let x = x.clamp(
        visible.origin.x,
        (visible.max().x - size.width).max(visible.origin.x),
    );
    let y = (settings.max().y - size.height - 28.0).clamp(
        visible.origin.y,
        (visible.max().y - size.height).max(visible.origin.y),
    );
    rect(x, y, size.width, size.height)
}

fn follow_settings(helper: &NSPanel, opened_at: Instant) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let front = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|a| a.bundleIdentifier())
        .map(|s| s.to_string());
    let own = NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|s| s.to_string());
    if front.as_deref() != Some(SETTINGS_ID) && front != own {
        helper.orderOut(None);
        return;
    }
    // During a Space transition, or when WindowServer omits another app's
    // bounds, keep the drag source usable without requesting more permission.
    let Some(settings) = settings_frame(mtm) else {
        if !helper.isVisible() {
            // Allow Settings to finish switching Spaces before using a centered fallback.
            if opened_at.elapsed() < Duration::from_millis(1200) {
                return;
            }
            helper.center();
            helper.makeKeyAndOrderFront(None);
        }
        return;
    };
    let screens = NSScreen::screens(mtm);
    let center = NSPoint::new(
        settings.origin.x + settings.size.width / 2.0,
        settings.origin.y + settings.size.height / 2.0,
    );
    let screen = screens
        .iter()
        .find(|s| {
            let f = s.frame();
            center.x >= f.origin.x
                && center.x <= f.max().x
                && center.y >= f.origin.y
                && center.y <= f.max().y
        })
        .or_else(|| screens.firstObject());
    let Some(screen) = screen else { return };
    // setFrame takes the entire window, including its title bar.
    let size = helper
        .frameRectForContentRect(rect(0.0, 0.0, GUIDE_W, GUIDE_H))
        .size;
    let frame = helper_frame(settings, screen.visibleFrame(), size);
    if helper.frame() != frame {
        helper.setFrame_display(frame, true);
    }
    if !helper.isVisible() {
        helper.makeKeyAndOrderFront(None);
    }
}

fn open_url(text: &str) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(text)) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

fn back() {
    if let Some(mtm) = MainThreadMarker::new() {
        show(mtm);
    }
}

fn close_guide() {
    let handles = {
        let mut guard = UI.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_mut().map(|ui| {
            ui.guide = None;
            (ui.window.clone(), ui.helper.clone())
        })
    };
    if let Some((window, helper)) = handles {
        window.orderOut(None);
        helper.orderOut(None);
    }
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    struct Controller;
    unsafe impl NSObjectProtocol for Controller {}
    impl Controller {
        #[unsafe(method(beginGuide:))]
        fn begin(&self, _sender: &AnyObject) {
            if let Some(pane) = Permissions::current().next() { show_pane(pane); }
            else {
                let restart = UI.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|ui| !ui.initially_screen_granted);
                if restart { crate::relaunch_app(self.mtm()); }
                else { close_guide(); }
            }
        }
        #[unsafe(method(openScreen:))]
        fn screen(&self, _sender: &AnyObject) { show_pane(Pane::Screen); }
        #[unsafe(method(openAccessibility:))]
        fn access(&self, _sender: &AnyObject) { show_pane(Pane::Accessibility); }
        #[unsafe(method(back:))]
        fn go_back(&self, _sender: &AnyObject) { back(); }
        #[unsafe(method(revealSettings:))]
        fn reveal(&self, _sender: &AnyObject) {
            let pane = UI.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|ui| ui.guide.map(|g| g.pane));
            if let Some(pane) = pane { open_url(pane.url()); refresh(); }
        }
        #[unsafe(method(restartGuide:))]
        fn restart(&self, _sender: &AnyObject) {
            let pane = UI.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|ui| ui.guide.map(|g| g.pane));
            if let Some(pane) = pane {
                NSUserDefaults::standardUserDefaults().setInteger_forKey(pane.resume_value(), &NSString::from_str(RESUME_KEY));
            }
            crate::relaunch_app(self.mtm());
        }
        #[unsafe(method(closeGuide:))]
        fn close(&self, _sender: &AnyObject) {
            close_guide();
        }
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &AnyObject) { refresh(); }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_bundle_drag_exports_a_file_url_readable_by_other_apps() {
        use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
        let root =
            std::env::temp_dir().join(format!("irec-permission-pasteboard-{}", std::process::id()));
        let bundle = root.join("iRec test.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            b"<plist><dict/></plist>",
        )
        .unwrap();
        assert!(app_bundle_path(&bundle).is_some());
        let url = NSURL::fileURLWithPath_isDirectory(
            &NSString::from_str(&bundle.to_string_lossy()),
            true,
        );
        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*url);
        // A private board exercises the same cross-process file URL format as
        // dragging without touching the user's clipboard or granting access.
        let board = NSPasteboard::pasteboardWithUniqueName();
        assert!(board.writeObjects(&NSArray::from_slice(&[writer])));
        let exported = board
            .stringForType(unsafe { NSPasteboardTypeFileURL })
            .unwrap();
        let received = NSURL::URLWithString(&exported).unwrap();
        assert_eq!(received.path(), url.path());
        board.clearContents();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capture_entry_points_use_the_drag_guide_without_blocking_stop_or_cancel() {
        for action in [
            Action::ShotFull,
            Action::PickShot,
            Action::PickScrollShot,
            Action::ToggleFull,
            Action::PickRegion,
        ] {
            assert!(starts_capture(&action, false));
        }
        for action in [
            Action::Stop,
            Action::CancelScrollShot,
            Action::FinishScrollShot,
        ] {
            assert!(!starts_capture(&action, true));
            assert!(!starts_capture(&action, false));
        }
        assert!(!starts_capture(&Action::ToggleFull, true));
        assert!(!starts_capture(&Action::PickRegion, true));
        let screen_only = Permissions {
            screen: true,
            access: false,
        };
        assert_eq!(screen_only.missing_for_capture(&Action::ShotFull), None);
        assert_eq!(
            screen_only.missing_for_capture(&Action::PickScrollShot),
            None,
            "scroll capture has visual detection fallback without Accessibility permission"
        );
        assert_eq!(
            screen_only.missing_for_capture(&Action::ScrollShotRegion(
                crate::state::Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 100.0,
                    h: 100.0,
                },
                1,
            )),
            None
        );
        assert_eq!(
            Permissions {
                screen: false,
                access: true
            }
            .missing_for_capture(&Action::ShotFull),
            Some(Pane::Screen)
        );
    }

    #[test]
    fn accepted_drop_is_not_reported_as_a_permission_grant() {
        let mut guide = GuideState {
            pane: Pane::Screen,
            initially_granted: false,
            dragging: false,
            dropped: true,
            opened_at: Instant::now(),
        };
        assert!(!guide.newly_granted(Permissions {
            screen: false,
            access: true
        }));
        assert!(guide.newly_granted(Permissions {
            screen: true,
            access: false
        }));
        guide.dragging = true;
        assert!(!guide.newly_granted(Permissions {
            screen: true,
            access: true
        }));
        guide.dragging = false;
        guide.initially_granted = true;
        assert!(!guide.newly_granted(Permissions {
            screen: true,
            access: true
        }));
    }

    #[test]
    fn only_system_permission_states_advance_to_the_next_pane() {
        assert_eq!(
            Permissions {
                screen: false,
                access: false
            }
            .next(),
            Some(Pane::Screen)
        );
        assert_eq!(
            Permissions {
                screen: true,
                access: false
            }
            .next(),
            Some(Pane::Accessibility)
        );
        assert_eq!(
            Permissions {
                screen: false,
                access: true
            }
            .next(),
            Some(Pane::Screen)
        );
        assert_eq!(
            Permissions {
                screen: true,
                access: true
            }
            .next(),
            None
        );
        assert_eq!(
            Pane::from_resume(Pane::Screen.resume_value()),
            Some(Pane::Screen)
        );
        assert_eq!(
            Pane::from_resume(Pane::Accessibility.resume_value()),
            Some(Pane::Accessibility)
        );
        assert_eq!(Pane::from_resume(0), None);
    }

    #[test]
    fn guide_stays_beside_settings_and_inside_each_display() {
        for (settings, visible) in [
            (
                rect(400.0, 200.0, 720.0, 640.0),
                rect(0.0, 0.0, 1728.0, 1080.0),
            ),
            (
                rect(980.0, 200.0, 720.0, 640.0),
                rect(0.0, 0.0, 1728.0, 1080.0),
            ),
            (rect(0.0, 0.0, 800.0, 600.0), rect(0.0, 0.0, 800.0, 600.0)),
            (
                rect(-1600.0, 100.0, 720.0, 640.0),
                rect(-1920.0, -200.0, 1920.0, 1080.0),
            ),
        ] {
            let helper = helper_frame(settings, visible, NSSize::new(GUIDE_W, GUIDE_H + 28.0));
            assert!(helper.origin.x >= visible.origin.x && helper.max().x <= visible.max().x);
            assert!(helper.origin.y >= visible.origin.y && helper.max().y <= visible.max().y);
            if visible.max().x - settings.max().x >= GUIDE_W + 12.0 {
                assert!(helper.origin.x > settings.max().x);
            } else if settings.origin.x - visible.origin.x >= GUIDE_W + 12.0 {
                assert!(helper.max().x < settings.origin.x);
            }
        }
    }

    #[test]
    fn raw_executable_is_never_offered_as_the_application_to_authorize() {
        assert!(app_bundle_path(Path::new("target/debug/iRec")).is_none());
        assert!(app_bundle_path(Path::new("/missing/iRec.app")).is_none());
    }
}
