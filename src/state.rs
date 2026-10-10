use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::recorder::Recorder;

/// Commands queued from the menu bar, global hotkeys and the region picker.
/// They are drained on the main thread by the app delegate's timer.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Start/stop a full screen recording.
    ToggleFull,
    /// Open the region picker overlay.
    PickRegion,
    /// Start a recording of the selected rectangle (Cocoa screen coordinates).
    StartRegion(Rect),
    /// Stop whatever is currently recording.
    Stop,
    /// Screenshot the whole screen.
    ShotFull,
    /// Open the region picker to screenshot the selection.
    PickShot,
    /// Screenshot the selected rectangle (Cocoa screen coordinates).
    ShotRegion(Rect),
    /// Open a saved image in the system Markup editor.
    Markup(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Where the recorded audio comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSource {
    Off,
    System,
    Mic,
    Both,
}

impl AudioSource {
    pub fn label(self) -> &'static str {
        match self {
            AudioSource::Off => "关闭",
            AudioSource::System => "系统声音",
            AudioSource::Mic => "麦克风",
            AudioSource::Both => "系统声音 + 麦克风",
        }
    }

    pub fn all() -> [AudioSource; 4] {
        [
            AudioSource::Off,
            AudioSource::System,
            AudioSource::Mic,
            AudioSource::Both,
        ]
    }

    /// 是否要录系统声音。
    pub fn wants_system(self) -> bool {
        matches!(self, AudioSource::System | AudioSource::Both)
    }

    /// 是否要录麦克风。
    pub fn wants_mic(self) -> bool {
        matches!(self, AudioSource::Mic | AudioSource::Both)
    }

    /// 是否需要音轨（ScreenCaptureKit 的系统声音采集同时充当混音载体）。
    pub fn any(self) -> bool {
        !matches!(self, AudioSource::Off)
    }
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
    pub audio: AudioSource,
    pub fps: i32,
    /// 1.0 = point size, 2.0 = retina pixels.
    pub scale: f64,
    pub quality: Quality,
    pub show_cursor: bool,
    pub out_dir: PathBuf,
    /// 晾衣绳：录完的视频挂在菜单栏下面。
    pub line: bool,
    /// 指针停在菜单栏时把绳子放下来。
    pub line_hover: bool,
    /// 截完图是否直接打开系统标注。
    pub markup: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio: AudioSource::System,
            fps: 30,
            scale: 2.0,
            quality: Quality::Balanced,
            show_cursor: true,
            out_dir: dirs_desktop(),
            line: true,
            line_hover: true,
            markup: true,
        }
    }
}

/// 设置持久化（NSUserDefaults）：保存路径等设置重启后仍然生效。
const KEY_OUT_DIR: &str = "outDir";
const KEY_AUDIO: &str = "audioSource";
const KEY_QUALITY: &str = "quality";
const KEY_FPS: &str = "fps";
const KEY_SCALE: &str = "scale";
const KEY_LINE: &str = "line";
const KEY_LINE_HOVER: &str = "lineHover";
const KEY_MARKUP: &str = "markup";

pub fn load_persisted(settings: &mut Settings) {
    use objc2_foundation::{NSUserDefaults, NSString};
    let defaults = NSUserDefaults::standardUserDefaults();
    if let Some(dir) = defaults.stringForKey(&NSString::from_str(KEY_OUT_DIR)) {
        let path = PathBuf::from(dir.to_string());
        if path.is_dir() {
            settings.out_dir = path;
        }
    }
    let audio = defaults.integerForKey(&NSString::from_str(KEY_AUDIO));
    if audio >= 0 && (audio as usize) < AudioSource::all().len() {
        settings.audio = AudioSource::all()[audio as usize];
    }
    let quality = defaults.integerForKey(&NSString::from_str(KEY_QUALITY));
    if quality >= 0 && (quality as usize) < Quality::all().len() {
        settings.quality = Quality::all()[quality as usize];
    }
    let fps = defaults.integerForKey(&NSString::from_str(KEY_FPS));
    if fps == 30 || fps == 60 {
        settings.fps = fps as i32;
    }
    let scale = defaults.doubleForKey(&NSString::from_str(KEY_SCALE));
    if scale == 1.0 || scale == 2.0 {
        settings.scale = scale;
    }
    // 没写过的键默认是 false，所以先看看有没有存过。
    if defaults.objectForKey(&NSString::from_str(KEY_LINE)).is_some() {
        settings.line = defaults.boolForKey(&NSString::from_str(KEY_LINE));
    }
    if defaults
        .objectForKey(&NSString::from_str(KEY_LINE_HOVER))
        .is_some()
    {
        settings.line_hover = defaults.boolForKey(&NSString::from_str(KEY_LINE_HOVER));
    }
    if defaults.objectForKey(&NSString::from_str(KEY_MARKUP)).is_some() {
        settings.markup = defaults.boolForKey(&NSString::from_str(KEY_MARKUP));
    }
}

pub fn persist(settings: &Settings) {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSUserDefaults, NSString};
    let defaults = NSUserDefaults::standardUserDefaults();
    let dir = NSString::from_str(&settings.out_dir.to_string_lossy());
    unsafe {
        let value: &AnyObject =
            &*(&*dir as *const NSString as *const AnyObject);
        defaults.setObject_forKey(Some(value), &NSString::from_str(KEY_OUT_DIR));
    }
    let audio = AudioSource::all()
        .iter()
        .position(|a| *a == settings.audio)
        .unwrap_or(1) as isize;
    let quality = Quality::all()
        .iter()
        .position(|q| *q == settings.quality)
        .unwrap_or(1) as isize;
    defaults.setInteger_forKey(audio, &NSString::from_str(KEY_AUDIO));
    defaults.setInteger_forKey(quality, &NSString::from_str(KEY_QUALITY));
    defaults.setInteger_forKey(settings.fps as isize, &NSString::from_str(KEY_FPS));
    defaults.setDouble_forKey(settings.scale, &NSString::from_str(KEY_SCALE));
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
    /// 当前这次录制框的是哪块区域（屏幕坐标，左上角为原点），录完卡片从这里飞上来。
    pub region: Option<Rect>,
    /// When the last recording finished (used for the transient status title).
    pub saved_at: Option<Instant>,
    pub last_error: Option<String>,
    /// 上一次开始录制被 macOS 的权限挡住了：提示和状态行都按这个走。
    pub permission_blocked: bool,
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
            region: None,
            saved_at: None,
            last_error: None,
            permission_blocked: false,
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
