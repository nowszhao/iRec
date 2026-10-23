mod audio;
mod frame;
mod hotkey;
mod hud;
mod line;
mod markup;
mod mic;
mod overlay;
mod permissions;
mod preview;
mod recorder;
mod scrollpreview;
mod scrollshot;
mod shot;
mod state;
mod update;

use std::cell::{OnceCell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSBackingStoreType, NSBezierPath, NSButton, NSButtonType, NSColor,
    NSEvent, NSFont, NSImage, NSMenu, NSMenuItem, NSModalResponseOK, NSOpenPanel, NSPasteboard,
    NSPasteboardWriting, NSPopUpButton, NSStatusBar, NSStatusItem, NSTextField, NSView, NSWindow,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSBundle, NSNotification, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString, NSURL, ns_string,
};

use crate::hotkey::{Hotkey, Hotkeys, KVK_DELETE, KVK_ESCAPE, SLOTS};
use crate::overlay::pick_region;
use crate::recorder::Recorder;
use crate::state::{
    Action, AppState, AudioSource, Quality, Rect, STATE, ScrollShotResult, Settings, Update,
    dirs_desktop, init_state, load_persisted, log, persist, push_action, with_state,
};

#[derive(Default)]
struct AppIvars {
    status_item: OnceCell<Retained<NSStatusItem>>,
    status_logo: OnceCell<Retained<NSImage>>,
    status_line: OnceCell<Retained<NSMenuItem>>,
    window: OnceCell<Retained<NSWindow>>,
    status_label: OnceCell<Retained<NSTextField>>,
    audio_popup: OnceCell<Retained<NSPopUpButton>>,
    out_dir_button: OnceCell<Retained<NSButton>>,
    line_toggle_item: OnceCell<Retained<NSMenuItem>>,
    update_item: OnceCell<Retained<NSMenuItem>>,
    start_menu_item: OnceCell<Retained<NSMenuItem>>,
    region_menu_item: OnceCell<Retained<NSMenuItem>>,
    shot_full_item: OnceCell<Retained<NSMenuItem>>,
    shot_region_item: OnceCell<Retained<NSMenuItem>>,
    scroll_shot_item: OnceCell<Retained<NSMenuItem>>,
    finish_scroll_shot_item: OnceCell<Retained<NSMenuItem>>,
    cancel_scroll_shot_item: OnceCell<Retained<NSMenuItem>>,
    hotkey_window: OnceCell<Retained<NSWindow>>,
    hotkey_fields: RefCell<Vec<Retained<NSTextField>>>,
    capture_panel: OnceCell<Retained<NSWindow>>,
    capture_view: OnceCell<Retained<CaptureView>>,
    setting_checks: RefCell<Vec<(u8, Retained<NSButton>)>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppIvars]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    // SAFETY: `NSApplicationDelegate` has no safety requirements.
    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, _: &NSApplication, _: bool) -> bool {
            if let Some(window) = self.ivars().window.get() {
                window.makeKeyAndOrderFront(None);
            }
            false
        }

        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            // Startup checks are passive. Missing capture permissions use
            // the drag guide, without triggering the former request dialogs.
            log("started");
            line::init(self.mtm());
            let (enabled, hover) = with_state(|s| (s.settings.line, s.settings.line_hover));
            line::apply_settings(enabled, hover);
            permissions::maybe_show_onboarding(self.mtm());
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            // Menu-bar app: the window is optional, so closing it just hides it.
            false
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            hud::close();
            hotkey::unregister_scroll_cancel();
            scrollpreview::close(self.mtm());
            scrollshot::cancel_and_wait();
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
            // 捕获窗口关掉后把热键装回去（Esc 取消也是走这条）。
            if *CAPTURE_ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) {
                let open = self
                    .ivars()
                    .capture_panel
                    .get()
                    .map(|panel| panel.isVisible())
                    .unwrap_or(false);
                if !open {
                    *CAPTURE_ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = false;
                    self.apply_hotkeys();
                }
            }
            self.maybe_auto_check_update();
            scrollpreview::sync(self.mtm());
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

        #[unsafe(method(shotFull:))]
        fn shot_full(&self, _sender: &AnyObject) {
            push_action(Action::ShotFull);
        }

        #[unsafe(method(shotRegion:))]
        fn shot_region(&self, _sender: &AnyObject) {
            push_action(Action::PickShot);
        }

        #[unsafe(method(scrollShot:))]
        fn scroll_shot(&self, _sender: &AnyObject) {
            push_action(Action::PickScrollShot);
        }

        #[unsafe(method(finishScrollShot:))]
        fn finish_scroll_shot(&self, _sender: &AnyObject) {
            push_action(Action::FinishScrollShot);
        }

        #[unsafe(method(cancelScrollShot:))]
        fn cancel_scroll_shot(&self, _sender: &AnyObject) {
            push_action(Action::CancelScrollShot);
        }

        #[unsafe(method(annotateLast:))]
        fn annotate_last(&self, _sender: &AnyObject) {
            if let Some(path) = self.latest_shot() {
                push_action(Action::Markup(path));
            }
        }

        #[unsafe(method(openHotkeys:))]
        fn open_hotkeys_action(&self, _sender: &AnyObject) {
            self.show_hotkey_window();
        }

        #[unsafe(method(captureHotkey:))]
        fn capture_hotkey(&self, sender: &AnyObject) {
            let tag: i64 = unsafe { objc2::msg_send![sender, tag] };
            self.begin_capture(tag.max(0) as u32);
        }

        #[unsafe(method(restoreHotkeys:))]
        fn restore_hotkeys(&self, _sender: &AnyObject) {
            with_state(|s| {
                s.settings.hotkeys = Hotkeys::default();
                persist(&s.settings);
            });
            self.apply_hotkeys();
        }

        #[unsafe(method(checkUpdate:))]
        fn check_update_action(&self, _sender: &AnyObject) {
            push_action(Action::CheckUpdate);
        }

        #[unsafe(method(installUpdate:))]
        fn install_update_action(&self, _sender: &AnyObject) {
            self.install_update();
        }

        #[unsafe(method(toggleUpdateCheck:))]
        fn toggle_update_check(&self, _sender: &AnyObject) {
            with_state(|s| {
                s.settings.update_check = !s.settings.update_check;
                persist(&s.settings);
            });
        }

        #[unsafe(method(setAudioSource:))]
        fn set_audio_source(&self, sender: &AnyObject) {
            let tag: i64 = unsafe { objc2::msg_send![sender, tag] };
            let source = AudioSource::all()
                .get(tag.max(0) as usize)
                .copied()
                .unwrap_or(AudioSource::System);
            with_state(|s| {
                s.settings.audio = source;
                persist(&s.settings);
            });
            if source.wants_mic() {
                request_mic_access();
            }
        }

        #[unsafe(method(audioChanged:))]
        fn audio_changed(&self, sender: &NSPopUpButton) {
            let index = sender.indexOfSelectedItem().max(0) as usize;
            let source = AudioSource::all()
                .get(index)
                .copied()
                .unwrap_or(AudioSource::System);
            with_state(|s| {
                s.settings.audio = source;
                persist(&s.settings);
            });
            if source.wants_mic() {
                request_mic_access();
            }
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

        #[unsafe(method(chooseOutputDir:))]
        fn choose_output_dir(&self, _sender: &AnyObject) {
            let panel = NSOpenPanel::openPanel(self.mtm());
            panel.setCanChooseDirectories(true);
            panel.setCanChooseFiles(false);
            panel.setCanCreateDirectories(true);
            panel.setPrompt(Some(&NSString::from_str("选择")));
            panel.setMessage(Some(&NSString::from_str("截图和录屏会保存到这个位置")));
            if panel.runModal() == NSModalResponseOK {
                if let Some(url) = panel.URL() {
                    if let Some(path) = url.path() {
                        with_state(|s| {
                            s.settings.out_dir = std::path::PathBuf::from(path.to_string());
                            persist(&s.settings);
                        });
                        log(&format!("output dir -> {}", path));
                    }
                }
            }
        }

        #[unsafe(method(resetOutputDir:))]
        fn reset_output_dir(&self, _sender: &AnyObject) {
            with_state(|s| {
                s.settings.out_dir = dirs_desktop();
                persist(&s.settings);
            });
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
            permissions::show(self.mtm());
        }

        /// 状态行上的权限提示可以直接点：重启之后授权才对这个进程生效。
        #[unsafe(method(relaunch:))]
        fn relaunch_action(&self, _sender: &AnyObject) {
            self.relaunch();
        }

        #[unsafe(method(qualityChanged:))]
        fn quality_changed(&self, sender: &NSPopUpButton) {
            let index = sender.indexOfSelectedItem().max(0) as usize;
            let quality = Quality::all().get(index).copied().unwrap_or(Quality::Balanced);
            with_state(|s| {
                s.settings.quality = quality;
                persist(&s.settings);
            });
        }

        #[unsafe(method(fpsChanged:))]
        fn fps_changed(&self, sender: &NSPopUpButton) {
            let index = sender.indexOfSelectedItem();
            let fps = if index == 1 { 60 } else { 30 };
            with_state(|s| {
                s.settings.fps = fps;
                persist(&s.settings);
            });
        }

        #[unsafe(method(scaleChanged:))]
        fn scale_changed(&self, sender: &NSPopUpButton) {
            let index = sender.indexOfSelectedItem();
            with_state(|s| {
                s.settings.scale = if index == 1 { 2.0 } else { 1.0 };
                persist(&s.settings);
            });
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

        #[unsafe(method(toggleLine:))]
        fn toggle_line(&self, _sender: &AnyObject) {
            line::toggle_pinned();
        }

        #[unsafe(method(toggleLineHover:))]
        fn toggle_line_hover(&self, _sender: &AnyObject) {
            let hover = with_state(|s| {
                s.settings.line_hover = !s.settings.line_hover;
                persist(&s.settings);
                s.settings.line_hover
            });
            line::apply_settings(with_state(|s| s.settings.line), hover);
        }

        #[unsafe(method(toggleLineEnabled:))]
        fn toggle_line_enabled(&self, _sender: &AnyObject) {
            let (enabled, hover) = with_state(|s| {
                s.settings.line = !s.settings.line;
                persist(&s.settings);
                (s.settings.line, s.settings.line_hover)
            });
            line::apply_settings(enabled, hover);
        }

        #[unsafe(method(clearLine:))]
        fn clear_line(&self, _sender: &AnyObject) {
            line::clear();
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
        with_state(|s| load_persisted(&mut s.settings));

        let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
        if let Some(button) = item.button(mtm) {
            button.setTitle(ns_string!("REC"));
            let logo = status_icon();
            button.setImage(Some(&logo));
            self.ivars().status_logo.set(logo).ok();
        }
        item.setMenu(Some(&self.build_menu(mtm)));
        self.ivars().status_item.set(item).ok();

        self.apply_hotkeys();

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

    /// Preferences use native controls; capture actions stay in the menu/HUD.
    fn build_window(&self, mtm: MainThreadMarker) -> Retained<NSWindow> {
        let settings = with_state(|s| s.settings.clone());
        let width = 480.0;
        let height = 610.0;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setTitle(ns_string!("iRec 设置"));
        window.setSharingType(objc2_app_kit::NSWindowSharingType::None);
        let content = window.contentView().expect("settings content");
        let heading = |title: &str, y: f64| {
            let label = make_label(
                mtm,
                title,
                NSRect::new(NSPoint::new(24.0, y), NSSize::new(430.0, 22.0)),
                13.0,
            );
            label.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
            content.addSubview(&label);
        };
        heading("捕获", 566.0);
        let audio_titles: Vec<&str> = AudioSource::all().iter().map(|a| a.label()).collect();
        let quality_titles: Vec<&str> = Quality::all().iter().map(|q| q.label()).collect();
        let rows = [
            (
                "声音",
                audio_titles,
                sel!(audioChanged:),
                AudioSource::all()
                    .iter()
                    .position(|a| *a == settings.audio)
                    .unwrap_or(1),
            ),
            (
                "画质",
                quality_titles,
                sel!(qualityChanged:),
                Quality::all()
                    .iter()
                    .position(|q| *q == settings.quality)
                    .unwrap_or(1),
            ),
            (
                "帧率",
                vec!["30 fps · 省空间", "60 fps · 更流畅"],
                sel!(fpsChanged:),
                usize::from(settings.fps == 60),
            ),
            (
                "分辨率",
                vec!["1x · 标准", "2x · 高清"],
                sel!(scaleChanged:),
                usize::from(settings.scale > 1.5),
            ),
        ];
        for (i, (label, values, action, selected)) in rows.into_iter().enumerate() {
            let y = 528.0 - i as f64 * 34.0;
            content.addSubview(&make_label(
                mtm,
                label,
                NSRect::new(NSPoint::new(24.0, y + 3.0), NSSize::new(100.0, 20.0)),
                13.0,
            ));
            let popup = make_popup(
                mtm,
                &values,
                NSRect::new(NSPoint::new(148.0, y), NSSize::new(308.0, 26.0)),
                action,
                self,
                selected as isize,
            );
            content.addSubview(&popup);
            if i == 0 {
                self.ivars().audio_popup.set(popup).ok();
            }
        }
        let out_dir = make_button(
            mtm,
            "保存位置…",
            sel!(chooseOutputDir:),
            NSRect::new(NSPoint::new(20.0, 383.0), NSSize::new(440.0, 30.0)),
            self,
        );
        content.addSubview(&out_dir);
        self.ivars().out_dir_button.set(out_dir).ok();
        heading("晾衣绳与截图", 345.0);
        for (id, label, action, checked, y) in [
            (
                0,
                "将截图和录屏挂到晾衣绳",
                sel!(toggleLineEnabled:),
                settings.line,
                316.0,
            ),
            (
                1,
                "悬停在 iRec 图标时展开",
                sel!(toggleLineHover:),
                settings.line_hover,
                288.0,
            ),
            (
                3,
                "自动检查更新",
                sel!(toggleUpdateCheck:),
                settings.update_check,
                260.0,
            ),
        ] {
            let button = make_button(
                mtm,
                label,
                action,
                NSRect::new(NSPoint::new(24.0, y), NSSize::new(360.0, 24.0)),
                self,
            );
            button.setButtonType(NSButtonType::Switch);
            button.setState(state_value(checked));
            content.addSubview(&button);
            self.ivars().setting_checks.borrow_mut().push((id, button));
        }
        let clear = make_button(
            mtm,
            "移除全部卡片（保留文件）",
            sel!(clearLine:),
            NSRect::new(NSPoint::new(20.0, 222.0), NSSize::new(260.0, 28.0)),
            self,
        );
        content.addSubview(&clear);
        heading(&format!("通用 · iRec {}", env!("CARGO_PKG_VERSION")), 200.0);
        for (i, (label, action)) in [
            ("快捷键…", sel!(openHotkeys:)),
            ("权限设置…", sel!(checkPermission:)),
            ("检查更新…", sel!(checkUpdate:)),
        ]
        .into_iter()
        .enumerate()
        {
            content.addSubview(&make_button(
                mtm,
                label,
                action,
                NSRect::new(
                    NSPoint::new(20.0 + i as f64 * 148.0, 124.0),
                    NSSize::new(144.0, 30.0),
                ),
                self,
            ));
        }
        let status = section_label(
            mtm,
            "",
            NSRect::new(NSPoint::new(24.0, 50.0), NSSize::new(432.0, 60.0)),
        );
        status.setMaximumNumberOfLines(3);
        content.addSubview(&status);
        self.ivars().status_label.set(status).ok();
        window.center();
        window
    }

    /// Frequent actions stay at one level; preferences live in their own window.
    fn build_menu(&self, mtm: MainThreadMarker) -> Retained<NSMenu> {
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        for (label, action, symbol, slot) in [
            (
                "截取区域…",
                sel!(shotRegion:),
                "viewfinder",
                &self.ivars().shot_region_item,
            ),
            (
                "截取全屏",
                sel!(shotFull:),
                "display",
                &self.ivars().shot_full_item,
            ),
            (
                "滚动截图…",
                sel!(scrollShot:),
                "rectangle.portrait",
                &self.ivars().scroll_shot_item,
            ),
            (
                "完成滚动截图",
                sel!(finishScrollShot:),
                "checkmark",
                &self.ivars().finish_scroll_shot_item,
            ),
            (
                "取消滚动截图",
                sel!(cancelScrollShot:),
                "xmark",
                &self.ivars().cancel_scroll_shot_item,
            ),
        ] {
            let item = menu_item(mtm, label, Some(action), self, 0);
            item.setImage(
                NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(symbol),
                    None,
                )
                .as_deref(),
            );
            item.setHidden(label.starts_with("完成") || label.starts_with("取消"));
            menu.addItem(&item);
            slot.set(item).ok();
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        for (label, action, symbol, slot) in [
            (
                "录制全屏",
                sel!(startFull:),
                "record.circle",
                &self.ivars().start_menu_item,
            ),
            (
                "录制区域…",
                sel!(pickRegion:),
                "viewfinder",
                &self.ivars().region_menu_item,
            ),
        ] {
            let item = menu_item(mtm, label, Some(action), self, 0);
            item.setImage(
                NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(symbol),
                    None,
                )
                .as_deref(),
            );
            menu.addItem(&item);
            slot.set(item).ok();
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let line = menu_item(mtm, "显示晾衣绳", Some(sel!(toggleLine:)), self, 0);
        menu.addItem(&line);
        self.ivars().line_toggle_item.set(line).ok();
        menu.addItem(&menu_item(
            mtm,
            "清空晾衣绳",
            Some(sel!(clearLine:)),
            self,
            0,
        ));
        let settings = menu_item(mtm, "设置…", Some(sel!(showWindow:)), self, 0);
        settings.setKeyEquivalent(ns_string!(","));
        menu.addItem(&settings);
        let update = menu_item(mtm, "发现新版本", Some(sel!(installUpdate:)), self, 0);
        update.setHidden(true);
        menu.addItem(&update);
        self.ivars().update_item.set(update).ok();
        let status = menu_item(mtm, "", None, self, 0);
        status.setHidden(true);
        menu.addItem(&status);
        self.ivars().status_line.set(status).ok();
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let quit = menu_item(mtm, "退出 iRec", Some(sel!(quit:)), self, 0);
        quit.setKeyEquivalent(ns_string!("q"));
        menu.addItem(&quit);
        menu
    }

    fn is_update_action(action: &Action) -> bool {
        matches!(
            action,
            Action::CheckUpdate
                | Action::UpdateFound(..)
                | Action::UpdateAlert(_)
                | Action::UpdateReady(_)
        )
    }

    fn handle(&self, action: Action) {
        let (recording, stopping) = with_state(|s| (s.recorder.is_some(), s.stopping));
        // Still closing the previous file; ignore new commands for now.
        // 更新跟录制无关，不受这条限制。
        if stopping && !Self::is_update_action(&action) {
            return;
        }
        if !permissions::prepare_capture(self.mtm(), &action, recording) {
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
                    pick_region(self.mtm(), false);
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
            Action::ShotFull => {
                self.screenshot(None);
            }
            Action::PickShot => match hud::current_rect() {
                // 选区框还在屏幕上：直接截它，不用再框一次。
                Some(rect) => self.screenshot(Some(rect)),
                None => {
                    hud::hide();
                    pick_region(self.mtm(), true);
                }
            },
            Action::ShotRegion(rect) => {
                self.screenshot(Some(rect));
            }
            Action::ShotRegionAnnotated(rect) => {
                self.screenshot_with_markup(Some(rect), true);
            }
            Action::PickScrollShot => match scrollshot::frontmost_target_pid() {
                Ok(target_pid) => {
                    with_state(|s| {
                        s.last_error = None;
                        s.scroll_shot_status =
                            Some("请框选页面内容；开始后 iRec 会临时切换到目标应用".to_string());
                    });
                    overlay::pick_scroll_region(self.mtm(), target_pid);
                }
                Err(err) => with_state(|s| s.last_error = Some(err)),
            },
            Action::ScrollShotRegion(rect, target_pid) => {
                with_state(|s| s.scroll_shot_status = Some("正在切换到目标应用…".to_string()));
                let (scale, out_dir) =
                    with_state(|s| (s.settings.scale, s.settings.out_dir.clone()));
                scrollpreview::show(self.mtm(), rect);
                if let Err(error) = hotkey::register_scroll_cancel() {
                    log(&format!("scroll shot: Esc 无法注册：{error}"));
                }
                if scrollshot::activate_pid(self.mtm(), target_pid) {
                    scrollshot::start(rect, target_pid, scale, out_dir);
                } else {
                    hotkey::unregister_scroll_cancel();
                    scrollpreview::close(self.mtm());
                    with_state(|s| {
                        s.scroll_shot_status = None;
                        s.last_error = Some("目标应用已退出，请重新开始滚动截屏".to_string());
                    });
                }
            }
            Action::CancelScrollShot => {
                if scrollshot::crop_is_ready() {
                    self.handle(Action::DiscardScrollShotCrop);
                } else {
                    with_state(|s| {
                        s.scroll_shot_status = Some("正在取消并恢复原位置…".to_string())
                    });
                    scrollshot::activate_target(self.mtm());
                    scrollshot::request_cancel();
                }
            }
            Action::FinishScrollShot => {
                if scrollshot::is_active() {
                    with_state(|s| s.scroll_shot_status = Some("正在完成长图…".to_string()));
                    scrollshot::activate_target(self.mtm());
                    scrollshot::request_finish();
                } else if scrollshot::crop_is_ready() {
                    self.handle(Action::ConfirmScrollShotCrop);
                }
            }
            Action::ScrollShotProgress(message) => {
                with_state(|s| s.scroll_shot_status = Some(message));
            }
            Action::ScrollShotSelectionCancelled => {
                with_state(|s| s.scroll_shot_status = None);
            }
            Action::ScrollShotFinished(result) => {
                scrollshot::finish_on_main_thread(self.mtm());
                match result {
                    ScrollShotResult::ReadyForCrop(warning) => {
                        if scrollpreview::show_crop(self.mtm()) {
                            scrollshot::set_crop_ready(true);
                            with_state(|s| {
                                if let Some(warning) = warning.as_deref() {
                                    log(&format!("scroll shot partial result: {warning}"));
                                }
                                s.scroll_shot_status =
                                    Some(scroll_shot_crop_status(warning.as_deref()));
                            });
                        } else {
                            hotkey::unregister_scroll_cancel();
                            scrollpreview::close(self.mtm());
                            scrollshot::set_crop_ready(false);
                            with_state(|s| {
                                s.scroll_shot_status = None;
                                s.last_error = Some("无法打开长图裁剪预览，请重新截图".to_string());
                            });
                        }
                    }
                    ScrollShotResult::Cancelled => {
                        hotkey::unregister_scroll_cancel();
                        scrollpreview::close(self.mtm());
                        scrollshot::set_crop_ready(false);
                        with_state(|s| s.scroll_shot_status = None);
                        log("scroll shot cancelled; original position restored");
                    }
                    ScrollShotResult::Failed(err) => {
                        hotkey::unregister_scroll_cancel();
                        scrollpreview::close(self.mtm());
                        scrollshot::set_crop_ready(false);
                        with_state(|s| s.scroll_shot_status = None);
                        log(&format!("scroll shot menu error: {err}"));
                        with_state(|s| s.last_error = Some(err));
                    }
                }
            }
            Action::ConfirmScrollShotCrop => {
                if !scrollshot::crop_is_ready() {
                    return;
                }
                if scrollpreview::crop_is_saving() {
                    return;
                }
                match scrollpreview::cropped_rgba() {
                    Ok((width, height, rgba)) => {
                        scrollpreview::set_crop_saving(true);
                        with_state(|s| {
                            s.scroll_shot_status = Some("正在保存裁剪后的长图…".to_string())
                        });
                        let Some(out_dir) = scrollpreview::crop_output_dir() else {
                            scrollpreview::set_crop_saving(false);
                            with_state(|s| {
                                s.scroll_shot_status =
                                    Some("长图保存路径已失效，请重新截图".to_string())
                            });
                            return;
                        };
                        std::thread::spawn(move || {
                            let result = scrollshot::save_crop_png(width, height, &rgba, &out_dir);
                            push_action(Action::ScrollShotCropFinished(result));
                        });
                    }
                    Err(err) => {
                        with_state(|s| s.last_error = Some(err));
                    }
                }
            }
            Action::DiscardScrollShotCrop => {
                if scrollshot::crop_is_ready() && !scrollpreview::crop_is_saving() {
                    scrollshot::set_crop_ready(false);
                    hotkey::unregister_scroll_cancel();
                    scrollpreview::close(self.mtm());
                    with_state(|s| s.scroll_shot_status = None);
                }
            }
            Action::ScrollShotCropFinished(result) => match result {
                Ok((path, bytes)) => {
                    scrollshot::set_crop_ready(false);
                    hotkey::unregister_scroll_cancel();
                    scrollpreview::close(self.mtm());
                    with_state(|s| {
                        s.scroll_shot_status = None;
                        s.last_saved = Some((path.clone(), bytes));
                        s.saved_at = Some(Instant::now());
                        s.last_error = None;
                    });
                    line::hang_later(path.clone(), None);
                    push_action(Action::CopyImageToClipboard(path));
                }
                Err(err) => {
                    scrollpreview::set_crop_saving(false);
                    log(&format!("scroll shot save failed: {err}"));
                    with_state(|s| {
                        s.scroll_shot_status = None;
                        s.last_error = Some(err);
                    });
                }
            },
            Action::Markup(path) => {
                crate::markup::edit(path, |path| {
                    line::refresh(path.clone());
                    push_action(Action::CopyImageToClipboard(path));
                });
            }
            Action::CopyImageToClipboard(path) => {
                copy_capture_to_clipboard(&path, true);
            }
            Action::CopyFileToClipboard(path) => {
                copy_capture_to_clipboard(&path, false);
            }
            Action::SetHotkey { slot, key } => {
                with_state(|s| {
                    s.settings.hotkeys.set(slot, key);
                    // 同一个组合只留一个：原来那个位置清空。
                    s.settings.hotkeys.clear_conflicts(slot, key);
                    persist(&s.settings);
                });
                log(&format!(
                    "hotkey: {} → {}",
                    Hotkeys::label(slot),
                    key.display()
                ));
                self.apply_hotkeys();
            }
            Action::CheckUpdate
            | Action::UpdateFound(..)
            | Action::UpdateAlert(_)
            | Action::UpdateReady(_) => self.handle_update(action),
        }
    }

    /// 启动几秒后静默查一次，之后每 24 小时一次。
    fn maybe_auto_check_update(&self) {
        let (enabled, checked, elapsed) = with_state(|s| {
            (
                s.settings.update_check,
                s.update_checked_at,
                s.launched_at.elapsed(),
            )
        });
        if !enabled {
            return;
        }
        let due = match checked {
            // 刚启动先缓一缓，别和启动抢资源。
            None => elapsed > Duration::from_secs(5),
            Some(at) => at.elapsed() > Duration::from_secs(24 * 60 * 60),
        };
        if due {
            self.check_update(false);
        }
    }

    /// 问 GitHub 有没有新版本。`manual=false` 时只在菜单栏那一行提示，不弹窗打扰。
    fn check_update(&self, manual: bool) {
        if with_state(|s| s.update.as_ref().is_some_and(|u| u.busy)) {
            return;
        }
        with_state(|s| {
            s.update = Some(Update {
                tag: None,
                message: Some("正在检查更新…".to_string()),
                busy: true,
            });
            s.update_checked_at = Some(Instant::now());
        });
        std::thread::spawn(move || {
            let done = |s: &mut AppState| {
                if let Some(update) = s.update.as_mut() {
                    update.busy = false;
                }
            };
            match update::latest_tag() {
                Ok(tag) => {
                    let newer = update::is_newer(&tag, update::current_version());
                    log(&format!(
                        "update: latest={tag}, current={}, newer={newer}",
                        update::current_version()
                    ));
                    with_state(done);
                    if newer {
                        push_action(Action::UpdateFound(tag, manual));
                    } else if manual {
                        push_action(Action::UpdateAlert(format!(
                            "已经是最新版本（{}）。",
                            update::current_version()
                        )));
                    } else {
                        with_state(|s| s.update = None);
                    }
                }
                Err(err) => {
                    log(&format!("update: 检查失败 {err}"));
                    with_state(done);
                    if manual {
                        push_action(Action::UpdateAlert(format!("检查更新失败：{err}")));
                    } else {
                        with_state(|s| s.update = None);
                    }
                }
            }
        });
    }

    /// 下载新版本，下载完交给主线程换包（换包要在主线程退出自己）。
    fn install_update(&self) {
        let tag = with_state(|s| s.update.as_ref().and_then(|u| u.tag.clone()));
        let Some(tag) = tag else { return };
        if with_state(|s| s.update.as_ref().is_some_and(|u| u.busy)) {
            return;
        }
        let version = update::strip_tag(&tag);
        log(&format!("update: 开始下载 {version}"));
        with_state(|s| {
            s.update = Some(Update {
                tag: Some(tag.clone()),
                message: Some("正在下载更新…".to_string()),
                busy: true,
            });
        });
        std::thread::spawn(move || match update::download(&version) {
            Ok(path) => {
                with_state(|s| {
                    if let Some(update) = s.update.as_mut() {
                        update.message = Some("正在安装更新…".to_string());
                    }
                });
                push_action(Action::UpdateReady(path));
            }
            Err(err) => {
                log(&format!("update: 下载失败 {err}"));
                push_action(Action::UpdateAlert(format!("下载更新失败：{err}")));
            }
        });
    }

    fn handle_update(&self, action: Action) {
        match action {
            Action::CheckUpdate => self.check_update(true),
            Action::UpdateFound(tag, manual) => {
                log(&format!("update: 发现新版本 {tag}（manual={manual}）"));
                with_state(|s| {
                    s.update = Some(Update {
                        tag: Some(tag.clone()),
                        message: None,
                        busy: false,
                    });
                });
                if manual {
                    let version = update::strip_tag(&tag);
                    self.show_update_alert(
                        &format!(
                            "新版本 {version} 已发布，当前是 {}。\n                             更新会下载对应芯片的包，换掉 iRec.app 后重新打开。",
                            update::current_version()
                        ),
                        true,
                    );
                }
            }
            Action::UpdateAlert(text) => {
                with_state(|s| s.update = None);
                self.show_update_alert(&text, false);
            }
            Action::UpdateReady(path) => match update::replace_and_relaunch(&path) {
                Ok(()) => {
                    log("update: 换包脚本已就位，退出等它接管");
                    self.stop_sync_if_recording();
                    NSApplication::sharedApplication(self.mtm()).terminate(None);
                }
                Err(err) => {
                    log(&format!("update: 换包失败 {err}"));
                    push_action(Action::UpdateAlert(err));
                }
            },
            _ => {}
        }
    }

    /// 更新相关的弹窗。install=true 时第一个按钮是「立即更新」。
    fn show_update_alert(&self, message: &str, install: bool) {
        let alert = unsafe { NSAlert::new(self.mtm()) };
        alert.setMessageText(&NSString::from_str(if install {
            "发现新版本"
        } else {
            "检查更新"
        }));
        let text = if install {
            format!("{message}\n\n发布包是 ad-hoc 签名，换包后屏幕录制权限可能要重新授权一次。")
        } else {
            message.to_string()
        };
        alert.setInformativeText(&NSString::from_str(&text));
        if install {
            alert.addButtonWithTitle(&NSString::from_str("立即更新"));
            alert.addButtonWithTitle(&NSString::from_str("以后再说"));
        } else {
            alert.addButtonWithTitle(&NSString::from_str("好"));
        }
        let choice = alert.runModal();
        if install && choice == NSAlertFirstButtonReturn {
            self.install_update();
        }
    }

    /// 按当前设置重新注册热键，并把设置窗口里的显示刷新一遍。
    fn apply_hotkeys(&self) {
        let hotkeys = with_state(|s| s.settings.hotkeys);
        log(&format!(
            "hotkey: {} = {}, {} = {}, {} = {}, {} = {}",
            Hotkeys::label(hotkey::SLOT_RECORD_FULL),
            hotkeys.record_full.display(),
            Hotkeys::label(hotkey::SLOT_RECORD_REGION),
            hotkeys.record_region.display(),
            Hotkeys::label(hotkey::SLOT_SHOT_FULL),
            hotkeys.shot_full.display(),
            Hotkeys::label(hotkey::SLOT_SHOT_REGION),
            hotkeys.shot_region.display(),
        ));
        if let Err(err) = hotkey::apply(&hotkeys) {
            log(&format!("hotkey: 注册失败 {err}"));
            with_state(|s| s.last_error = Some(format!("快捷键注册失败：{err}")));
        }
        self.refresh_hotkey_window();
    }

    fn refresh_hotkey_window(&self) {
        let hotkeys = with_state(|s| s.settings.hotkeys);
        let fields = self.ivars().hotkey_fields.borrow();
        for (index, slot) in SLOTS.iter().enumerate() {
            if let Some(field) = fields.get(index) {
                field.setStringValue(&NSString::from_str(&hotkeys.get(*slot).display()));
            }
        }
    }

    /// 打开快捷键设置窗口（菜单栏「快捷键设置…」）。
    fn show_hotkey_window(&self) {
        let mtm = self.mtm();
        let window = match self.ivars().hotkey_window.get() {
            Some(window) => window.clone(),
            None => {
                let window = self.build_hotkey_window(mtm);
                self.ivars().hotkey_window.set(window.clone()).ok();
                window
            }
        };
        self.refresh_hotkey_window();
        window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    }

    fn build_hotkey_window(&self, mtm: MainThreadMarker) -> Retained<NSWindow> {
        let width = 360.0;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, 236.0));
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setSharingType(objc2_app_kit::NSWindowSharingType(0));
        window.setTitle(ns_string!("快捷键"));
        let content = window.contentView().expect("content view");

        let mut y = 180.0;
        let title = make_label(
            mtm,
            "四个动作的全局快捷键",
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(width - 40.0, 22.0)),
            14.0,
        );
        content.addSubview(&title);

        for slot in SLOTS.iter() {
            y -= 34.0;
            let label = make_label(
                mtm,
                Hotkeys::label(*slot),
                NSRect::new(NSPoint::new(20.0, y), NSSize::new(120.0, 22.0)),
                13.0,
            );
            content.addSubview(&label);
            let field = make_label(
                mtm,
                "",
                NSRect::new(NSPoint::new(146.0, y), NSSize::new(110.0, 22.0)),
                13.0,
            );
            content.addSubview(&field);
            self.ivars().hotkey_fields.borrow_mut().push(field);
            let button = make_button(
                mtm,
                "更改",
                sel!(captureHotkey:),
                NSRect::new(NSPoint::new(262.0, y), NSSize::new(78.0, 26.0)),
                self,
            );
            button.setTag(*slot as isize);
            content.addSubview(&button);
        }

        y -= 40.0;
        let restore = make_button(
            mtm,
            "恢复默认",
            sel!(restoreHotkeys:),
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(120.0, 30.0)),
            self,
        );
        content.addSubview(&restore);
        let hint = make_label(
            mtm,
            "新快捷键要带 ⌘ / ⌥ / ⌃",
            NSRect::new(NSPoint::new(150.0, y + 5.0), NSSize::new(190.0, 20.0)),
            11.0,
        );
        content.addSubview(&hint);

        window.center();
        window
    }

    /// 弹一个小窗口等用户按组合键：Esc 取消，⌫ 清除。
    fn begin_capture(&self, slot: u32) {
        let mtm = self.mtm();
        *CAPTURE_SLOT.lock().unwrap_or_else(|e| e.into_inner()) = slot;
        *CAPTURE_ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = true;
        hotkey::unregister_all();
        let panel = match self.ivars().capture_panel.get() {
            Some(panel) => panel.clone(),
            None => {
                let panel = self.build_capture_panel(mtm);
                self.ivars().capture_panel.set(panel.clone()).ok();
                panel
            }
        };
        if let Some(view) = self.ivars().capture_view.get() {
            if let Some(hint) = view.ivars().hint.get() {
                hint.setStringValue(&NSString::from_str("按下新的组合键…\n⌫ 清除，Esc 取消"));
            }
        }
        panel.setTitle(&NSString::from_str(&format!(
            "{} 的新快捷键",
            Hotkeys::label(slot)
        )));
        panel.makeKeyAndOrderFront(None);
        let responder = match self.ivars().capture_view.get() {
            Some(view) => panel.makeFirstResponder(Some(&**view)),
            None => false,
        };
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        log(&format!(
            "capture: slot={} ({}) firstResponder={responder}",
            slot,
            Hotkeys::label(slot)
        ));
    }

    fn build_capture_panel(&self, mtm: MainThreadMarker) -> Retained<NSWindow> {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(340.0, 150.0));
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setSharingType(objc2_app_kit::NSWindowSharingType(0));
        window.setTitle(ns_string!("按下新的快捷键"));

        let view = CaptureView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(340.0, 150.0)),
        );
        let hint = make_label(
            mtm,
            "按下新的组合键…\n⌫ 清除，Esc 取消",
            NSRect::new(NSPoint::new(20.0, 40.0), NSSize::new(300.0, 60.0)),
            14.0,
        );
        view.addSubview(&hint);
        view.ivars().hint.set(hint).ok();
        window.setContentView(Some(&view));
        self.ivars().capture_view.set(view).ok();

        window.center();
        window
    }

    /// 权限没生效时的引导。
    fn show_permission_alert(&self) {
        permissions::show(self.mtm());
    }

    /// 退出并重新打开自己：授权改动之后就差这一步。
    fn relaunch(&self) {
        relaunch_app(self.mtm());
    }

    fn start(&self, rect: Option<Rect>) {
        let settings = with_state(|s| s.settings.clone());
        match Recorder::start(rect, &settings) {
            Ok(recorder) => with_state(|s| {
                s.recorder = Some(recorder);
                s.started_at = Some(Instant::now());
                s.region = rect;
                s.last_error = None;
                s.permission_blocked = false;
            }),
            Err(err) => {
                eprintln!("irec: {err}");
                // 权限没生效是最常见的失败原因，单独给一条能照着做的提示。
                let blocked = !unsafe { CGPreflightScreenCaptureAccess() };
                log(&format!(
                    "start failed: {err} (permission blocked={blocked})"
                ));
                with_state(|s| {
                    s.last_error = Some(err);
                    s.permission_blocked = blocked;
                });
                if blocked {
                    self.show_permission_alert();
                }
            }
        }
        // 录制时把绳子收起来：挡视线，窗口本身也不会被录进去。
        line::set_busy(with_state(|s| s.recorder.is_some()));
    }

    /// Stops the recording in the background so the main run loop keeps
    /// running while AVAssetWriter closes the file.
    fn finish(&self) {
        let recorder = with_state(|s| s.recorder.take());
        let Some(recorder) = recorder else { return };
        let region = with_state(|s| s.region.take());
        with_state(|s| s.stopping = true);
        std::thread::spawn(move || {
            let result = recorder.stop();
            let clipboard_path = result.as_ref().ok().map(|(path, _, _)| path.clone());
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
                        s.last_saved = Some((path.clone(), bytes));
                        s.saved_at = Some(Instant::now());
                        s.last_error = None;
                        // 缩略图和挂上去都在后台，主线程只负责画。
                        if s.settings.line {
                            line::hang_later(path.clone(), region);
                        }
                    }
                    Err(err) => {
                        log(&format!("stop failed: {err}"));
                        s.last_error = Some(err);
                    }
                }
            });
            if let Some(path) = clipboard_path {
                push_action(Action::CopyFileToClipboard(path));
            }
            line::set_busy(false);
        });
    }

    /// 最近一张截图（用于「标注最新截图…」）。
    fn latest_shot(&self) -> Option<PathBuf> {
        let saved = with_state(|s| s.last_saved.as_ref().map(|(p, _)| p.clone()));
        if let Some(path) = saved {
            if is_image(&path) {
                return Some(path);
            }
        }
        let dir = with_state(|s| s.settings.out_dir.clone());
        let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !is_image(&path) || !path.to_string_lossy().contains("iRec-shot-") {
                    continue;
                }
                let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
                let modified = modified.unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().map(|(_, t)| modified > *t).unwrap_or(true) {
                    best = Some((path, modified));
                }
            }
        }
        best.map(|(path, _)| path)
    }

    /// 截图：抓一帧存成 PNG，然后挂到晾衣绳上。
    fn screenshot(&self, region: Option<Rect>) {
        self.screenshot_with_markup(region, false);
    }

    fn screenshot_with_markup(&self, region: Option<Rect>, markup: bool) {
        let settings = with_state(|s| s.settings.clone());
        crate::shot::capture(
            region,
            settings.scale,
            &settings.out_dir,
            true,
            move |result| match result {
                Ok((path, bytes)) => {
                    with_state(|s| {
                        s.last_saved = Some((path.clone(), bytes));
                        s.saved_at = Some(Instant::now());
                        s.last_error = None;
                    });
                    push_action(Action::CopyImageToClipboard(path.clone()));
                    // 标注要在主线程开，交给定时器那一轮。
                    if markup {
                        push_action(Action::Markup(path));
                    }
                }
                Err(err) => {
                    eprintln!("irec: {err}");
                    with_state(|s| s.last_error = Some(err));
                }
            },
        );
    }

    /// Used when quitting: block until the file is closed.
    fn stop_sync_if_recording(&self) {
        let recorder = with_state(|s| s.recorder.take());
        if let Some(recorder) = recorder {
            if let Ok((path, _, _)) = recorder.stop() {
                copy_capture_to_clipboard(&path, false);
            }
            with_state(|s| {
                s.stopping = false;
                s.started_at = None;
            });
        }
    }

    /// Mirrors the current state into the control window.
    fn update_window(&self) {
        let settings = with_state(|s| s.settings.clone());
        for (id, button) in self.ivars().setting_checks.borrow().iter() {
            button.setState(state_value(match id {
                0 => settings.line,
                1 => settings.line_hover,
                _ => settings.update_check,
            }));
            if *id == 1 {
                button.setEnabled(settings.line);
            }
        }
        let (label, audio, out_dir) = with_state(|s| {
            (
                window_status(s),
                s.settings.audio,
                s.settings.out_dir.clone(),
            )
        });
        if let Some(field) = self.ivars().status_label.get() {
            field.setStringValue(&NSString::from_str(&label));
        }
        if let Some(button) = self.ivars().out_dir_button.get() {
            button.setTitle(&NSString::from_str(&format!(
                "保存位置：{}（点击更改）",
                short_path(&out_dir)
            )));
        }
        if let Some(popup) = self.ivars().audio_popup.get() {
            let index = AudioSource::all()
                .iter()
                .position(|a| *a == audio)
                .unwrap_or(1);
            popup.selectItemAtIndex(index as isize);
        }
    }

    fn refresh_status(&self) {
        let mtm = self.mtm();
        let Some(item) = self.ivars().status_item.get() else {
            return;
        };
        let Some(button) = item.button(mtm) else {
            return;
        };

        let title = with_state(|s| status_title(s));
        button.setTitle(&NSString::from_str(&title));
        if let Some(logo) = self.ivars().status_logo.get() {
            button.setImage(Some(logo));
        }
        button.setToolTip(Some(ns_string!("iRec · 截图与录屏")));
        if let Some(window) = button.window() {
            line::set_anchor(
                window.convertRectToScreen(button.convertRect_toView(button.bounds(), None)),
            );
        }
    }

    fn refresh_menu(&self) {
        let settings = with_state(|s| s.settings.clone());
        let (recording, stopping) = with_state(|s| (s.recorder.is_some(), s.stopping));
        let scroll_status = with_state(|s| s.scroll_shot_status.clone());
        let scrolling = scrollshot::is_active();
        let crop_ready = scrollshot::crop_is_ready();
        let crop_saving = scrollpreview::crop_is_saving();
        if let Some(item) = self.ivars().scroll_shot_item.get() {
            item.setHidden(scrolling);
            item.setEnabled(!recording && !stopping && !scrolling && scroll_status.is_none());
        }
        if let Some(item) = self.ivars().finish_scroll_shot_item.get() {
            item.setTitle(&NSString::from_str(if crop_ready {
                "保存裁剪"
            } else {
                "完成滚动截图"
            }));
            item.setHidden(!scrolling && !crop_ready);
            item.setEnabled(if crop_ready {
                !crop_saving
            } else {
                scrolling
                    && !scrollshot::cancel_requested()
                    && scroll_status.as_deref() != Some("正在完成长图…")
            });
        }
        if let Some(item) = self.ivars().cancel_scroll_shot_item.get() {
            item.setTitle(&NSString::from_str(if crop_ready {
                "放弃长图"
            } else {
                "取消滚动截图"
            }));
            item.setHidden(!scrolling && !crop_ready);
            item.setEnabled(if crop_ready { !crop_saving } else { scrolling });
        }
        let (enabled, _, revealed) = line::status();
        if let Some(item) = self.ivars().line_toggle_item.get() {
            item.setTitle(&NSString::from_str(if revealed {
                "收起晾衣绳"
            } else {
                "显示晾衣绳"
            }));
            item.setEnabled(enabled);
        }
        if let Some(item) = self.ivars().update_item.get() {
            let view = with_state(|s| {
                s.update.as_ref().map(|u| {
                    let title = if u.busy {
                        u.message
                            .clone()
                            .unwrap_or_else(|| "正在检查更新…".to_string())
                    } else if let Some(tag) = &u.tag {
                        format!("↑ 发现新版本 {tag}，点此更新")
                    } else {
                        u.message.clone().unwrap_or_default()
                    };
                    // 忙的时候不给点，免得重复触发。
                    (title, u.tag.is_some() && !u.busy)
                })
            });
            match view {
                Some((title, actionable)) => {
                    item.setTitle(&NSString::from_str(&title));
                    item.setHidden(!actionable);
                    item.setEnabled(actionable);
                }
                None => item.setHidden(true),
            }
        }
        let hotkeys = settings.hotkeys;
        let elapsed = with_state(|s| {
            s.started_at
                .map(|t| {
                    let secs = t.elapsed().as_secs();
                    format!("{:02}:{:02}", secs / 60, secs % 60)
                })
                .unwrap_or_default()
        });
        let start_title = if stopping {
            "正在保存…".to_string()
        } else if recording {
            format!("停止录制 · {elapsed}")
        } else {
            "录制全屏".to_string()
        };
        let named = [
            (
                self.ivars().start_menu_item.get(),
                start_title.as_str(),
                hotkeys.record_full,
            ),
            (
                self.ivars().region_menu_item.get(),
                "录制区域…",
                hotkeys.record_region,
            ),
            (
                self.ivars().shot_full_item.get(),
                "截取全屏",
                hotkeys.shot_full,
            ),
            (
                self.ivars().shot_region_item.get(),
                "截取区域…",
                hotkeys.shot_region,
            ),
        ];
        for (item, name, key) in named {
            if let Some(item) = item {
                item.setTitle(&NSString::from_str(name));
                key.apply_to_menu(item);
            }
        }
        if let Some(item) = self.ivars().start_menu_item.get() {
            item.setEnabled(!stopping && !scrolling && scroll_status.is_none());
        }
        if let Some(item) = self.ivars().region_menu_item.get() {
            item.setHidden(recording || stopping);
            item.setEnabled(!scrolling && scroll_status.is_none());
        }
        for item in [
            self.ivars().shot_full_item.get(),
            self.ivars().shot_region_item.get(),
        ]
        .into_iter()
        .flatten()
        {
            item.setEnabled(!recording && !stopping && !scrolling && scroll_status.is_none());
        }
        if let Some(item) = self.ivars().status_line.get() {
            let blocked = with_state(|s| s.permission_blocked);
            if blocked {
                let text = if unsafe { CGPreflightScreenCaptureAccess() } {
                    "✓ 权限已开启，重启 iRec 后生效（点此重启）"
                } else {
                    "⚠ 屏幕录制权限未生效，点此重启 iRec"
                };
                item.setTitle(&NSString::from_str(text));
                item.setHidden(false);
                unsafe { item.setAction(Some(sel!(relaunch:))) };
                unsafe { item.setTarget(Some(self)) };
            }
            let text = with_state(|s| {
                if s.permission_blocked {
                    None
                } else if let Some(status) = &s.scroll_shot_status {
                    Some(menu_status_text(status))
                } else if let Some(err) = &s.last_error {
                    Some(menu_status_text(&format!("⚠ {err}")))
                } else {
                    None
                }
            });
            match text {
                Some(text) => {
                    item.setTitle(&NSString::from_str(&text));
                    item.setHidden(false);
                    unsafe { item.setAction(None) };
                    unsafe { item.setTarget(None) };
                }
                None => {
                    if !blocked {
                        item.setHidden(true);
                    }
                }
            }
        }
    }
}

fn menu_status_text(text: &str) -> String {
    if text.contains("画面重叠无法可靠识别") {
        return "⚠ 长图拼接失败，请换选区重试".to_string();
    }
    const MAX_CHARS: usize = 42;
    let mut chars = text.chars();
    let short: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

fn scroll_shot_crop_status(warning: Option<&str>) -> String {
    match warning {
        Some(warning) => format!("{warning}；拖动边界裁剪保存"),
        None => "拖动上下边界裁剪，保存后加入晾衣绳".to_string(),
    }
}

#[cfg(test)]
mod menu_status_tests {
    use super::{menu_status_text, scroll_shot_crop_status};

    #[test]
    fn long_stitch_error_is_shortened_for_the_menu() {
        let text = "⚠ 画面重叠无法可靠识别，已停止拼接；请框选稳定的页面内容后重试（已重试一次；请重新框选更大的页面区域，避开固定工具栏和动画内容）";
        let short = menu_status_text(text);
        assert_eq!(short, "⚠ 长图拼接失败，请换选区重试");
        assert!(short.chars().count() < 30);
    }

    #[test]
    fn other_long_menu_messages_are_bounded() {
        let short = menu_status_text(&"错误".repeat(100));
        assert!(short.chars().count() <= 43);
        assert!(short.ends_with('…'));
    }

    #[test]
    fn partial_scroll_shot_is_presented_as_a_usable_crop_result() {
        let status = scroll_shot_crop_status(Some("拼接暂停，已保留 2 屏"));
        assert_eq!(status, "拼接暂停，已保留 2 屏；拖动边界裁剪保存");
        assert!(!status.contains("失败"));
        assert!(status.contains("裁剪保存"));
    }

    #[test]
    fn page_without_scroll_is_presented_as_a_crop_action() {
        let status = scroll_shot_crop_status(None);
        assert_eq!(status, "拖动上下边界裁剪，保存后加入晾衣绳");
        assert!(!status.contains("失败"));
        assert!(!status.contains("错误"));
    }

}

/// 正在等用户按组合键的槽位（CaptureView 靠它知道要改哪一个）。
static CAPTURE_SLOT: Mutex<u32> = Mutex::new(1);
/// 正在等按键：这期间先摘掉全局热键，免得按 ⇧⌘R 顺手就开始录制了。
static CAPTURE_ACTIVE: Mutex<bool> = Mutex::new(false);

#[derive(Default)]
struct CaptureIvars {
    hint: OnceCell<Retained<NSTextField>>,
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = CaptureIvars]
    struct CaptureView;

    unsafe impl NSObjectProtocol for CaptureView {}

    impl CaptureView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let slot = *CAPTURE_SLOT.lock().unwrap_or_else(|e| e.into_inner());
            let Some(window) = self.window() else { return };
            let code = event.keyCode();
            if code == KVK_ESCAPE {
                window.close();
                return;
            }
            if code == KVK_DELETE {
                push_action(Action::SetHotkey {
                    slot,
                    key: Hotkey::none(),
                });
                window.close();
                return;
            }
            let key = Hotkey {
                key_code: code as u32,
                modifiers: Hotkey::event_modifiers(event.modifierFlags()),
            };
            if !key.valid() {
                if let Some(hint) = self.ivars().hint.get() {
                    hint.setStringValue(&NSString::from_str("要带上 ⌘ / ⌥ / ⌃ 之一，再按一次"));
                }
                return;
            }
            push_action(Action::SetHotkey { slot, key });
            window.close();
        }
    }
);

impl CaptureView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CaptureIvars::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// A monochrome rendition of the product's screen, capture brackets and record dot.
/// AppKit tints the template to match light, dark and highlighted menu bars.
fn status_icon() -> Retained<NSImage> {
    let drawing = RcBlock::new(|_: NSRect| -> objc2::runtime::Bool {
        let screen = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
            NSRect::new(NSPoint::new(2.0, 4.0), NSSize::new(16.0, 12.0)),
            2.0,
            2.0,
        );
        NSColor::blackColor()
            .colorWithAlphaComponent(0.35)
            .setStroke();
        screen.setLineWidth(0.8);
        screen.stroke();

        NSColor::blackColor().setStroke();
        let brackets = NSBezierPath::bezierPath();
        brackets.setLineWidth(1.5);
        brackets.setLineCapStyle(objc2_app_kit::NSLineCapStyle::Round);
        brackets.setLineJoinStyle(objc2_app_kit::NSLineJoinStyle::Round);
        for (x, sx) in [(3.5, 1.0), (16.5, -1.0)] {
            for (y, sy) in [(5.5, 1.0), (14.5, -1.0)] {
                brackets.moveToPoint(NSPoint::new(x, y + sy * 2.0));
                brackets.lineToPoint(NSPoint::new(x, y));
                brackets.lineToPoint(NSPoint::new(x + sx * 2.5, y));
            }
        }
        brackets.stroke();
        NSColor::blackColor().setFill();
        NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
            NSPoint::new(7.9, 7.9),
            NSSize::new(4.2, 4.2),
        ))
        .fill();
        objc2::runtime::Bool::YES
    });
    let image =
        NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(20.0, 20.0), false, &drawing);
    image.setTemplate(true);
    image
}

/// Asks for microphone access the first time a mic source is selected, so the
/// system prompt appears without waiting for the next recording.
fn request_mic_access() {
    #[allow(deprecated)]
    let media_type: Option<&objc2_foundation::NSString> = unsafe { AVMediaTypeAudio };
    let Some(media_type) = media_type else { return };
    let block: RcBlock<dyn Fn(objc2::runtime::Bool)> =
        RcBlock::new(|granted: objc2::runtime::Bool| {
            log(&format!("microphone access granted={}", granted.as_bool()));
        });
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &block) };
}

/// 把家目录缩写成 `~`，菜单里显示更紧凑。
fn short_path(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home).to_string_lossy().to_string();
        if let Some(rest) = text.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    text
}

fn make_label(
    mtm: MainThreadMarker,
    text: &str,
    frame: NSRect,
    size: f64,
) -> Retained<NSTextField> {
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

/// 分组小标题：小一号的次要色，把控件分成几块。
fn section_label(mtm: MainThreadMarker, text: &str, frame: NSRect) -> Retained<NSTextField> {
    let field = make_label(mtm, text, frame, 12.0);
    field.setTextColor(Some(&NSColor::secondaryLabelColor()));
    field
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
    let settings = menu_item(mtm, "设置…", Some(sel!(showWindow:)), target, 0);
    settings.setKeyEquivalent(ns_string!(","));
    app_menu.addItem(&settings);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&menu_item(
        mtm,
        "清空晾衣绳",
        Some(sel!(clearLine:)),
        target,
        0,
    ));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
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
    // The same capture menu is reachable while a utility window has focus.
    let capture = NSMenu::new(mtm);
    let capture_root = menu_item(mtm, "捕获", None, target, 0);
    for (label, action) in [
        ("截取区域…", sel!(shotRegion:)),
        ("截取全屏", sel!(shotFull:)),
        ("滚动截图…", sel!(scrollShot:)),
        ("录制全屏 / 停止", sel!(startFull:)),
        ("显示晾衣绳", sel!(toggleLine:)),
    ] {
        capture.addItem(&menu_item(mtm, label, Some(action), target, 0));
    }
    capture_root.setSubmenu(Some(&capture));
    main.addItem(&capture_root);
    std::mem::forget(app_menu);
    std::mem::forget(app_item);
    main
}

fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png") | Some("jpg") | Some("jpeg") | Some("tiff") | Some("heic")
    )
}

/// Publish the completed capture to the general pasteboard on the main thread.
fn copy_capture_to_clipboard(path: &Path, image: bool) {
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    let ok = if image {
        NSImage::initWithContentsOfFile(
            NSImage::alloc(),
            &NSString::from_str(&path.to_string_lossy()),
        )
        .is_some_and(|image| {
            let writer: &ProtocolObject<dyn NSPasteboardWriting> =
                ProtocolObject::from_ref(&*image);
            board.writeObjects(&NSArray::from_slice(&[writer]))
        })
    } else {
        let url =
            NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), false);
        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*url);
        board.writeObjects(&NSArray::from_slice(&[writer]))
    };
    if !ok {
        log(&format!("clipboard: 无法复制 {}", path.display()));
    }
}

/// 给 sh 用的单引号转义。
fn shell_quote(text: &str) -> String {
    text.replace('\'', "'\\''")
}

pub(crate) fn relaunch_app(mtm: MainThreadMarker) {
    log("relaunching");
    let bundle = NSBundle::mainBundle();
    let app = bundle.bundlePath().to_string();
    let exe = bundle.executablePath().map(|p| p.to_string());
    let command = if app.ends_with(".app") {
        format!("sleep 1; open -n '{}'", shell_quote(&app))
    } else if let Some(exe) = exe {
        format!("sleep 1; '{}' >/dev/null 2>&1 &", shell_quote(&exe))
    } else {
        return;
    };
    if std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .spawn()
        .is_err()
    {
        log("relaunch: 无法启动重启命令");
        return;
    }
    NSApplication::sharedApplication(mtm).terminate(None);
}

fn window_status(state: &AppState) -> String {
    if let Some(message) = state.update.as_ref().and_then(|u| u.message.as_ref()) {
        return message.clone();
    }
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
    format!(
        "就绪 · {} 录屏 / {} 截图",
        state.settings.hotkeys.record_full.display(),
        state.settings.hotkeys.shot_full.display()
    )
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
    if let Some(status) = &state.scroll_shot_status {
        if status.contains("取消") {
            return "⇅ 取消中".to_string();
        }
        if status.contains("完成") {
            return "⇅ 完成中".to_string();
        }
        return "⇅ 滚动截屏".to_string();
    }
    if let Some(err) = &state.last_error {
        if err.len() > 12 {
            return "⚠ 出错了".to_string();
        }
        return format!("⚠ {err}");
    }
    String::new()
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
    fn CGPreflightScreenCaptureAccess() -> bool;
}

fn main() {
    init_state();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        selftest(&args);
        return;
    }
    if args.iter().any(|a| a == "--shot") {
        shot_cli(&args);
        return;
    }
    if args.iter().any(|a| a == "--update") {
        update_cli();
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
/// 命令行截图：`irec --shot [--region x,y,w,h] [--scale 2] [--out ~/Desktop]`
/// （Rect 的原点在屏幕左下、y 朝上，和录制用的坐标系一致。）
fn shot_cli(args: &[String]) {
    let region = arg_value(args, "--region").and_then(|v| {
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
    let scale = arg_value(args, "--scale")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2.0);
    let out = arg_value(args, "--out")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| with_state(|s| s.settings.out_dir.clone()));

    let (tx, rx) = std::sync::mpsc::channel();
    crate::shot::capture(region, scale, &out, false, move |result| {
        let _ = tx.send(result);
    });
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok((path, bytes))) => println!(
            "OK {} ({:.2} MB)",
            path.display(),
            bytes as f64 / 1_048_576.0
        ),
        Ok(Err(err)) => {
            println!("FAILED: {err}");
            std::process::exit(1);
        }
        Err(_) => {
            println!("FAILED: 等截图结果超时");
            std::process::exit(1);
        }
    }
}

/// 命令行检查更新：`irec --update`
fn update_cli() {
    println!("当前版本：{}", update::current_version());
    match update::latest_tag() {
        Ok(tag) => {
            println!("最新版本：{tag}");
            if !update::is_newer(&tag, update::current_version()) {
                println!("已是最新，不用更新。");
                return;
            }
            let version = update::strip_tag(&tag);
            println!("下载 {}", update::asset_url(&version));
            match update::download(&version) {
                Ok(path) => match update::replace_and_relaunch(&path) {
                    Ok(()) => println!("已排好换包脚本：iRec 退出后自动换成 {tag} 并重新打开。"),
                    Err(err) => println!("FAILED: {err}\n手动下载：{}", update::releases_url()),
                },
                Err(err) => println!("FAILED: {err}"),
            }
        }
        Err(err) => println!("FAILED: {err}"),
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

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
        settings.audio = AudioSource::Off;
    }
    if let Some(name) = value(args, "--audio") {
        settings.audio = match name.as_str() {
            "off" | "none" => AudioSource::Off,
            "mic" | "microphone" => AudioSource::Mic,
            "both" => AudioSource::Both,
            _ => AudioSource::System,
        };
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
        "irec selftest: {:?} audio, {} fps, {}x, region {:?}",
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
