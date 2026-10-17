//! First-run permission guide.
//!
//! macOS does not let an app flip its own TCC switches. This window makes the
//! unavoidable part as small as possible: trigger the system prompts, open the
//! exact settings panes, watch both states live, and relaunch when ready.

use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSBezelStyle, NSButton, NSColor, NSFont, NSTextField,
    NSWindow, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
use objc2_foundation::{
    MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer, NSURL,
};

use crate::state::{persist, with_state};

const W: f64 = 560.0;
const H: f64 = 340.0;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const CFDictionary) -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

struct Ui {
    window: Retained<NSWindow>,
    screen_status: Retained<NSTextField>,
    access_status: Retained<NSTextField>,
    primary: Retained<NSButton>,
    note: Retained<NSTextField>,
    _controller: Retained<Controller>,
    _timer: Retained<NSTimer>,
}

// SAFETY: every AppKit object is created and touched on the main thread.
unsafe impl Send for Ui {}

static UI: Mutex<Option<Ui>> = Mutex::new(None);

pub fn screen_recording() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub fn accessibility() -> bool {
    unsafe { AXIsProcessTrusted() }
}

pub fn maybe_show_onboarding(mtm: MainThreadMarker) {
    if screen_recording() && accessibility() {
        return;
    }
    let prompted = with_state(|state| state.settings.permissions_prompted);
    if prompted {
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
    let mut guard = UI.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(ui) = guard.as_mut() {
        ui.window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        refresh();
        return;
    }

    let controller = Controller::new(mtm);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(W, H)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("iRec 权限"));
    let Some(content) = window.contentView() else {
        return;
    };

    let title = make_label(
        mtm,
        NSRect::new(NSPoint::new(28.0, 276.0), NSSize::new(W - 56.0, 30.0)),
        22.0,
        true,
        &NSColor::labelColor(),
    );
    title.setStringValue(&NSString::from_str("开始使用 iRec"));
    content.addSubview(&title);

    let subtitle = make_label(
        mtm,
        NSRect::new(NSPoint::new(28.0, 246.0), NSSize::new(W - 56.0, 20.0)),
        12.0,
        false,
        &NSColor::secondaryLabelColor(),
    );
    subtitle.setStringValue(&NSString::from_str(
        "macOS 不允许应用代替你打开权限开关，点申请后按系统提示确认即可。",
    ));
    content.addSubview(&subtitle);

    let screen_label = make_label(
        mtm,
        NSRect::new(NSPoint::new(28.0, 190.0), NSSize::new(130.0, 22.0)),
        14.0,
        true,
        &NSColor::labelColor(),
    );
    screen_label.setStringValue(&NSString::from_str("屏幕录制"));
    content.addSubview(&screen_label);
    let screen_status = make_label(
        mtm,
        NSRect::new(NSPoint::new(150.0, 191.0), NSSize::new(160.0, 20.0)),
        13.0,
        false,
        &NSColor::systemOrangeColor(),
    );
    content.addSubview(&screen_status);
    let screen_button = make_button(
        mtm,
        "打开",
        NSRect::new(NSPoint::new(452.0, 184.0), NSSize::new(80.0, 28.0)),
        sel!(openScreen:),
        &controller,
        false,
    );
    content.addSubview(&screen_button);

    let access_label = make_label(
        mtm,
        NSRect::new(NSPoint::new(28.0, 148.0), NSSize::new(130.0, 22.0)),
        14.0,
        true,
        &NSColor::labelColor(),
    );
    access_label.setStringValue(&NSString::from_str("辅助功能"));
    content.addSubview(&access_label);
    let access_status = make_label(
        mtm,
        NSRect::new(NSPoint::new(150.0, 149.0), NSSize::new(160.0, 20.0)),
        13.0,
        false,
        &NSColor::systemOrangeColor(),
    );
    content.addSubview(&access_status);
    let access_button = make_button(
        mtm,
        "打开",
        NSRect::new(NSPoint::new(452.0, 142.0), NSSize::new(80.0, 28.0)),
        sel!(openAccessibility:),
        &controller,
        false,
    );
    content.addSubview(&access_button);

    let note = make_label(
        mtm,
        NSRect::new(NSPoint::new(28.0, 100.0), NSSize::new(W - 56.0, 20.0)),
        11.0,
        false,
        &NSColor::tertiaryLabelColor(),
    );
    content.addSubview(&note);

    let close = make_button(
        mtm,
        "稍后",
        NSRect::new(NSPoint::new(220.0, 30.0), NSSize::new(68.0, 32.0)),
        sel!(closeGuide:),
        &controller,
        false,
    );
    content.addSubview(&close);
    let recheck = make_button(
        mtm,
        "重新检查",
        NSRect::new(NSPoint::new(298.0, 30.0), NSSize::new(92.0, 32.0)),
        sel!(recheck:),
        &controller,
        false,
    );
    content.addSubview(&recheck);
    let primary = make_button(
        mtm,
        "一键申请",
        NSRect::new(NSPoint::new(398.0, 28.0), NSSize::new(134.0, 34.0)),
        sel!(apply:),
        &controller,
        true,
    );
    content.addSubview(&primary);

    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.6,
            &controller,
            sel!(tick:),
            None,
            true,
        )
    };
    *guard = Some(Ui {
        window: window.clone(),
        screen_status,
        access_status,
        primary,
        note,
        _controller: controller,
        _timer: timer,
    });
    drop(guard);

    refresh();
    window.center();
    window.makeKeyAndOrderFront(None);
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

fn refresh() {
    let screen = screen_recording();
    let access = accessibility();
    let both = screen && access;
    let mut guard = UI.lock().unwrap_or_else(|error| error.into_inner());
    let Some(ui) = guard.as_mut() else {
        return;
    };
    ui.screen_status
        .setStringValue(&NSString::from_str(if screen {
            "● 已开启"
        } else {
            "● 未开启"
        }));
    let screen_color = if screen {
        NSColor::systemGreenColor()
    } else {
        NSColor::systemOrangeColor()
    };
    ui.screen_status.setTextColor(Some(&screen_color));
    ui.access_status
        .setStringValue(&NSString::from_str(if access {
            "● 已开启"
        } else {
            "● 未开启"
        }));
    let access_color = if access {
        NSColor::systemGreenColor()
    } else {
        NSColor::systemOrangeColor()
    };
    ui.access_status.setTextColor(Some(&access_color));
    ui.primary.setTitle(&NSString::from_str(if both {
        "重新启动 iRec"
    } else {
        "一键申请"
    }));
    ui.note.setStringValue(&NSString::from_str(if both {
        "两项权限已就绪。重新启动 iRec 后开始使用。"
    } else {
        "打开权限开关后回到这里，iRec 会自动检查状态。"
    }));
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
    button.setFont(Some(&NSFont::systemFontOfSize(if primary {
        13.0
    } else {
        12.0
    })));
    if primary {
        button.setContentTintColor(Some(&NSColor::controlAccentColor()));
        button.setKeyEquivalent(&NSString::from_str("\r"));
    }
    unsafe { button.setAction(Some(action)) };
    unsafe { button.setTarget(Some(target)) };
    button
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    impl Controller {
        #[unsafe(method(apply:))]
        fn apply(&self, _sender: &AnyObject) {
            if screen_recording() && accessibility() {
                if let Some(mtm) = MainThreadMarker::new() {
                    crate::relaunch_app(mtm);
                }
                return;
            }
            if !screen_recording() {
                let granted = unsafe { CGRequestScreenCaptureAccess() };
                if !granted {
                    open_url(
                        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
                    );
                }
            }
            if !accessibility() {
                prompt_accessibility();
            }
            refresh();
        }

        #[unsafe(method(recheck:))]
        fn recheck(&self, _sender: &AnyObject) {
            refresh();
        }

        #[unsafe(method(closeGuide:))]
        fn close_guide(&self, _sender: &AnyObject) {
            if let Some(ui) = UI
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
            {
                ui.window.orderOut(None);
            }
        }

        #[unsafe(method(openScreen:))]
        fn open_screen(&self, _sender: &AnyObject) {
            open_url(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
            );
        }

        #[unsafe(method(openAccessibility:))]
        fn open_accessibility(&self, _sender: &AnyObject) {
            open_url(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            );
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &AnyObject) {
            refresh();
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn prompt_accessibility() {
    let key = CFString::from_str("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::new(true);
    let options = CFDictionary::from_slices(&[&*key], &[value]);
    unsafe {
        AXIsProcessTrustedWithOptions(options.as_opaque());
    }
}

fn open_url(text: &str) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(text)) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}
