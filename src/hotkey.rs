//! Global hotkeys through the Carbon Event Manager.
//!
//! Unlike `NSEvent` global monitors or `CGEventTap`s this needs no
//! accessibility / input-monitoring permission, so the app works right after
//! the very first launch.
//!
//! 四个动作的快捷键都能改，存在 NSUserDefaults 里（见 `state.rs` 的 Settings）。

use std::ffi::c_void;
use std::sync::Mutex;

use objc2_app_kit::NSEventModifierFlags;

use crate::state::{log, push_action, Action};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventHotKeyID {
    pub signature: u32,
    pub id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventTypeSpec {
    pub event_class: u32,
    pub event_kind: u32,
}

type OSStatus = i32;

/// Opaque Carbon references. They are only touched from the main thread, which
/// is where the hot key handler runs.
#[derive(Clone, Copy)]
pub struct EventHandlerRef(pub *mut c_void);
unsafe impl Send for EventHandlerRef {}
#[derive(Clone, Copy)]
pub struct EventHotKeyRef(pub *mut c_void);
unsafe impl Send for EventHotKeyRef {}
pub type EventRef = *mut c_void;
pub type EventTargetRef = *mut c_void;
pub type EventHandlerCallRef = *mut c_void;

pub const K_EVENT_CLASS_KEYBOARD: u32 = 0x6b657962; // 'keyb'
pub const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
/// `kEventParamDirectObject` = '----'. Four hyphens: the value is a four
/// character code, and a shorter literal silently matches no parameter.
pub const K_EVENT_PARAM_DIRECT_OBJECT: u32 = 0x2d2d2d2d; // '----'
pub const TYPE_EVENT_HOT_KEY_ID: u32 = 0x686b6964; // 'hkid'

pub const CMD_KEY: u32 = 256;
pub const SHIFT_KEY: u32 = 512;
pub const OPT_KEY: u32 = 2048;
pub const CTRL_KEY: u32 = 4096;

/// Virtual key codes (kVK_ANSI_*).
pub const KVK_ANSI_R: u32 = 15;
pub const KVK_ANSI_E: u32 = 14;
pub const KVK_ANSI_S: u32 = 1;
pub const KVK_ANSI_A: u32 = 0;

pub const KVK_DELETE: u16 = 51;
pub const KVK_ESCAPE: u16 = 53;

static HANDLER: Mutex<Option<EventHandlerRef>> = Mutex::new(None);
static KEYS: Mutex<Vec<EventHotKeyRef>> = Mutex::new(Vec::new());

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerProcPtr,
        num_types: u32,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut *mut c_void,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        hot_key_id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out: *mut *mut c_void,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hot_key: *mut c_void) -> OSStatus;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        buffer_size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> OSStatus;
}

pub type EventHandlerProcPtr =
    unsafe extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

/// 一个全局快捷键：虚拟键码 + Carbon 修饰键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub key_code: u32,
    pub modifiers: u32,
}

impl Hotkey {
    /// 键码取这个值表示「没设快捷键」。
    pub const NONE: u32 = u32::MAX;

    pub fn none() -> Self {
        Self {
            key_code: Self::NONE,
            modifiers: 0,
        }
    }

    pub fn is_none(self) -> bool {
        self.key_code == Self::NONE
    }

    /// 至少要带 ⌘ / ⌥ / ⌃ 之一：纯字母会把正常打字挡住，⇧ 也一样容易误触。
    pub fn valid(self) -> bool {
        !self.is_none() && self.modifiers & (CMD_KEY | OPT_KEY | CTRL_KEY) != 0
    }

    /// 显示成 `⇧⌘R` 这样的样子（顺序 ⌃⌥⇧⌘，和 macOS 一致）。
    pub fn display(self) -> String {
        if self.is_none() {
            return "未设置".to_string();
        }
        let mut out = String::new();
        if self.modifiers & CTRL_KEY != 0 {
            out.push('⌃');
        }
        if self.modifiers & OPT_KEY != 0 {
            out.push('⌥');
        }
        if self.modifiers & SHIFT_KEY != 0 {
            out.push('⇧');
        }
        if self.modifiers & CMD_KEY != 0 {
            out.push('⌘');
        }
        out.push_str(&key_name(self.key_code));
        out
    }

    /// 存进 NSUserDefaults 的形式：`修饰键:键码`。
    pub fn encode(self) -> String {
        format!("{}:{}", self.modifiers, self.key_code)
    }

    pub fn decode(text: &str) -> Option<Self> {
        let (modifiers, key_code) = text.split_once(':')?;
        Some(Self {
            key_code: key_code.parse().ok()?,
            modifiers: modifiers.parse().ok()?,
        })
    }

    /// NSEvent 的修饰键位 → Carbon 的修饰键位。
    pub fn event_modifiers(flags: NSEventModifierFlags) -> u32 {
        let mut modifiers = 0;
        if flags.contains(NSEventModifierFlags::Command) {
            modifiers |= CMD_KEY;
        }
        if flags.contains(NSEventModifierFlags::Shift) {
            modifiers |= SHIFT_KEY;
        }
        if flags.contains(NSEventModifierFlags::Option) {
            modifiers |= OPT_KEY;
        }
        if flags.contains(NSEventModifierFlags::Control) {
            modifiers |= CTRL_KEY;
        }
        modifiers
    }
}

/// 四个可改的快捷键。slot 就是 Carbon 热键的 id。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkeys {
    pub record_full: Hotkey,
    pub record_region: Hotkey,
    pub shot_full: Hotkey,
    pub shot_region: Hotkey,
}

pub const SLOT_RECORD_FULL: u32 = 1;
pub const SLOT_RECORD_REGION: u32 = 2;
pub const SLOT_SHOT_FULL: u32 = 3;
pub const SLOT_SHOT_REGION: u32 = 4;
pub const SLOTS: [u32; 4] = [
    SLOT_RECORD_FULL,
    SLOT_RECORD_REGION,
    SLOT_SHOT_FULL,
    SLOT_SHOT_REGION,
];

impl Default for Hotkeys {
    fn default() -> Self {
        let cmd_shift = CMD_KEY | SHIFT_KEY;
        Self {
            record_full: Hotkey {
                key_code: KVK_ANSI_R,
                modifiers: cmd_shift,
            },
            record_region: Hotkey {
                key_code: KVK_ANSI_E,
                modifiers: cmd_shift,
            },
            shot_full: Hotkey {
                key_code: KVK_ANSI_S,
                modifiers: cmd_shift,
            },
            shot_region: Hotkey {
                key_code: KVK_ANSI_A,
                modifiers: cmd_shift,
            },
        }
    }
}

impl Hotkeys {
    pub fn get(self, slot: u32) -> Hotkey {
        match slot {
            SLOT_RECORD_REGION => self.record_region,
            SLOT_SHOT_FULL => self.shot_full,
            SLOT_SHOT_REGION => self.shot_region,
            _ => self.record_full,
        }
    }

    pub fn set(&mut self, slot: u32, key: Hotkey) {
        match slot {
            SLOT_RECORD_REGION => self.record_region = key,
            SLOT_SHOT_FULL => self.shot_full = key,
            SLOT_SHOT_REGION => self.shot_region = key,
            _ => self.record_full = key,
        }
    }

    pub fn label(slot: u32) -> &'static str {
        match slot {
            SLOT_RECORD_REGION => "录制（选区）",
            SLOT_SHOT_FULL => "截图（全屏）",
            SLOT_SHOT_REGION => "截图（选区）",
            _ => "录制（全屏）",
        }
    }

    /// 同一个组合只留一个位置：新设的生效，原来那个清掉。
    pub fn clear_conflicts(&mut self, slot: u32, key: Hotkey) {
        if key.is_none() {
            return;
        }
        for other in SLOTS {
            if other != slot && self.get(other) == key {
                self.set(other, Hotkey::none());
            }
        }
    }
}

/// 按当前设置重新注册四个热键。重复调用会先注销旧的。
pub fn apply(hotkeys: &Hotkeys) -> Result<(), String> {
    install_handler()?;
    unregister_all();
    let mut failed = None;
    for slot in SLOTS {
        let key = hotkeys.get(slot);
        if !key.valid() {
            continue;
        }
        if let Err(err) = register(key.key_code, key.modifiers, slot) {
            failed = Some(err);
        }
    }
    match failed {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn install_handler() -> Result<(), String> {
    if HANDLER.lock().unwrap().is_some() {
        return Ok(());
    }
    let mut handler_ref: *mut c_void = std::ptr::null_mut();
    let spec = [EventTypeSpec {
        event_class: K_EVENT_CLASS_KEYBOARD,
        event_kind: K_EVENT_HOT_KEY_PRESSED,
    }];
    let status = unsafe {
        InstallEventHandler(
            GetApplicationEventTarget(),
            hotkey_handler,
            1,
            spec.as_ptr(),
            std::ptr::null_mut(),
            &mut handler_ref,
        )
    };
    if status != 0 {
        return Err(format!("InstallEventHandler failed: {status}"));
    }
    *HANDLER.lock().unwrap() = Some(EventHandlerRef(handler_ref));
    Ok(())
}

fn register(key_code: u32, modifiers: u32, id: u32) -> Result<(), String> {
    let mut key_ref: *mut c_void = std::ptr::null_mut();
    let hot_key_id = EventHotKeyID {
        signature: 0x73637263, // 'scrc'
        id,
    };
    let status = unsafe {
        RegisterEventHotKey(
            key_code,
            modifiers,
            hot_key_id,
            GetApplicationEventTarget(),
            0,
            &mut key_ref,
        )
    };
    if status != 0 {
        return Err(format!("RegisterEventHotKey failed: {status}"));
    }
    KEYS.lock().unwrap().push(EventHotKeyRef(key_ref));
    Ok(())
}

pub fn unregister_all() {
    let mut keys = KEYS.lock().unwrap();
    for key in keys.drain(..) {
        unsafe {
            UnregisterEventHotKey(key.0);
        }
    }
}

unsafe extern "C" fn hotkey_handler(
    _call_ref: EventHandlerCallRef,
    event: EventRef,
    _user_data: *mut c_void,
) -> OSStatus {
    let mut id = EventHotKeyID {
        signature: 0,
        id: 0,
    };
    let status = unsafe {
        GetEventParameter(
            event,
            K_EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyID>(),
            std::ptr::null_mut(),
            &mut id as *mut EventHotKeyID as *mut c_void,
        )
    };
    if status == 0 {
        match id.id {
            SLOT_RECORD_FULL => push_action(Action::ToggleFull),
            SLOT_RECORD_REGION => push_action(Action::PickRegion),
            SLOT_SHOT_FULL => push_action(Action::ShotFull),
            SLOT_SHOT_REGION => push_action(Action::PickShot),
            _ => {}
        }
    } else {
        // 取不到参数说明常量或事件名不对，别让热键静默失效。
        log(&format!("hotkey: GetEventParameter 失败 status={status}"));
    }
    0
}

/// 虚拟键码 → 显示用的字符。查不到的就显示键码，不至于空白。
fn key_name(code: u32) -> String {
    let name = match code {
        0 => "A",
        1 => "S",
        2 => "D",
        3 => "F",
        4 => "H",
        5 => "G",
        6 => "Z",
        7 => "X",
        8 => "C",
        9 => "V",
        10 => "§",
        11 => "B",
        12 => "Q",
        13 => "W",
        14 => "E",
        15 => "R",
        16 => "Y",
        17 => "T",
        18 => "1",
        19 => "2",
        20 => "3",
        21 => "4",
        22 => "6",
        23 => "5",
        24 => "=",
        25 => "9",
        26 => "7",
        27 => "-",
        28 => "8",
        29 => "0",
        30 => "]",
        31 => "O",
        32 => "U",
        33 => "[",
        34 => "I",
        35 => "P",
        36 => "↩",
        37 => "L",
        38 => "J",
        39 => "'",
        40 => "K",
        41 => ";",
        42 => "\\",
        43 => ",",
        44 => "/",
        45 => "N",
        46 => "M",
        47 => ".",
        48 => "⇥",
        49 => "␣",
        50 => "`",
        51 => "⌫",
        53 => "⎋",
        96 => "F5",
        97 => "F6",
        98 => "F7",
        99 => "F3",
        100 => "F8",
        101 => "F9",
        109 => "F10",
        103 => "F11",
        105 => "F13",
        111 => "F12",
        118 => "F4",
        119 => "F2",
        120 => "F1",
        122 => "F1",
        123 => "←",
        124 => "→",
        125 => "↓",
        126 => "↑",
        other => return format!("键 {other}"),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_cmd_shift_resa() {
        let keys = Hotkeys::default();
        assert_eq!(keys.record_full.display(), "⇧⌘R");
        assert_eq!(keys.record_region.display(), "⇧⌘E");
        assert_eq!(keys.shot_full.display(), "⇧⌘S");
        assert_eq!(keys.shot_region.display(), "⇧⌘A");
    }

    #[test]
    fn encode_decode_round_trip() {
        let key = Hotkey {
            key_code: 9, // V
            modifiers: CMD_KEY | OPT_KEY,
        };
        assert_eq!(key.display(), "⌥⌘V");
        assert_eq!(Hotkey::decode(&key.encode()), Some(key));
        let none = Hotkey::none();
        assert_eq!(Hotkey::decode(&none.encode()), Some(none));
        assert_eq!(none.display(), "未设置");
    }

    #[test]
    fn plain_letters_are_rejected() {
        let plain = Hotkey {
            key_code: 9,
            modifiers: 0,
        };
        assert!(!plain.valid());
        let shift_only = Hotkey {
            key_code: 9,
            modifiers: SHIFT_KEY,
        };
        assert!(!shift_only.valid());
    }

    #[test]
    fn carbon_four_char_codes_are_four_bytes() {
        // 'keyb' / 'hkid' / '----' 都是四字符码，少写一个字节就会查不到参数。
        assert_eq!(K_EVENT_CLASS_KEYBOARD, u32::from_be_bytes(*b"keyb"));
        assert_eq!(TYPE_EVENT_HOT_KEY_ID, u32::from_be_bytes(*b"hkid"));
        assert_eq!(K_EVENT_PARAM_DIRECT_OBJECT, u32::from_be_bytes(*b"----"));
    }

    #[test]
    fn conflicts_are_cleared() {
        let mut keys = Hotkeys::default();
        // 把「录制（全屏）」设成原本「录制（选区）」的 ⇧⌘E。
        let key = Hotkey {
            key_code: KVK_ANSI_E,
            modifiers: CMD_KEY | SHIFT_KEY,
        };
        keys.set(SLOT_RECORD_FULL, key);
        keys.clear_conflicts(SLOT_RECORD_FULL, key);
        assert_eq!(keys.record_full, key);
        assert_eq!(keys.record_region, Hotkey::none());
        // 不冲突的位置不受影响。
        assert_eq!(keys.shot_full.display(), "⇧⌘S");
    }
}
