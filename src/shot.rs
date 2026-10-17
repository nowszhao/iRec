//! 截图：用 ScreenCaptureKit 的 `SCScreenshotManager` 抓一帧，存成 PNG，
//! 再像录屏一样挂到晾衣绳上（截图卡片上没有播放键）。
//!
//! 自己的窗口（控制窗口、选区框、晾衣绳）通过 `SCContentFilter` 排除，
//! 所以截图里不会拍到它们。

use std::path::{Path, PathBuf};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSPNGFileType};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSDictionary, NSError, NSString};
use objc2_screen_capture_kit::{SCContentFilter, SCScreenshotManager, SCStreamConfiguration};

use crate::line;
use crate::recorder::{fetch_shareable_content, local_timestamp, main_display, own_windows};
use crate::state::{log, Rect};

/// 抓一张图存进 `dir`，默认挂到晾衣绳上；`done` 在抓完（或失败）后调用。
///
/// `region` 用 iRec 的屏幕坐标：原点在屏幕左下，y 朝上。
pub fn capture(
    region: Option<Rect>,
    scale: f64,
    dir: &Path,
    hang: bool,
    done: impl FnOnce(Result<(PathBuf, u64), String>) + Send + 'static,
) {
    let content = match fetch_shareable_content() {
        Ok(content) => content,
        Err(err) => {
            done(Err(err));
            return;
        }
    };
    let display = match main_display(&content) {
        Ok(display) => display,
        Err(err) => {
            done(Err(err));
            return;
        }
    };
    // 自己的窗口不进截图。
    let excluded = own_windows(&content);
    let frame = unsafe { display.frame() };
    let (display_w, display_h) = (frame.size.width as f64, frame.size.height as f64);
    let scale = scale.max(0.5);

    // 整屏：整块显示器；选区：把 y 朝上的 Rect 换成截图要的矩形。
    let (source, width, height) = match region {
        Some(r) => {
            let w = r.w.max(2.0);
            let h = r.h.max(2.0);
            (
                CGRect {
                    origin: CGPoint {
                        x: r.x,
                        y: display_h - r.y - r.h,
                    },
                    size: CGSize {
                        width: w,
                        height: h,
                    },
                },
                (w * scale).round() as usize,
                (h * scale).round() as usize,
            )
        }
        None => (
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: frame.size,
            },
            (display_w * scale).round() as usize,
            (display_h * scale).round() as usize,
        ),
    };

    let cfg = unsafe { SCStreamConfiguration::new() };
    unsafe {
        cfg.setWidth(width);
        cfg.setHeight(height);
        cfg.setSourceRect(source);
        cfg.setShowsCursor(false);
        cfg.setScalesToFit(false);
        cfg.setPreservesAspectRatio(true);
    }
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &excluded,
        )
    };

    let dir = dir.to_path_buf();
    // 回调只会跑一次，但 block 得是 Fn：把它藏在 Mutex 里，跑的时候取出来。
    let done: std::sync::Mutex<Option<Box<dyn FnOnce(Result<(PathBuf, u64), String>) + Send>>> =
        std::sync::Mutex::new(Some(Box::new(done)));
    let block = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let result = if image.is_null() {
            Err(message_of(error))
        } else {
            match unsafe { Retained::retain(image) } {
                Some(image) => match save_png(&image, &dir) {
                    Ok((path, bytes)) => Ok((path, bytes, image)),
                    Err(err) => Err(err),
                },
                None => Err("截图返回了空图像".to_string()),
            }
        };
        let callback = done.lock().ok().and_then(|mut guard| guard.take());
        let Some(callback) = callback else { return };
        match result {
            Ok((path, bytes, image)) => {
                log(&format!(
                    "shot saved {} ({:.2} MB)",
                    path.display(),
                    bytes as f64 / 1_048_576.0
                ));
                if hang {
                    line::hang_image(image, path.clone(), region);
                }
                callback(Ok((path, bytes)));
            }
            Err(err) => callback(Err(err)),
        }
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &cfg,
            Some(&block),
        )
    };
}

fn message_of(error: *mut NSError) -> String {
    if error.is_null() {
        return "截图失败".to_string();
    }
    let error = unsafe { Retained::retain(error) };
    match error {
        Some(error) => error.to_string(),
        None => "截图失败".to_string(),
    }
}

/// CGImage → PNG 文件。
pub(crate) fn save_png(image: &CGImage, dir: &Path) -> Result<(PathBuf, u64), String> {
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), image);
    let props = NSDictionary::<NSBitmapImageRepPropertyKey, AnyObject>::new();
    let data = unsafe { rep.representationUsingType_properties(NSPNGFileType, &props) }
        .ok_or_else(|| "PNG 编码失败".to_string())?;
    let path = shot_path(dir);
    if !data.writeToFile_atomically(&NSString::from_str(&path.to_string_lossy()), true) {
        return Err(format!("写文件失败：{}", path.display()));
    }
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok((path, bytes))
}

fn shot_path(dir: &Path) -> PathBuf {
    let stamp = local_timestamp();
    let mut candidate = dir.join(format!("iRec-shot-{stamp}.png"));
    let mut i = 1;
    while candidate.exists() {
        candidate = dir.join(format!("iRec-shot-{stamp}-{i}.png"));
        i += 1;
    }
    candidate
}
