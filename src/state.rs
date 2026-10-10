use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::recorder::Recorder;

/// Commands queued from the menu bar, global hotkeys and the region picker.
/// They are drained on the main thread by the app delegate's timer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// Start/stop a full screen recording.
    ToggleFull,
    /// Open the region picker overlay.
    PickRegion,
    /// Start a recording of the selected rectangle (Cocoa screen coordinates).
    StartRegion(Rect),
    /// Stop whatever is currently recording.
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Small,
    Balanced,
    High,
    Max,
}

impl Quality {
    /// Target bits per (pixel * second). HEVC handles screen content well, so
    /// these numbers stay deliberately low to keep files small.
    pub fn bpp(self) -> f64 {
        match self {
            Quality::Small => 0.016,
            Quality::Balanced => 0.030,
            Quality::High => 0.052,
            Quality::Max => 0.090,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Quality::Small => "更小体积",
            Quality::Balanced => "均衡（默认）",
            Quality::High => "高清",
            Quality::Max => "极清",
        }
    }

    pub fn all() -> [Quality; 4] {
        [Quality::Small, Quality::Balanced, Quality::High, Quality::Max]
    }
}

#[derive(Clone)]
pub struct Settings {
    pub audio: bool,
    pub fps: i32,
    /// 1.0 = point size, 2.0 = retina pixels.
    pub scale: f64,
    pub quality: Quality,
    pub show_cursor: bool,
    pub out_dir: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio: true,
            fps: 30,
            scale: 2.0,
            quality: Quality::Balanced,
            show_cursor: true,
            out_dir: dirs_desktop(),
        }
    }
}

pub struct AppState {
    pub settings: Settings,
    pub recorder: Option<Recorder>,
    /// True while the previous recording is still being finalised.
    pub stopping: bool,
    pub started_at: Option<Instant>,
    pub actions: VecDeque<Action>,
    /// Last finished recording: (path, bytes).
    pub last_saved: Option<(PathBuf, u64)>,
    /// When the last recording finished (used for the transient status title).
    pub saved_at: Option<Instant>,
    pub last_error: Option<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            recorder: None,
            stopping: false,
            started_at: None,
            actions: VecDeque::new(),
            last_saved: None,
            saved_at: None,
            last_error: None,
        }
    }
}

pub static STATE: Mutex<Option<AppState>> = Mutex::new(None);

pub fn init_state() {
    *STATE.lock().unwrap() = Some(AppState::default());
}

pub fn with_state<R>(f: impl FnOnce(&mut AppState) -> R) -> R {
    let mut guard = match STATE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(guard.as_mut().expect("app state not initialised"))
}

/// Appends a line to `~/Library/Logs/irec.log` (best effort).
pub fn log(message: &str) {
    use std::io::Write as _;
    eprintln!("iRec: {message}");
    let Ok(home) = std::env::var("HOME") else { return };
    let path = std::path::PathBuf::from(home).join("Library/Logs/irec.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "{secs}	{message}");
    }
}

pub fn push_action(action: Action) {
    with_state(|s| s.actions.push_back(action));
}

pub fn dirs_desktop() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join("Desktop");
        if p.is_dir() {
            return p;
        }
    }
    PathBuf::from(".")
}
