mod frame;
mod hotkey;
mod hud;
mod overlay;
mod recorder;
mod state;

use std::cell::{OnceCell, RefCell};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::AnyThread;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSBackingStoreType, NSBezierPath, NSButton, NSButtonType, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSFont, NSImage, NSMenu, NSMenuItem,
    NSPopUpButton, NSStatusBar, NSStatusItem, NSTextField, NSWarningAlertStyle, NSWindow,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString, NSURL,
};

use crate::overlay::pick_region;
use crate::recorder::Recorder;
use crate::state::{
    init_state, log, push_action, with_state, Action, AppState, Quality, Rect, Settings, STATE,
};

#[derive(Default)]
struct AppIvars {
    status_item: OnceCell<Retained<NSStatusItem>>,
    audio_item: OnceCell<Retained<NSMenuItem>>,
    quality_items: RefCell<Vec<(usize, Retained<NSMenuItem>)>>,
    fps_items: RefCell<Vec<(i32, Retained<NSMenuItem>)>>,
    scale_items: RefCell<Vec<(i32, Retained<NSMenuItem>)>>,
    last_file: OnceCell<Retained<NSMenuItem>>,
    status_line: OnceCell<Retained<NSMenuItem>>,
    window: OnceCell<Retained<NSWindow>>,
    status_label: OnceCell<Retained<NSTextField>>,
    audio_button: OnceCell<Retained<NSButton>>,
    start_button: OnceCell<Retained<NSButton>>,
    region_button: OnceCell<Retained<NSButton>>,
    stop_button: OnceCell<Retained<NSButton>>,
    stop_menu_item: OnceCell<Retained<NSMenuItem>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppIvars]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    // SAFETY: `NSApplicationDelegate` has no safety requirements.
    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            // Deliberately no CGRequestScreenCaptureAccess() here: preflight is
            // unreliable on macOS 15 and requesting on every launch makes
            // macOS re-prompt even when access was already granted. Permission
            // is only requested when a capture actually fails.
            log("started");
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            // Menu-bar app: the window is optional, so closing it just hides it.
            false
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            hud::close();
            self.stop_sync_if_recording();
        }
    }

    impl AppDelegate {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &AnyObject) {
            let mut pending = Vec::new();
            with_state(|s| {
                while let Some(action) = s.actions.pop_front() {
                    pending.push(action);
                }
            });
            for action in pending {
                log(&format!("action {action:?}"));
                self.handle(action);
            }
            // If the encoder gave up mid-recording, close the file right away
            // instead of silently recording nothing.
            let failure = with_state(|s| s.recorder.as_ref().and_then(|r| r.failed()));
            if let Some(err) = failure {
                log(&format!("aborting recording: {err}"));
                with_state(|s| s.last_error = Some(err));
                self.finish();
            }
            self.refresh_status();
            self.refresh_menu();
            self.update_window();
            let (recording, elapsed) = with_state(|s| {
                let elapsed = s.started_at.map(|started| {
                    let secs = started.elapsed().as_secs();
                    format!("{:02}:{:02}", secs / 60, secs % 60)
                });
                (s.recorder.is_some(), elapsed)
            });
            hud::sync(recording, elapsed);
        }

        #[unsafe(method(startFull:))]
        fn start_full(&self, _sender: &AnyObject) {
            push_action(Action::ToggleFull);
        }

        #[unsafe(method(pickRegion:))]
        fn pick_region_action(&self, _sender: &AnyObject) {
            push_action(Action::PickRegion);
        }

        #[unsafe(method(stopRecording:))]
        fn stop_recording(&self, _sender: &AnyObject) {
            push_action(Action::Stop);
        }

        #[unsafe(method(toggleAudio:))]
        fn toggle_audio(&self, _sender: &AnyObject) {
            with_state(|s| s.settings.audio = !s.settings.audio);
        }

        #[unsafe(method(setQuality:))]
        fn set_quality(&self, sender: &AnyObject) {
            let tag = unsafe { objc2::msg_send![sender, tag] };
            let quality = match tag {
                0 => Quality::Small,
                2 => Quality::High,
                3 => Quality::Max,
                _ => Quality::Balanced,
            };
            with_state(|s| s.settings.quality = quality);
        }

        #[unsafe(method(setFps:))]
        fn set_fps(&self, sender: &AnyObject) {
            let tag: i64 = unsafe { objc2::msg_send![sender, tag] };
            with_state(|s| s.settings.fps = tag.max(1) as i32);
        }

        #[unsafe(method(setScale:))]
        fn set_scale(&self, sender: &AnyObject) {
            let tag: i64 = unsafe { objc2::msg_send![sender, tag] };
            with_state(|s| s.settings.scale = tag as f64 / 10.0);
        }

        #[unsafe(method(showWindow:))]
        fn show_window(&self, _sender: &AnyObject) {
            if let Some(window) = self.ivars().window.get() {
                window.makeKeyAndOrderFront(None);
                NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
            }
        }

        #[unsafe(method(checkPermission:))]
        fn check_permission(&self, _sender: &AnyObject) {
            // Asking explicitly shows macOS' own prompt when needed.
            let granted = crate::recorder::request_permission(true)
                || unsafe { CGPreflightScreenCaptureAccess() };
            let alert = unsafe { NSAlert::new(self.mtm()) };
            if granted {
                alert.setMessageText(&NSString::from_str("屏幕录制权限已开启"));
                alert.setInformativeText(&NSString::from_str("可以开始录制了。"));
            } else {
                alert.setMessageText(&NSString::from_str("屏幕录制权限未开启"));
                alert.setInformativeText(&NSString::from_str(
                    "1) 在 系统设置 → 隐私与安全性 → 屏幕录制 里勾选 iRec；\
                     2) 如果列表里没有 iRec，先按一次 ⇧⌘R 让系统弹出授权框并选“允许”；\
                     3) 改完退出并重新打开 iRec。",
                ));
            }
            alert.addButtonWithTitle(&NSString::from_str("打开系统设置"));
            alert.addButtonWithTitle(&NSString::from_str("好"));
            if alert.runModal() == NSAlertFirstButtonReturn {
                open_screen_capture_settings();
            }
            log(&format!("permission check: preflight={granted}"));
        }

        #[unsafe(method(qualityChanged:))]
        fn quality_changed(&self, sender: &NSPopUpButton) {
            let index = sender.indexOfSelectedItem().max(0) as usize;
            let quality = Quality::all().get(index).copied().unwrap_or(Quality::Balanced);
            with_state(|s| s.settings.quality = quality);
        }

        #[unsafe(method(fpsChanged:))]
        fn fps_changed(&self, sender: &NSPopUpButton) {
            let index = sender.indexOfSelectedItem();
            let fps = if index == 1 { 60 } else { 30 };
            with_state(|s| s.settings.fps = fps);
        }

        #[unsafe(method(scaleChanged:))]
        fn scale_changed(&self, sender: &NSPopUpButton) {
            let index = sender.indexOfSelectedItem();
            with_state(|s| s.settings.scale = if index == 1 { 2.0 } else { 1.0 });
        }

        #[unsafe(method(revealLast:))]
        fn reveal_last(&self, _sender: &AnyObject) {
            let path = with_state(|s| s.last_saved.as_ref().map(|(p, _)| p.clone()));
            if let Some(path) = path {
                let url = NSURL::fileURLWithPath_isDirectory(
                    &NSString::from_str(path.to_string_lossy().as_ref()),
                    false,
                );
                let urls = NSArray::from_retained_slice(&[url]);
                NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&urls);
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: &AnyObject) {
            hud::close();
            hotkey::unregister_all();
            self.stop_sync_if_recording();
            let app = NSApplication::sharedApplication(self.mtm());
            app.terminate(None);
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn setup(&self, mtm: MainThreadMarker) {
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
        if let Some(button) = item.button(mtm) {
            button.setTitle(ns_string!("REC"));
            button.setImage(Some(&status_icon(mtm, false)));
        }
        item.setMenu(Some(&self.build_menu(mtm)));
        self.ivars().status_item.set(item).ok();

        if let Err(err) = hotkey::register_defaults() {
            eprintln!("irec: hotkey registration failed: {err}");
            with_state(|s| s.last_error = Some(format!("快捷键注册失败：{err}")));
        }

        // Built but not shown: the app starts silently in the menu bar.
        let window = self.build_window(mtm);
        self.ivars().window.set(window).ok();

        let timer = unsafe {
            objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.25,
                self,
                sel!(tick:),
                None,
                true,
            )
        };
        std::mem::forget(timer);
    }

    /// Builds the control window: buttons, quality pickers and live status.
    fn build_window(&self, mtm: MainThreadMarker) -> Retained<NSWindow> {
        let width = 360.0;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, 470.0));
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setSharingType(objc2_app_kit::NSWindowSharingType(0));
        window.setTitle(ns_string!("iRec"));
        let content = window.contentView().expect("content view");

        let mut y = 422.0;
        let title = make_label(
            mtm,
            "iRec · 屏幕录制",
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(width - 40.0, 24.0)),
            16.0,
        );
        content.addSubview(&title);

        y -= 40.0;
        let start = make_button(
            mtm,
            "开始录制（全屏）",
            sel!(startFull:),
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(width - 40.0, 32.0)),
            self,
        );
        content.addSubview(&start);
        self.ivars().start_button.set(start).ok();

        y -= 40.0;
        let region = make_button(
            mtm,
            "选择区域录制",
            sel!(pickRegion:),
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(width - 40.0, 32.0)),
            self,
        );
        content.addSubview(&region);
        self.ivars().region_button.set(region).ok();

        y -= 40.0;
        let stop = make_button(
            mtm,
            "停止录制",
            sel!(stopRecording:),
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(width - 40.0, 32.0)),
            self,
        );
        content.addSubview(&stop);
        self.ivars().stop_button.set(stop).ok();

        y -= 40.0;
        let audio = make_button(
            mtm,
            "录制系统声音",
            sel!(toggleAudio:),
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(width - 40.0, 24.0)),
            self,
        );
        audio.setButtonType(NSButtonType::Switch);
        audio.setState(NSControlStateValueOn);
        content.addSubview(&audio);
        self.ivars().audio_button.set(audio).ok();

        y -= 44.0;
        let quality_label = make_label(
            mtm,
            "画质",
            NSRect::new(NSPoint::new(20.0, y + 4.0), NSSize::new(50.0, 18.0)),
            13.0,
        );
        content.addSubview(&quality_label);
        let quality = make_popup(
            mtm,
            &[
                Quality::Small.label(),
                Quality::Balanced.label(),
                Quality::High.label(),
                Quality::Max.label(),
            ],
            NSRect::new(NSPoint::new(90.0, y), NSSize::new(width - 110.0, 26.0)),
            sel!(qualityChanged:),
            self,
            1,
        );
        content.addSubview(&quality);

        y -= 34.0;
        let fps_label = make_label(
            mtm,
            "帧率",
            NSRect::new(NSPoint::new(20.0, y + 4.0), NSSize::new(50.0, 18.0)),
            13.0,
        );
        content.addSubview(&fps_label);
        let fps = make_popup(
            mtm,
            &["30 fps（省空间）", "60 fps（流畅）"],
            NSRect::new(NSPoint::new(90.0, y), NSSize::new(width - 110.0, 26.0)),
            sel!(fpsChanged:),
            self,
            0,
        );
        content.addSubview(&fps);

        y -= 34.0;
        let scale_label = make_label(
            mtm,
            "分辨率",
            NSRect::new(NSPoint::new(20.0, y + 4.0), NSSize::new(50.0, 18.0)),
            13.0,
        );
        content.addSubview(&scale_label);
        let scale = make_popup(
            mtm,
            &["1x（省空间）", "2x（高清，默认）"],
            NSRect::new(NSPoint::new(90.0, y), NSSize::new(width - 110.0, 26.0)),
            sel!(scaleChanged:),
            self,
            1,
        );
        content.addSubview(&scale);

        y -= 44.0;
        let status = make_label(
            mtm,
            "就绪 · 快捷键 ⇧⌘R 全屏 / ⇧⌘E 选区",
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(width - 40.0, 20.0)),
            13.0,
        );
        content.addSubview(&status);
        self.ivars().status_label.set(status).ok();

        y -= 40.0;
        let reveal = make_button(
            mtm,
            "在访达中显示",
            sel!(revealLast:),
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(150.0, 30.0)),
            self,
        );
        content.addSubview(&reveal);
        let quit = make_button(
            mtm,
            "退出",
            sel!(quit:),
            NSRect::new(NSPoint::new(190.0, y), NSSize::new(width - 210.0, 30.0)),
            self,
        );
        content.addSubview(&quit);

        y -= 38.0;
        let permission = make_button(
            mtm,
            "申请 / 检查屏幕录制权限",
            sel!(checkPermission:),
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(width - 40.0, 30.0)),
            self,
        );
        content.addSubview(&permission);

        window.center();
        window
    }

    fn build_menu(&self, mtm: MainThreadMarker) -> Retained<NSMenu> {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("irec"));
        // Otherwise AppKit re-enables the stop item on every menu open.
        menu.setAutoenablesItems(false);

        let start = menu_item(mtm, "开始录制（全屏）   ⇧⌘R", Some(sel!(startFull:)), self, 0);
        menu.addItem(&start);

        let region = menu_item(mtm, "选择区域录制      ⇧⌘E", Some(sel!(pickRegion:)), self, 0);
        menu.addItem(&region);

        let stop = menu_item(mtm, "停止录制", Some(sel!(stopRecording:)), self, 0);
        menu.addItem(&stop);
        self.ivars().stop_menu_item.set(stop).ok();

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let audio = menu_item(mtm, "录制声音（系统音频）", Some(sel!(toggleAudio:)), self, 0);
        menu.addItem(&audio);
        self.ivars().audio_item.set(audio).ok();

        // 画质
        let quality_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("画质"));
        for (index, quality) in Quality::all().iter().enumerate() {
            let item = menu_item(
                mtm,
                quality.label(),
                Some(sel!(setQuality:)),
                self,
                index as i64,
            );
            quality_menu.addItem(&item);
            self.ivars().quality_items.borrow_mut().push((index, item));
        }
        let quality_root = menu_item(mtm, "画质（越小越省空间）", None, self, 0);
        menu.addItem(&quality_root);
        menu.setSubmenu_forItem(Some(&quality_menu), &quality_root);

        // 帧率
        let fps_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("帧率"));
        for fps in [30i32, 60i32] {
            let item = menu_item(
                mtm,
                &format!("{fps} fps"),
                Some(sel!(setFps:)),
                self,
                fps as i64,
            );
            fps_menu.addItem(&item);
            self.ivars().fps_items.borrow_mut().push((fps, item));
        }
        let fps_root = menu_item(mtm, "帧率", None, self, 0);
        menu.addItem(&fps_root);
        menu.setSubmenu_forItem(Some(&fps_menu), &fps_root);

        // 分辨率
        let scale_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("分辨率"));
        for scale in [10i32, 20i32] {
            let label = if scale == 20 { "2x（高清，默认）" } else { "1x（省空间）" };
            let item = menu_item(mtm, label, Some(sel!(setScale:)), self, scale as i64);
            scale_menu.addItem(&item);
            self.ivars().scale_items.borrow_mut().push((scale, item));
        }
        let scale_root = menu_item(mtm, "分辨率", None, self, 0);
        menu.addItem(&scale_root);
        menu.setSubmenu_forItem(Some(&scale_menu), &scale_root);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let reveal = menu_item(mtm, "在访达中显示最新录制", Some(sel!(revealLast:)), self, 0);
        menu.addItem(&reveal);

        let open_window = menu_item(mtm, "打开控制窗口…", Some(sel!(showWindow:)), self, 0);
        menu.addItem(&open_window);

        let permission = menu_item(
            mtm,
            "申请 / 检查屏幕录制权限",
            Some(sel!(checkPermission:)),
            self,
            0,
        );
        menu.addItem(&permission);
        self.ivars().last_file.set(reveal).ok();

        let status_line = menu_item(mtm, "就绪", None, self, 0);
        status_line.setHidden(true);
        menu.addItem(&status_line);
        self.ivars().status_line.set(status_line).ok();

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let quit = menu_item(mtm, "退出", Some(sel!(quit:)), self, 0);
        menu.addItem(&quit);

        menu
    }

    fn handle(&self, action: Action) {
        let (recording, stopping) = with_state(|s| (s.recorder.is_some(), s.stopping));
        if stopping {
            // Still closing the previous file; ignore new commands for now.
            return;
        }
        match action {
            Action::ToggleFull => {
                if recording {
                    self.finish();
                } else {
                    hud::hide();
                    self.start(None);
                }
            }
            Action::PickRegion => {
                if recording {
                    self.finish();
                } else {
                    hud::hide();
                    pick_region(self.mtm());
                }
            }
            Action::StartRegion(rect) => {
                if !recording {
                    self.start(Some(rect));
                }
            }
            Action::Stop => {
                if recording {
                    self.finish();
                }
            }
        }
    }

    /// Opens the Screen Recording pane of System Settings.
    fn open_settings(&self) {
        open_screen_capture_settings();
    }

    /// Tells the user how to grant screen recording access and offers to open
    /// the relevant System Settings pane.
    fn show_permission_alert(&self) {
        let alert = unsafe { NSAlert::new(self.mtm()) };
        alert.setAlertStyle(NSWarningAlertStyle);
        alert.setMessageText(&NSString::from_str("需要“屏幕录制”权限"));
        alert.setInformativeText(&NSString::from_str(
            "请在 系统设置 → 隐私与安全性 → 屏幕录制 中勾选 iRec，然后重新录制。",
        ));
        alert.addButtonWithTitle(&NSString::from_str("打开系统设置"));
        alert.addButtonWithTitle(&NSString::from_str("好"));
        if alert.runModal() == NSAlertFirstButtonReturn {
            self.open_settings();
        }
    }

    fn start(&self, rect: Option<Rect>) {
        let settings = with_state(|s| s.settings.clone());
        match Recorder::start(rect, &settings) {
            Ok(recorder) => with_state(|s| {
                s.recorder = Some(recorder);
                s.started_at = Some(Instant::now());
                s.last_error = None;
            }),
            Err(err) => {
                eprintln!("irec: {err}");
                with_state(|s| s.last_error = Some(err));
            }
        }
    }

    /// Stops the recording in the background so the main run loop keeps
    /// running while AVAssetWriter closes the file.
    fn finish(&self) {
        let recorder = with_state(|s| s.recorder.take());
        let Some(recorder) = recorder else { return };
        with_state(|s| s.stopping = true);
        std::thread::spawn(move || {
            let result = recorder.stop();
            with_state(|s| {
                s.stopping = false;
                s.started_at = None;
                match result {
                    Ok((path, bytes, frames)) => {
                        log(&format!(
                            "saved {} ({:.1} MB, {frames} frames)",
                            path.display(),
                            bytes as f64 / 1_048_576.0
                        ));
                        s.last_saved = Some((path, bytes));
                        s.saved_at = Some(Instant::now());
                        s.last_error = None;
                    }
                    Err(err) => {
                        log(&format!("stop failed: {err}"));
                        s.last_error = Some(err);
                    }
                }
            });
        });
    }

    /// Used when quitting: block until the file is closed.
    fn stop_sync_if_recording(&self) {
        let recorder = with_state(|s| s.recorder.take());
        if let Some(recorder) = recorder {
            let _ = recorder.stop();
            with_state(|s| {
                s.stopping = false;
                s.started_at = None;
            });
        }
    }

    /// Mirrors the current state into the control window.
    fn update_window(&self) {
        let (label, recording, stopping, audio) = with_state(|s| {
            (
                window_status(s),
                s.recorder.is_some(),
                s.stopping,
                s.settings.audio,
            )
        });
        if let Some(field) = self.ivars().status_label.get() {
            field.setStringValue(&NSString::from_str(&label));
        }
        if let Some(button) = self.ivars().audio_button.get() {
            button.setState(if audio {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        if let Some(button) = self.ivars().start_button.get() {
            button.setTitle(&NSString::from_str(if stopping {
                "正在保存…"
            } else if recording {
                "录制中…"
            } else {
                "开始录制（全屏）"
            }));
            button.setEnabled(!recording && !stopping);
        }
        if let Some(button) = self.ivars().region_button.get() {
            button.setEnabled(!recording && !stopping);
        }
        // "停止录制" only does something while a recording is running.
        if let Some(button) = self.ivars().stop_button.get() {
            button.setEnabled(recording && !stopping);
        }
    }

    fn refresh_status(&self) {
        let mtm = self.mtm();
        let Some(item) = self.ivars().status_item.get() else { return };
        let Some(button) = item.button(mtm) else { return };

        let (title, recording) = with_state(|s| (status_title(s), s.recorder.is_some()));
        button.setTitle(&NSString::from_str(&title));
        button.setImage(Some(&status_icon(mtm, recording)));
    }

    fn refresh_menu(&self) {
        let settings = with_state(|s| s.settings.clone());
        let (recording, stopping) = with_state(|s| (s.recorder.is_some(), s.stopping));
        if let Some(item) = self.ivars().stop_menu_item.get() {
            item.setEnabled(recording && !stopping);
        }
        if let Some(item) = self.ivars().audio_item.get() {
            item.setState(if settings.audio {
                objc2_app_kit::NSControlStateValueOn
            } else {
                objc2_app_kit::NSControlStateValueOff
            });
        }
        for (index, item) in self.ivars().quality_items.borrow().iter() {
            let matches = Quality::all()[*index] == settings.quality;
            item.setState(state_value(matches));
        }
        for (fps, item) in self.ivars().fps_items.borrow().iter() {
            item.setState(state_value(*fps == settings.fps));
        }
        for (scale, item) in self.ivars().scale_items.borrow().iter() {
            item.setState(state_value((*scale as f64 / 10.0 - settings.scale).abs() < 0.01));
        }
        if let Some(item) = self.ivars().last_file.get() {
            let has_file = with_state(|s| s.last_saved.is_some());
            item.setHidden(!has_file);
        }
        if let Some(item) = self.ivars().status_line.get() {
            let text = with_state(|s| {
                if let Some(err) = &s.last_error {
                    Some(format!("⚠ {err}"))
                } else if let Some((path, bytes)) = &s.last_saved {
                    Some(format!(
                        "已保存 {}（{:.1} MB）",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        *bytes as f64 / 1_048_576.0
                    ))
                } else {
                    None
                }
            });
            match text {
                Some(text) => {
                    item.setTitle(&NSString::from_str(&text));
                    item.setHidden(false);
                }
                None => item.setHidden(true),
            }
        }
    }
}

/// Draws the menu bar icon: a filled red dot while recording, a ring when idle.
fn status_icon(mtm: MainThreadMarker, recording: bool) -> Retained<NSImage> {
    let size = NSSize::new(18.0, 18.0);
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    image.lockFocus();
    unsafe {
        let rect = NSRect::new(NSPoint::new(3.0, 3.0), NSSize::new(12.0, 12.0));
        let dot = NSBezierPath::bezierPathWithOvalInRect(rect);
        if recording {
            NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.25, 0.25, 1.0).setFill();
            dot.fill();
        } else {
            NSColor::colorWithSRGBRed_green_blue_alpha(0.55, 0.55, 0.58, 1.0).setStroke();
            dot.setLineWidth(1.5);
            dot.stroke();
        }
    }
    image.unlockFocus();
    image
}

fn make_label(mtm: MainThreadMarker, text: &str, frame: NSRect, size: f64) -> Retained<NSTextField> {
    let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
    field.setStringValue(&NSString::from_str(text));
    field.setBezeled(false);
    field.setDrawsBackground(false);
    field.setEditable(false);
    field.setSelectable(false);
    field.setFont(Some(&NSFont::systemFontOfSize(size)));
    field
}

fn make_button(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    frame: NSRect,
    target: &AppDelegate,
) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    button.setTitle(&NSString::from_str(title));
    button.setBezelStyle(objc2_app_kit::NSBezelStyle::Rounded);
    unsafe {
        button.setTarget(Some(target));
        button.setAction(Some(action));
    }
    button
}

fn make_popup(
    mtm: MainThreadMarker,
    items: &[&str],
    frame: NSRect,
    action: Sel,
    target: &AppDelegate,
    selected: isize,
) -> Retained<NSPopUpButton> {
    let popup = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), frame, false);
    let titles: Vec<Retained<NSString>> = items.iter().map(|t| NSString::from_str(t)).collect();
    popup.addItemsWithTitles(&NSArray::from_retained_slice(&titles));
    popup.selectItemAtIndex(selected);
    unsafe {
        popup.setTarget(Some(target));
        popup.setAction(Some(action));
    }
    popup
}

/// Main menu so Cmd+Q works in a nib-less app.
fn build_main_menu(mtm: MainThreadMarker, target: &AppDelegate) -> Retained<NSMenu> {
    let main = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("MainMenu"));
    let app_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("iRec"));
    let quit = menu_item(mtm, "退出 iRec", Some(sel!(quit:)), target, 0);
    quit.setKeyEquivalent(ns_string!("q"));
    quit.setKeyEquivalentModifierMask(objc2_app_kit::NSEventModifierFlags::Command);
    app_menu.addItem(&quit);

    let app_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("iRec"),
            None,
            ns_string!(""),
        )
    };
    app_item.setSubmenu(Some(&app_menu));
    main.addItem(&app_item);
    std::mem::forget(app_menu);
    std::mem::forget(app_item);
    main
}

fn open_screen_capture_settings() {
    let url = NSURL::URLWithString(&NSString::from_str(
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
    ));
    if let Some(url) = Option::<Retained<NSURL>>::from(url) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

fn window_status(state: &AppState) -> String {
    if let Some(started) = state.started_at {
        let secs = started.elapsed().as_secs();
        return format!("● 录制中 {:02}:{:02}", secs / 60, secs % 60);
    }
    if state.stopping {
        return "正在保存…".to_string();
    }
    if let Some(err) = &state.last_error {
        return format!("⚠ {err}");
    }
    if let Some((path, bytes)) = &state.last_saved {
        return format!(
            "已保存 {}（{:.1} MB）",
            path.file_name().unwrap_or_default().to_string_lossy(),
            *bytes as f64 / 1_048_576.0
        );
    }
    "就绪 · 快捷键 ⇧⌘R 全屏 / ⇧⌘E 选区".to_string()
}

fn state_value(on: bool) -> objc2_app_kit::NSControlStateValue {
    if on {
        objc2_app_kit::NSControlStateValueOn
    } else {
        objc2_app_kit::NSControlStateValueOff
    }
}

fn status_title(state: &AppState) -> String {
    if let Some(started) = state.started_at {
        let secs = started.elapsed().as_secs();
        return format!("● {:02}:{:02}", secs / 60, secs % 60);
    }
    if let Some(saved_at) = state.saved_at {
        if saved_at.elapsed() < Duration::from_secs(8) {
            if let Some((_, bytes)) = &state.last_saved {
                return format!("✓ {:.1} MB", *bytes as f64 / 1_048_576.0);
            }
        }
    }
    if let Some(err) = &state.last_error {
        if err.len() > 12 {
            return "⚠ 出错了".to_string();
        }
        return format!("⚠ {err}");
    }
    "REC".to_string()
}

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<Sel>,
    target: &AppDelegate,
    tag: i64,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            ns_string!(""),
        )
    };
    unsafe { item.setTarget(Some(target)) };
    item.setTag(tag as isize);
    item
}

// Asks macOS for screen recording access up front so the user sees the
// permission prompt as soon as the app launches.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGPreflightScreenCaptureAccess() -> bool;
}

fn main() {
    init_state();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        selftest(&args);
        return;
    }

    let mtm = MainThreadMarker::new().expect("irec must start on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Menu-bar app: no Dock icon and no window on launch. Everything is
    // reachable from the status item menu (plus the two global hotkeys).
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);


    let delegate = AppDelegate::new(mtm);
    unsafe { app.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
    app.setMainMenu(Some(&build_main_menu(mtm, &delegate)));
    delegate.setup(mtm);
    println!("iRec: 已常驻菜单栏，点击 REC 图标操作，或使用 ⇧⌘R / ⇧⌘E");

    std::mem::forget(delegate);
    app.run();
}

/// Headless smoke test: records for a few seconds and prints the result.
fn selftest(args: &[String]) {
    fn value(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    }

    let seconds: f64 = value(args, "--seconds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3.0);
    let region = value(args, "--region").and_then(|v| {
        let parts: Vec<f64> = v.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        if parts.len() == 4 {
            Some(Rect {
                x: parts[0],
                y: parts[1],
                w: parts[2],
                h: parts[3],
            })
        } else {
            None
        }
    });

    let mut settings = with_state(|s| s.settings.clone());
    if args.iter().any(|a| a == "--no-audio") {
        settings.audio = false;
    }
    if let Some(fps) = value(args, "--fps").and_then(|v| v.parse().ok()) {
        settings.fps = fps;
    }
    if let Some(scale) = value(args, "--scale").and_then(|v| v.parse().ok()) {
        settings.scale = scale;
    }
    if let Some(dir) = value(args, "--out") {
        settings.out_dir = std::path::PathBuf::from(dir);
    }

    println!(
        "irec selftest: {} audio, {} fps, {}x, region {:?}",
        settings.audio, settings.fps, settings.scale, region
    );

    let recorder = match Recorder::start(region, &settings) {
        Ok(r) => r,
        Err(err) => {
            println!("FAILED to start: {err}");
            std::process::exit(1);
        }
    };
    println!("recording to {}", recorder.path().display());
    std::thread::sleep(Duration::from_secs_f64(seconds));
    match recorder.stop() {
        Ok((path, bytes, frames)) => println!(
            "OK {} ({:.2} MB, {frames} frames)",
            path.display(),
            bytes as f64 / 1_048_576.0
        ),
        Err(err) => {
            println!("FAILED to stop: {err}");
            std::process::exit(1);
        }
    }
}

#[allow(dead_code)]
fn state_ref() -> &'static std::sync::Mutex<Option<AppState>> {
    &STATE
}

#[allow(dead_code)]
fn settings_default() -> Settings {
    Settings::default()
}
