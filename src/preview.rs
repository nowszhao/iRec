//! Native, local preview of a capture. Closing it never changes the file.
use std::cell::{OnceCell, RefCell};
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, class, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_av_foundation::AVPlayer;
use objc2_foundation::{
    MainThreadMarker, NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString, NSURL,
};

#[link(name = "AVKit", kind = "framework")]
unsafe extern "C" {}

thread_local! {
    static PREVIEW: RefCell<Option<Preview>> = const { RefCell::new(None) };
}

struct Preview {
    panel: Retained<NSPanel>,
    controller: Retained<PreviewController>,
}

#[derive(Default)]
struct PreviewIvars {
    path: OnceCell<PathBuf>,
    image: OnceCell<Retained<NSImage>>,
    player: OnceCell<Retained<AVPlayer>>,
    status: OnceCell<Retained<NSTextField>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = PreviewIvars]
    struct PreviewController;

    unsafe impl NSObjectProtocol for PreviewController {}
    unsafe impl NSWindowDelegate for PreviewController {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _: &NSNotification) {
            if let Some(player) = self.ivars().player.get() { unsafe { player.pause() }; }
        }
    }

    impl PreviewController {
        #[unsafe(method(copyCapture:))]
        fn copy_capture(&self, _: &AnyObject) {
            let board = NSPasteboard::generalPasteboard();
            board.clearContents();
            let ok = if let Some(image) = self.ivars().image.get() {
                let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&**image);
                board.writeObjects(&NSArray::from_slice(&[writer]))
            } else {
                let url = file_url(self.path());
                let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*url);
                board.writeObjects(&NSArray::from_slice(&[writer]))
            };
            self.note(if ok { "已复制" } else { "复制失败，请重试" });
        }

        #[unsafe(method(annotateCapture:))]
        fn annotate_capture(&self, _: &AnyObject) {
            let path = self.path().to_path_buf();
            close();
            crate::markup::edit(path, crate::line::refresh);
        }

        #[unsafe(method(togglePlayback:))]
        fn toggle_playback(&self, sender: &NSButton) {
            if let Some(player) = self.ivars().player.get() {
                let playing = unsafe { player.rate() } != 0.0;
                unsafe { if playing { player.pause(); } else { player.play(); } }
                sender.setTitle(&NSString::from_str(if playing { "播放" } else { "暂停" }));
                sender.setImage(NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(if playing { "play.fill" } else { "pause.fill" }), None).as_deref());
            }
        }

        #[unsafe(method(openCapture:))]
        fn open_capture(&self, _: &AnyObject) {
            if !NSWorkspace::sharedWorkspace().openURL(&file_url(self.path())) {
                self.note("无法打开文件");
            }
        }

        #[unsafe(method(revealCapture:))]
        fn reveal_capture(&self, _: &AnyObject) {
            NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[file_url(self.path())]));
        }

        #[unsafe(method(removeCapture:))]
        fn remove_capture(&self, _: &AnyObject) {
            crate::line::remove_path(self.path(), false);
            close();
        }

        #[unsafe(method(trashCapture:))]
        fn trash_capture(&self, _: &AnyObject) {
            if crate::line::remove_path(self.path(), true) { close(); }
            else { self.note("无法移到废纸篓，文件仍保留"); }
        }

        #[unsafe(method(closePreview:))]
        fn close_preview(&self, _: &AnyObject) { close(); }

        #[unsafe(method(moreCapture:))]
        fn more_capture(&self, sender: &NSButton) {
            let menu = NSMenu::new(self.mtm());
            for (label, action) in [
                ("用默认应用打开", sel!(openCapture:)),
                ("在访达中显示", sel!(revealCapture:)),
                ("从晾衣绳移除", sel!(removeCapture:)),
                ("移到废纸篓", sel!(trashCapture:)),
            ] {
                if label == "从晾衣绳移除" { menu.addItem(&NSMenuItem::separatorItem(self.mtm())); }
                let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(self.mtm()), &NSString::from_str(label), Some(action), &NSString::new()) };
                unsafe { item.setTarget(Some(self)); }
                menu.addItem(&item);
            }
            menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(0.0, 0.0), Some(sender));
        }
    }
);

impl PreviewController {
    fn path(&self) -> &Path {
        self.ivars().path.get().expect("preview path")
    }
    fn note(&self, message: &str) {
        if let Some(label) = self.ivars().status.get() {
            label.setStringValue(&NSString::from_str(message));
        }
    }
}

fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), false)
}

pub fn is_visible() -> bool {
    PREVIEW.with(|p| p.borrow().as_ref().is_some_and(|p| p.panel.isVisible()))
}

pub fn close() {
    PREVIEW.with(|p| {
        if let Some(p) = p.borrow().as_ref() {
            if let Some(player) = p.controller.ivars().player.get() {
                unsafe { player.pause() };
            }
            p.panel.close();
        }
    });
}

pub fn show(path: PathBuf, video: bool, anchor: NSRect) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    close();
    let Some(screen) = NSScreen::screens(mtm)
        .iter()
        .find(|s| {
            let f = s.frame();
            anchor.origin.x >= f.origin.x && anchor.origin.x < f.max().x
        })
        .or_else(|| NSScreen::mainScreen(mtm))
    else {
        return;
    };
    let screen = screen.visibleFrame();
    let width = 600.0_f64.min(screen.size.width - 24.0);
    let height = 460.0_f64.min(screen.size.height - 48.0);
    let frame = NSRect::new(
        NSPoint::new(
            (anchor.origin.x + anchor.size.width / 2.0 - width / 2.0)
                .clamp(screen.origin.x + 8.0, screen.max().x - width - 8.0),
            (anchor.origin.y - height - 12.0)
                .clamp(screen.origin.y + 8.0, screen.max().y - height - 28.0),
        ),
        NSSize::new(width, height),
    );
    let panel = unsafe {
        NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::UtilityWindow,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        panel.setReleasedWhenClosed(false);
    }
    panel.setTitle(&NSString::from_str(if video {
        "录屏预览"
    } else {
        "截图预览"
    }));
    panel.setSharingType(NSWindowSharingType::None);
    panel.setLevel(NSFloatingWindowLevel + 2);
    panel.setHidesOnDeactivate(false);
    let controller = PreviewController::alloc(mtm).set_ivars(PreviewIvars::default());
    let controller: Retained<PreviewController> = unsafe { msg_send![super(controller), init] };
    controller.ivars().path.set(path.clone()).ok();
    unsafe {
        panel.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    }
    let content = panel.contentView().expect("preview content");
    let media_frame = NSRect::new(
        NSPoint::new(12.0, 64.0),
        NSSize::new(width - 24.0, height - 76.0),
    );
    if video {
        let player = unsafe { AVPlayer::playerWithURL(&file_url(&path), mtm) };
        // AVPlayerView supplies native playback, scrubbing and accessibility.
        let view: objc2::rc::Allocated<NSView> = unsafe { msg_send![class!(AVPlayerView), alloc] };
        let view: Retained<NSView> = unsafe { msg_send![view, initWithFrame: media_frame] };
        unsafe {
            let _: () = msg_send![&*view, setPlayer: &*player];
        }
        content.addSubview(&view);
        controller.ivars().player.set(player).ok();
    } else if let Some(image) = NSImage::initWithContentsOfFile(
        NSImage::alloc(),
        &NSString::from_str(&path.to_string_lossy()),
    ) {
        let size = image.size();
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), media_frame);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setScrollerStyle(NSScrollerStyle::Overlay);
        scroll.setBackgroundColor(&NSColor::controlBackgroundColor());
        let scale = (media_frame.size.width / size.width.max(1.0)).min(1.0);
        let doc_h = (size.height * scale).max(media_frame.size.height);
        let view = NSImageView::initWithFrame(
            NSImageView::alloc(mtm),
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(media_frame.size.width, doc_h),
            ),
        );
        view.setImage(Some(&image));
        view.setImageScaling(NSImageScaling::ScaleProportionallyDown);
        scroll.setDocumentView(Some(&view));
        content.addSubview(&scroll);
        view.scrollPoint(NSPoint::new(
            0.0,
            (doc_h - media_frame.size.height).max(0.0),
        ));
        controller.ivars().image.set(image).ok();
    }
    let label = NSTextField::labelWithString(
        &NSString::from_str(&path.file_name().unwrap_or_default().to_string_lossy()),
        mtm,
    );
    label.setFrame(NSRect::new(
        NSPoint::new(16.0, 42.0),
        NSSize::new(width - 32.0, 18.0),
    ));
    label.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    label.setTextColor(Some(&NSColor::secondaryLabelColor()));
    content.addSubview(&label);
    controller.ivars().status.set(label).ok();
    let actions = [
        ("复制", "doc.on.doc", sel!(copyCapture:), "c"),
        (
            if video { "播放" } else { "标注" },
            if video { "play.fill" } else { "pencil" },
            if video {
                sel!(togglePlayback:)
            } else {
                sel!(annotateCapture:)
            },
            if video { " " } else { "e" },
        ),
        ("更多", "ellipsis", sel!(moreCapture:), ""),
        ("关闭", "xmark", sel!(closePreview:), "\u{1b}"),
    ];
    for (i, (title, symbol, action, key)) in actions.into_iter().enumerate() {
        let button = NSButton::initWithFrame(
            NSButton::alloc(mtm),
            NSRect::new(
                NSPoint::new(12.0 + i as f64 * 100.0, 8.0),
                NSSize::new(92.0, 28.0),
            ),
        );
        button.setTitle(&NSString::from_str(title));
        button.setBezelStyle(NSBezelStyle::Rounded);
        button.setImage(
            NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str(symbol),
                None,
            )
            .as_deref(),
        );
        button.setImagePosition(NSCellImagePosition::ImageLeading);
        button.setToolTip(Some(&NSString::from_str(title)));
        button.setKeyEquivalent(&NSString::from_str(key));
        unsafe {
            button.setKeyEquivalentModifierMask(if key == "\u{1b}" || key == " " {
                NSEventModifierFlags::empty()
            } else {
                NSEventModifierFlags::Command
            });
            button.setTarget(Some(&*controller));
            button.setAction(Some(action));
        }
        content.addSubview(&button);
    }
    panel.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    PREVIEW.with(|p| *p.borrow_mut() = Some(Preview { panel, controller }));
}
