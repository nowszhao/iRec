mod audio;
mod frame;
mod hotkey;
mod hud;
mod line;
mod markup;
mod mic;
mod overlay;
mod recorder;
mod scrollpreview;
mod scrollshot;
mod shot;
mod state;
mod update;

use std::cell::{OnceCell, RefCell};
use std::sync::Mutex;
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
    NSControlStateValueOff, NSControlStateValueOn, NSEvent, NSFont, NSImage, NSMenu, NSMenuItem,
    NSAlertSecondButtonReturn, NSModalResponseOK, NSOpenPanel, NSPopUpButton, NSStatusBar,
    NSStatusItem, NSTextField, NSView, NSWarningAlertStyle, NSWindow, NSWindowStyleMask, NSWorkspace,
};
use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSArray, NSBundle, NSNotification, NSObject, NSObjectProtocol,
    NSPoint, NSRect, NSSize, NSString, NSURL,
};

use crate::hotkey::{Hotkey, Hotkeys, KVK_DELETE, KVK_ESCAPE, SLOTS};
use crate::overlay::pick_region;
use crate::recorder::Recorder;
use crate::state::{
    dirs_desktop, init_state, load_persisted, log, persist, push_action, with_state, Action,
    AppState, AudioSource, Quality, Rect, ScrollShotResult, Settings, Update, STATE,
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
    update_item: OnceCell<Retained<NSMenuItem>>,
    update_check_item: OnceCell<Retained<NSMenuItem>>,
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

        #[unsafe(method(toggleMarkup:))]
        fn toggle_markup(&self, _sender: &AnyObject) {
            with_state(|s| {
                s.settings.markup = !s.settings.markup;
                persist(&s.settings);
            });
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

    /// Builds the control window: buttons, quality pickers and live status.
    ///
    /// 高度由每段的高度相加得出，不再写死 —— 之前加控件时忘了加高度，
    /// 最后一行被排到了窗口外面。
    fn build_window(&self, mtm: MainThreadMarker) -> Retained<NSWindow> {
        let width = 380.0;
        let pad: f64 = 18.0;
        let group: f64 = 14.0;
        let bands: [f64; 19] = [
            26.0,  // 标题
            group,
            34.0,  // 开始录制（全屏）
            34.0,  // 选择区域录制
            34.0,  // 停止录制
            group,
            20.0,  // 「参数」小标题
            30.0,  // 声音
            30.0,  // 画质
            30.0,  // 帧率
            30.0,  // 分辨率
            group,
            20.0,  // 「输出」小标题
            32.0,  // 保存位置
            32.0,  // 快捷键
            group,
            22.0,  // 状态行
            12.0,
            32.0,  // 在访达中显示 / 申请检查权限
        ];
        let height: f64 = bands.iter().sum::<f64>() + pad * 2.0;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
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
        let inner = width - pad * 2.0;
        let mut y = height - pad;

        y -= bands[0];
        let title = make_label(
            mtm,
            "iRec · 屏幕录制与截图",
            NSRect::new(NSPoint::new(pad, y + 2.0), NSSize::new(inner, 22.0)),
            15.0,
        );
        title.setFont(Some(&NSFont::boldSystemFontOfSize(15.0)));
        content.addSubview(&title);

        y -= bands[1];

        y -= bands[2];
        let start = make_button(
            mtm,
            "开始录制（全屏）",
            sel!(startFull:),
            NSRect::new(NSPoint::new(pad, y + 1.0), NSSize::new(inner, 32.0)),
            self,
        );
        content.addSubview(&start);
        self.ivars().start_button.set(start).ok();

        y -= bands[3];
        let region = make_button(
            mtm,
            "选择区域录制",
            sel!(pickRegion:),
            NSRect::new(NSPoint::new(pad, y + 1.0), NSSize::new(inner, 32.0)),
            self,
        );
        content.addSubview(&region);
        self.ivars().region_button.set(region).ok();

        y -= bands[4];
        let stop = make_button(
            mtm,
            "停止录制",
            sel!(stopRecording:),
            NSRect::new(NSPoint::new(pad, y + 1.0), NSSize::new(inner, 32.0)),
            self,
        );
        content.addSubview(&stop);
        self.ivars().stop_button.set(stop).ok();

        y -= bands[5];
        y -= bands[6];
        content.addSubview(&section_label(
            mtm,
            "参数",
            NSRect::new(NSPoint::new(pad, y), NSSize::new(inner, 16.0)),
        ));

        y -= bands[7];
        content.addSubview(&make_label(
            mtm,
            "声音",
            NSRect::new(NSPoint::new(pad, y + 4.0), NSSize::new(72.0, 18.0)),
            13.0,
        ));
        let audio_titles: Vec<&str> = AudioSource::all().iter().map(|a| a.label()).collect();
        let audio = make_popup(
            mtm,
            &audio_titles,
            NSRect::new(NSPoint::new(pad + 80.0, y + 2.0), NSSize::new(inner - 80.0, 26.0)),
            sel!(audioChanged:),
            self,
            1,
        );
        content.addSubview(&audio);
        self.ivars().audio_popup.set(audio).ok();

        y -= bands[8];
        content.addSubview(&make_label(
            mtm,
            "画质",
            NSRect::new(NSPoint::new(pad, y + 4.0), NSSize::new(72.0, 18.0)),
            13.0,
        ));
        let quality = make_popup(
            mtm,
            &[
                Quality::Small.label(),
                Quality::Balanced.label(),
                Quality::High.label(),
                Quality::Max.label(),
            ],
            NSRect::new(NSPoint::new(pad + 80.0, y + 2.0), NSSize::new(inner - 80.0, 26.0)),
            sel!(qualityChanged:),
            self,
            1,
        );
        content.addSubview(&quality);

        y -= bands[9];
        content.addSubview(&make_label(
            mtm,
            "帧率",
            NSRect::new(NSPoint::new(pad, y + 4.0), NSSize::new(72.0, 18.0)),
            13.0,
        ));
        let fps = make_popup(
            mtm,
            &["30 fps（省空间）", "60 fps（流畅）"],
            NSRect::new(NSPoint::new(pad + 80.0, y + 2.0), NSSize::new(inner - 80.0, 26.0)),
            sel!(fpsChanged:),
            self,
            0,
        );
        content.addSubview(&fps);

        y -= bands[10];
        content.addSubview(&make_label(
            mtm,
            "分辨率",
            NSRect::new(NSPoint::new(pad, y + 4.0), NSSize::new(72.0, 18.0)),
            13.0,
        ));
        let scale = make_popup(
            mtm,
            &["1x（省空间）", "2x（高清，默认）"],
            NSRect::new(NSPoint::new(pad + 80.0, y + 2.0), NSSize::new(inner - 80.0, 26.0)),
            sel!(scaleChanged:),
            self,
            1,
        );
        content.addSubview(&scale);

        y -= bands[11];
        y -= bands[12];
        content.addSubview(&section_label(
            mtm,
            "输出",
            NSRect::new(NSPoint::new(pad, y), NSSize::new(inner, 16.0)),
        ));

        y -= bands[13];
        let out_dir = make_button(
            mtm,
            "保存位置：选择…",
            sel!(chooseOutputDir:),
            NSRect::new(NSPoint::new(pad, y), NSSize::new(inner, 30.0)),
            self,
        );
        content.addSubview(&out_dir);
        self.ivars().out_dir_button.set(out_dir).ok();

        y -= bands[14];
        let hotkeys = make_button(
            mtm,
            "快捷键设置…",
            sel!(openHotkeys:),
            NSRect::new(NSPoint::new(pad, y), NSSize::new(inner, 30.0)),
            self,
        );
        content.addSubview(&hotkeys);

        y -= bands[15];
        y -= bands[16];
        let status = section_label(
            mtm,
            "就绪",
            NSRect::new(NSPoint::new(pad, y + 2.0), NSSize::new(inner, 18.0)),
        );
        content.addSubview(&status);
        self.ivars().status_label.set(status).ok();

        y -= bands[17];
        y -= bands[18];
        let reveal = make_button(
            mtm,
            "在访达中显示",
            sel!(revealLast:),
            NSRect::new(NSPoint::new(pad, y), NSSize::new(160.0, 30.0)),
            self,
        );
        content.addSubview(&reveal);
        let permission = make_button(
            mtm,
            "申请 / 检查权限",
            sel!(checkPermission:),
            NSRect::new(NSPoint::new(pad + 168.0, y), NSSize::new(inner - 168.0, 30.0)),
            self,
        );
        content.addSubview(&permission);

        // 排完应该正好落在下边距上：不对就说明 bands 和摆放不一致。
        log(&format!("window: {width}x{height}, 底部 y={y}（应为 {pad}）"));
        window.center();
        window
    }

    /// Builds the status menu. 动作按「录制 / 截图 / 晾衣绳 / 设置」分成几组，
    /// 参数类的东西收到「设置」子菜单里，主菜单只留常用项。
    fn build_menu(&self, mtm: MainThreadMarker) -> Retained<NSMenu> {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("irec"));
        // Otherwise AppKit re-enables the stop item on every menu open.
        menu.setAutoenablesItems(false);

        let start = menu_item(mtm, "开始录制（全屏）", Some(sel!(startFull:)), self, 0);
        menu.addItem(&start);
        self.ivars().start_menu_item.set(start).ok();

        let region = menu_item(mtm, "选择区域录制", Some(sel!(pickRegion:)), self, 0);
        menu.addItem(&region);
        self.ivars().region_menu_item.set(region).ok();

        let stop = menu_item(mtm, "停止录制", Some(sel!(stopRecording:)), self, 0);
        menu.addItem(&stop);
        self.ivars().stop_menu_item.set(stop).ok();

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let shot_full = menu_item(mtm, "截取全屏", Some(sel!(shotFull:)), self, 0);
        menu.addItem(&shot_full);
        self.ivars().shot_full_item.set(shot_full).ok();

        let shot_region = menu_item(mtm, "截取选区", Some(sel!(shotRegion:)), self, 0);
        menu.addItem(&shot_region);
        self.ivars().shot_region_item.set(shot_region).ok();

        let scroll_shot = menu_item(mtm, "滚动截屏…", Some(sel!(scrollShot:)), self, 0);
        menu.addItem(&scroll_shot);
        self.ivars().scroll_shot_item.set(scroll_shot).ok();

        let finish_scroll_shot = menu_item(mtm, "完成滚动截屏", Some(sel!(finishScrollShot:)), self, 0);
        finish_scroll_shot.setHidden(true);
        menu.addItem(&finish_scroll_shot);
        self.ivars().finish_scroll_shot_item.set(finish_scroll_shot).ok();

        let cancel_scroll_shot = menu_item(mtm, "取消滚动截屏", Some(sel!(cancelScrollShot:)), self, 0);
        cancel_scroll_shot.setHidden(true);
        menu.addItem(&cancel_scroll_shot);
        self.ivars().cancel_scroll_shot_item.set(cancel_scroll_shot).ok();

        let annotate = menu_item(mtm, "标注最新截图…", Some(sel!(annotateLast:)), self, 0);
        menu.addItem(&annotate);
        self.ivars().annotate_item.set(annotate).ok();

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // 晾衣绳 ▸
        let line_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("晾衣绳"));
        let line_toggle = menu_item(mtm, "显示晾衣绳", Some(sel!(toggleLine:)), self, 0);
        line_menu.addItem(&line_toggle);
        self.ivars().line_toggle_item.set(line_toggle).ok();

        let line_hover = menu_item(
            mtm,
            "指针停在菜单栏时放下",
            Some(sel!(toggleLineHover:)),
            self,
            0,
        );
        line_menu.addItem(&line_hover);
        self.ivars().line_hover_item.set(line_hover).ok();

        let line_enable = menu_item(
            mtm,
            "录完自动挂到晾衣绳",
            Some(sel!(toggleLineEnabled:)),
            self,
            0,
        );
        line_menu.addItem(&line_enable);
        self.ivars().line_enable_item.set(line_enable).ok();

        let line_clear = menu_item(mtm, "清空晾衣绳", Some(sel!(clearLine:)), self, 0);
        line_menu.addItem(&line_clear);

        let line_root = menu_item(mtm, "晾衣绳", None, self, 0);
        menu.addItem(&line_root);
        menu.setSubmenu_forItem(Some(&line_menu), &line_root);

        // 设置 ▸
        let settings = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("设置"));

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
        settings.addItem(&audio_root);
        settings.setSubmenu_forItem(Some(&audio_menu), &audio_root);

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
        let quality_root = menu_item(mtm, "画质", None, self, 0);
        settings.addItem(&quality_root);
        settings.setSubmenu_forItem(Some(&quality_menu), &quality_root);

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
        settings.addItem(&fps_root);
        settings.setSubmenu_forItem(Some(&fps_menu), &fps_root);

        let scale_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("分辨率"));
        for scale in [10i32, 20i32] {
            let label = if scale == 20 { "2x（高清，默认）" } else { "1x（省空间）" };
            let item = menu_item(mtm, label, Some(sel!(setScale:)), self, scale as i64);
            scale_menu.addItem(&item);
            self.ivars().scale_items.borrow_mut().push((scale, item));
        }
        let scale_root = menu_item(mtm, "分辨率", None, self, 0);
        settings.addItem(&scale_root);
        settings.setSubmenu_forItem(Some(&scale_menu), &scale_root);

        settings.addItem(&NSMenuItem::separatorItem(mtm));
        let out_dir_item = menu_item(mtm, "保存位置：—", None, self, 0);
        out_dir_item.setEnabled(false);
        settings.addItem(&out_dir_item);
        self.ivars().out_dir_item.set(out_dir_item).ok();

        let choose = menu_item(mtm, "选择保存位置…", Some(sel!(chooseOutputDir:)), self, 0);
        settings.addItem(&choose);
        let reset_dir = menu_item(mtm, "恢复为桌面", Some(sel!(resetOutputDir:)), self, 0);
        settings.addItem(&reset_dir);

        settings.addItem(&NSMenuItem::separatorItem(mtm));
        let hotkeys = menu_item(mtm, "快捷键设置…", Some(sel!(openHotkeys:)), self, 0);
        settings.addItem(&hotkeys);
        let markup = menu_item(mtm, "截图后打开标注", Some(sel!(toggleMarkup:)), self, 0);
        settings.addItem(&markup);
        self.ivars().markup_item.set(markup).ok();

        let settings_root = menu_item(mtm, "设置", None, self, 0);
        menu.addItem(&settings_root);
        menu.setSubmenu_forItem(Some(&settings), &settings_root);

        let reveal = menu_item(mtm, "在访达中显示最新录制", Some(sel!(revealLast:)), self, 0);
        menu.addItem(&reveal);
        self.ivars().last_file.set(reveal).ok();

        let open_window = menu_item(mtm, "打开控制窗口…", Some(sel!(showWindow:)), self, 0);
        menu.addItem(&open_window);

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let permission = menu_item(
            mtm,
            "申请 / 检查屏幕录制权限",
            Some(sel!(checkPermission:)),
            self,
            0,
        );
        menu.addItem(&permission);

        let check_update = menu_item(mtm, "检查更新…", Some(sel!(checkUpdate:)), self, 0);
        menu.addItem(&check_update);

        let auto_update = menu_item(
            mtm,
            "启动时检查更新",
            Some(sel!(toggleUpdateCheck:)),
            self,
            0,
        );
        menu.addItem(&auto_update);
        self.ivars().update_check_item.set(auto_update).ok();

        let update_item =
            menu_item(mtm, "↑ 发现新版本，点此更新", Some(sel!(installUpdate:)), self, 0);
        update_item.setHidden(true);
        menu.addItem(&update_item);
        self.ivars().update_item.set(update_item).ok();

        let status_line = menu_item(mtm, "就绪", None, self, 0);
        status_line.setHidden(true);
        menu.addItem(&status_line);
        self.ivars().status_line.set(status_line).ok();

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let quit = menu_item(mtm, "退出", Some(sel!(quit:)), self, 0);
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
            Action::PickScrollShot => match scrollshot::frontmost_target_pid() {
                Ok(target_pid) => {
                    with_state(|s| {
                        s.last_error = None;
                        s.scroll_shot_status = Some("请框选页面内容；开始后 iRec 会临时切换到目标应用".to_string());
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
                if scrollshot::activate_pid(self.mtm(), target_pid) {
                    scrollshot::start(rect, target_pid, scale, out_dir);
                } else {
                    scrollpreview::close(self.mtm());
                    with_state(|s| {
                        s.scroll_shot_status = None;
                        s.last_error =
                            Some("目标应用已退出，请重新开始滚动截屏".to_string());
                    });
                }
            }
            Action::CancelScrollShot => {
                with_state(|s| s.scroll_shot_status = Some("正在取消并恢复原位置…".to_string()));
                scrollshot::activate_target(self.mtm());
                scrollshot::request_cancel();
            }
            Action::FinishScrollShot => {
                with_state(|s| s.scroll_shot_status = Some("正在完成并恢复原位置…".to_string()));
                scrollshot::activate_target(self.mtm());
                scrollshot::request_finish();
            }
            Action::ScrollShotProgress(message) => {
                with_state(|s| s.scroll_shot_status = Some(message));
            }
            Action::ScrollShotSelectionCancelled => {
                with_state(|s| s.scroll_shot_status = None);
            }
            Action::ScrollShotFinished(result) => {
                scrollshot::finish_on_main_thread(self.mtm());
                scrollpreview::close(self.mtm());
                with_state(|s| s.scroll_shot_status = None);
                let warning = match &result {
                    ScrollShotResult::FinishedWithWarning(_, _, warning) => {
                        Some(warning.clone())
                    }
                    _ => None,
                };
                match result {
                    ScrollShotResult::Finished(path, bytes)
                    | ScrollShotResult::FinishedWithWarning(path, bytes, _) => {
                        line::hang_later(path.clone(), None);
                        with_state(|s| {
                            s.last_saved = Some((path.clone(), bytes));
                            s.saved_at = Some(Instant::now());
                            s.last_error = warning.clone();
                        });
                        if with_state(|s| s.settings.markup) {
                            push_action(Action::Markup(path));
                        }
                    }
                    ScrollShotResult::Cancelled => {
                        log("scroll shot cancelled; original position restored");
                    }
                    ScrollShotResult::Failed(err) => {
                        with_state(|s| s.last_error = Some(err));
                    }
                }
            }
            Action::Markup(path) => {
                crate::markup::edit(path, |path| {
                    line::refresh(path);
                });
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
            format!(
                "{message}\n\n发布包是 ad-hoc 签名，换包后屏幕录制权限可能要重新授权一次。"
            )
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
        let scroll_status = with_state(|s| s.scroll_shot_status.clone());
        let scrolling = scrollshot::is_active();
        if let Some(item) = self.ivars().stop_menu_item.get() {
            item.setEnabled(recording && !stopping);
        }
        if let Some(item) = self.ivars().scroll_shot_item.get() {
            item.setHidden(scrolling);
            item.setEnabled(!recording && !stopping && !scrolling && scroll_status.is_none());
        }
        if let Some(item) = self.ivars().finish_scroll_shot_item.get() {
            item.setHidden(!scrolling);
            item.setEnabled(
                scrolling
                    && !scrollshot::cancel_requested()
                    && scroll_status.as_deref() != Some("正在完成并恢复原位置…"),
            );
        }
        if let Some(item) = self.ivars().cancel_scroll_shot_item.get() {
            item.setHidden(!scrolling);
            item.setEnabled(scrolling);
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
        if let Some(item) = self.ivars().update_item.get() {
            let view = with_state(|s| {
                s.update.as_ref().map(|u| {
                    let title = if u.busy {
                        u.message.clone().unwrap_or_else(|| "正在检查更新…".to_string())
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
                    item.setHidden(false);
                    item.setEnabled(actionable);
                }
                None => item.setHidden(true),
            }
        }
        if let Some(item) = self.ivars().update_check_item.get() {
            item.setState(state_value(with_state(|s| s.settings.update_check)));
        }
        let hotkeys = with_state(|s| s.settings.hotkeys);
        let named = [
            (self.ivars().start_menu_item.get(), "开始录制（全屏）", hotkeys.record_full),
            (self.ivars().region_menu_item.get(), "选择区域录制", hotkeys.record_region),
            (self.ivars().shot_full_item.get(), "截取全屏", hotkeys.shot_full),
            (self.ivars().shot_region_item.get(), "截取选区", hotkeys.shot_region),
        ];
        for (item, name, key) in named {
            if let Some(item) = item {
                item.setTitle(&NSString::from_str(&format!("{name}   {}", key.display())));
            }
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
                    Some(status.clone())
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
