mod audio;
mod frame;
mod hotkey;
mod hud;
mod line;
mod markup;
mod mic;
mod overlay;
mod recorder;
mod shot;
mod state;

use std::cell::{OnceCell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::AnyThread;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSBackingStoreType, NSBezierPath, NSButton, NSButtonType, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSFont, NSImage, NSMenu, NSMenuItem,
    NSAlertSecondButtonReturn, NSModalResponseOK, NSOpenPanel, NSPopUpButton, NSStatusBar,
    NSStatusItem, NSTextField, NSWarningAlertStyle, NSWindow, NSWindowStyleMask, NSWorkspace,
};
use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSArray, NSBundle, NSNotification, NSObject, NSObjectProtocol,
    NSPoint, NSRect, NSSize, NSString, NSURL,
};

use crate::overlay::pick_region;
use crate::recorder::Recorder;
use crate::state::{
    dirs_desktop, init_state, load_persisted, log, persist, push_action, with_state, Action,
    AppState, AudioSource, Quality, Rect, Settings, STATE,
};

#[derive(Default)]
struct AppIvars {
    status_item: OnceCell<Retained<NSStatusItem>>,
    audio_items: RefCell<Vec<(usize, Retained<NSMenuItem>)>>,
    quality_items: RefCell<Vec<(usize, Retained<NSMenuItem>)>>,
    fps_items: RefCell<Vec<(i32, Retained<NSMenuItem>)>>,
    scale_items: RefCell<Vec<(i32, Retained<NSMenuItem>)>>,
    last_file: OnceCell<Retained<NSMenuItem>>,
    status_line: OnceCell<Retained<NSMenuItem>>,
    window: OnceCell<Retained<NSWindow>>,
    status_label: OnceCell<Retained<NSTextField>>,
    audio_popup: OnceCell<Retained<NSPopUpButton>>,
    out_dir_item: OnceCell<Retained<NSMenuItem>>,
    out_dir_button: OnceCell<Retained<NSButton>>,
    start_button: OnceCell<Retained<NSButton>>,
    region_button: OnceCell<Retained<NSButton>>,
    stop_button: OnceCell<Retained<NSButton>>,
    stop_menu_item: OnceCell<Retained<NSMenuItem>>,
    line_toggle_item: OnceCell<Retained<NSMenuItem>>,
    line_hover_item: OnceCell<Retained<NSMenuItem>>,
    line_enable_item: OnceCell<Retained<NSMenuItem>>,
    annotate_item: OnceCell<Retained<NSMenuItem>>,
    markup_item: OnceCell<Retained<NSMenuItem>>,
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
            line::init(self.mtm());
            let (enabled, hover) = with_state(|s| (s.settings.line, s.settings.line_hover));
            line::apply_settings(enabled, hover);
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

        #[unsafe(method(shotFull:))]
        fn shot_full(&self, _sender: &AnyObject) {
            push_action(Action::ShotFull);
        }

        #[unsafe(method(shotRegion:))]
        fn shot_region(&self, _sender: &AnyObject) {
            push_action(Action::PickShot);
        }

        #[unsafe(method(annotateLast:))]
        fn annotate_last(&self, _sender: &AnyObject) {
            if let Some(path) = self.latest_shot() {
                push_action(Action::Markup(path));
            }
        }

        #[unsafe(method(toggleMarkup:))]
        fn toggle_markup(&self, _sender: &AnyObject) {
            with_state(|s| {
                s.settings.markup = !s.settings.markup;
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
            panel.setMessage(Some(&NSString::from_str("录制的视频会保存到这个位置")));
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
            // Asking explicitly shows macOS' own prompt when needed.
            let granted = crate::recorder::request_permission(true)
                || unsafe { CGPreflightScreenCaptureAccess() };
            log(&format!("permission check: preflight={granted}"));
            if granted {
                with_state(|s| s.permission_blocked = false);
                let alert = unsafe { NSAlert::new(self.mtm()) };
                alert.setMessageText(&NSString::from_str("屏幕录制权限已开启"));
                alert.setInformativeText(&NSString::from_str(
                    "可以开始录制了。要是刚才录过一次失败的，完全退出再打开 iRec 就好。",
                ));
                alert.addButtonWithTitle(&NSString::from_str("好"));
                alert.runModal();
            } else {
                self.show_permission_alert();
            }
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
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, 508.0));
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
        let audio_label = make_label(
            mtm,
            "声音",
            NSRect::new(NSPoint::new(20.0, y + 4.0), NSSize::new(50.0, 18.0)),
            13.0,
        );
        content.addSubview(&audio_label);
        let audio_titles: Vec<&str> = AudioSource::all().iter().map(|a| a.label()).collect();
        let audio = make_popup(
            mtm,
            &audio_titles,
            NSRect::new(NSPoint::new(90.0, y), NSSize::new(width - 110.0, 26.0)),
            sel!(audioChanged:),
            self,
            1,
        );
        content.addSubview(&audio);
        self.ivars().audio_popup.set(audio).ok();

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

        y -= 44.0;
        let out_dir = make_button(
            mtm,
            "保存位置：选择…",
            sel!(chooseOutputDir:),
            NSRect::new(NSPoint::new(20.0, y), NSSize::new(width - 40.0, 30.0)),
            self,
        );
        content.addSubview(&out_dir);
        self.ivars().out_dir_button.set(out_dir).ok();

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
        let shot_full = menu_item(mtm, "截取全屏      ⇧⌘S", Some(sel!(shotFull:)), self, 0);
        menu.addItem(&shot_full);

        let shot_region = menu_item(mtm, "截取选区      ⇧⌘A", Some(sel!(shotRegion:)), self, 0);
        menu.addItem(&shot_region);

        let annotate = menu_item(mtm, "标注最新截图…", Some(sel!(annotateLast:)), self, 0);
        menu.addItem(&annotate);
        self.ivars().annotate_item.set(annotate).ok();

        let markup = menu_item(mtm, "截图后打开标注", Some(sel!(toggleMarkup:)), self, 0);
        menu.addItem(&markup);
        self.ivars().markup_item.set(markup).ok();

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let audio_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("声音"));
        for (index, source) in AudioSource::all().iter().enumerate() {
            let item = menu_item(
                mtm,
                source.label(),
                Some(sel!(setAudioSource:)),
                self,
                index as i64,
            );
            audio_menu.addItem(&item);
            self.ivars().audio_items.borrow_mut().push((index, item));
        }
        let audio_root = menu_item(mtm, "声音", None, self, 0);
        menu.addItem(&audio_root);
        menu.setSubmenu_forItem(Some(&audio_menu), &audio_root);

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

        let out_dir_item = menu_item(mtm, "保存位置：—", None, self, 0);
        out_dir_item.setEnabled(false);
        menu.addItem(&out_dir_item);
        self.ivars().out_dir_item.set(out_dir_item).ok();

        let choose = menu_item(
            mtm,
            "选择保存位置…",
            Some(sel!(chooseOutputDir:)),
            self,
            0,
        );
        menu.addItem(&choose);

        let reset_dir = menu_item(
            mtm,
            "恢复为桌面",
            Some(sel!(resetOutputDir:)),
            self,
            0,
        );
        menu.addItem(&reset_dir);

        let open_window = menu_item(mtm, "打开控制窗口…", Some(sel!(showWindow:)), self, 0);
        menu.addItem(&open_window);

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let line_toggle = menu_item(mtm, "显示晾衣绳", Some(sel!(toggleLine:)), self, 0);
        menu.addItem(&line_toggle);
        self.ivars().line_toggle_item.set(line_toggle).ok();

        let line_hover = menu_item(
            mtm,
            "指针停在菜单栏时放下",
            Some(sel!(toggleLineHover:)),
            self,
            0,
        );
        menu.addItem(&line_hover);
        self.ivars().line_hover_item.set(line_hover).ok();

        let line_enable = menu_item(
            mtm,
            "录完自动挂到晾衣绳",
            Some(sel!(toggleLineEnabled:)),
            self,
            0,
        );
        menu.addItem(&line_enable);
        self.ivars().line_enable_item.set(line_enable).ok();

        let line_clear = menu_item(mtm, "清空晾衣绳", Some(sel!(clearLine:)), self, 0);
        menu.addItem(&line_clear);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

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
            Action::Markup(path) => {
                crate::markup::edit(path, |path| {
                    line::refresh(path);
                });
            }
        }
    }

    /// Opens the Screen Recording pane of System Settings.
    fn open_settings(&self) {
        open_screen_capture_settings();
    }

    /// 屏幕录制权限没生效时的引导。macOS 的授权只对之后启动的进程生效，
    /// 所以主要动作是「重新启动 iRec」，而不是反复去系统设置。
    fn show_permission_alert(&self) {
        let granted = unsafe { CGPreflightScreenCaptureAccess() };
        let alert = unsafe { NSAlert::new(self.mtm()) };
        alert.setAlertStyle(NSWarningAlertStyle);
        if granted {
            alert.setMessageText(&NSString::from_str("权限已开启，重启 iRec 后生效"));
            alert.setInformativeText(&NSString::from_str(
                "macOS 的屏幕录制授权只对之后启动的进程生效，所以要完全退出再打开一次 iRec。\n\
                 要是重开之后还这样：去系统设置把 iRec 的开关关掉再打开一次——\
                 开关里可能记着上一个签名（重新编译过就很常见）。",
            ));
        } else {
            alert.setMessageText(&NSString::from_str("屏幕录制权限还没生效"));
            alert.setInformativeText(&NSString::from_str(
                "1) 在 系统设置 → 隐私与安全性 → 屏幕录制 里，把 iRec 的开关\
                 关掉再打开一次：只确认它是开着的不够，开关里可能记着上一个签名\
                 （重新编译过就很常见）；列表里有多条 iRec 就全部打开。\n\
                 2) 授权之后要完全退出再打开 iRec——授权只对之后启动的进程生效，\
                 这也是「设置里已经勾了、这边还是录不了」最常见的原因；\
                 本弹窗第一个按钮就是「重新启动 iRec」。\n\
                 3) 如果列表里根本没有 iRec，就先按一次 ⇧⌘R，让系统弹出授权框并选“允许”。\n\
                 4) 反复失效的话：默认打包是 ad-hoc 签名，每次重建都是新的身份，\
                 旧的授权会作废；用 SIGN_IDENTITY 签名（见 README）可固定身份。",
            ));
        }
        alert.addButtonWithTitle(&NSString::from_str("重新启动 iRec"));
        alert.addButtonWithTitle(&NSString::from_str("打开系统设置"));
        alert.addButtonWithTitle(&NSString::from_str("好"));
        let choice = alert.runModal();
        if choice == NSAlertFirstButtonReturn {
            self.relaunch();
        } else if choice == NSAlertSecondButtonReturn {
            self.open_settings();
        }
    }

    /// 退出并重新打开自己：授权改动之后就差这一步。
    fn relaunch(&self) {
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
        self.stop_sync_if_recording();
        NSApplication::sharedApplication(self.mtm()).terminate(None);
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
                log(&format!("start failed: {err} (permission blocked={blocked})"));
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
                            line::hang_later(path, region);
                        }
                    }
                    Err(err) => {
                        log(&format!("stop failed: {err}"));
                        s.last_error = Some(err);
                    }
                }
            });
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
        let settings = with_state(|s| s.settings.clone());
        crate::shot::capture(
            region,
            settings.scale,
            &settings.out_dir,
            true,
            move |result| match result {
                Ok((path, bytes)) => {
                    let markup = with_state(|s| s.settings.markup);
                    with_state(|s| {
                        s.last_saved = Some((path.clone(), bytes));
                        s.saved_at = Some(Instant::now());
                        s.last_error = None;
                    });
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
            let _ = recorder.stop();
            with_state(|s| {
                s.stopping = false;
                s.started_at = None;
            });
        }
    }

    /// Mirrors the current state into the control window.
    fn update_window(&self) {
        let (label, recording, stopping, audio, out_dir) = with_state(|s| {
            (
                window_status(s),
                s.recorder.is_some(),
                s.stopping,
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
        for (index, item) in self.ivars().audio_items.borrow().iter() {
            let matches = AudioSource::all()[*index] == settings.audio;
            item.setState(state_value(matches));
        }
        if let Some(item) = self.ivars().out_dir_item.get() {
            item.setTitle(&NSString::from_str(&format!(
                "保存位置：{}",
                short_path(&settings.out_dir)
            )));
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
        let (enabled, hover, revealed) = line::status();
        if let Some(item) = self.ivars().line_toggle_item.get() {
            item.setTitle(&NSString::from_str(if revealed {
                "收起晾衣绳"
            } else {
                "显示晾衣绳"
            }));
            item.setEnabled(enabled);
        }
        if let Some(item) = self.ivars().line_hover_item.get() {
            item.setState(state_value(hover));
            item.setEnabled(enabled);
        }
        if let Some(item) = self.ivars().line_enable_item.get() {
            item.setState(state_value(enabled));
        }
        if let Some(item) = self.ivars().markup_item.get() {
            item.setState(state_value(with_state(|s| s.settings.markup)));
        }
        if let Some(item) = self.ivars().annotate_item.get() {
            item.setEnabled(self.latest_shot().is_some());
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
                } else if let Some(err) = &s.last_error {
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

fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png") | Some("jpg") | Some("jpeg") | Some("tiff") | Some("heic")
    )
}

/// 给 sh 用的单引号转义。
fn shell_quote(text: &str) -> String {
    text.replace('\'', "'\\''")
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
    if args.iter().any(|a| a == "--shot") {
        shot_cli(&args);
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
