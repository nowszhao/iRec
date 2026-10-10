//! Global hotkeys through the Carbon Event Manager.
//!
//! Unlike `NSEvent` global monitors or `CGEventTap`s this needs no
//! accessibility / input-monitoring permission, so the app works right after
//! the very first launch.

use std::ffi::c_void;
use std::sync::Mutex;

use crate::state::{push_action, Action};

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
pub const K_EVENT_PARAM_DIRECT_OBJECT: u32 = 0x2d2d2d2d; // '----'
pub const TYPE_EVENT_HOT_KEY_ID: u32 = 0x686b6964; // 'hkid'

pub const CMD_KEY: u32 = 256;
pub const SHIFT_KEY: u32 = 512;

/// Virtual key codes (kVK_ANSI_*).
pub const KVK_ANSI_R: u32 = 15;
pub const KVK_ANSI_E: u32 = 14;

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

/// Registers `Cmd+Shift+R` (full screen) and `Cmd+Shift+E` (region).
pub fn register_defaults() -> Result<(), String> {
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

    register(KVK_ANSI_R, CMD_KEY | SHIFT_KEY, 1)?;
    register(KVK_ANSI_E, CMD_KEY | SHIFT_KEY, 2)?;
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
            1 => push_action(Action::ToggleFull),
            2 => push_action(Action::PickRegion),
            _ => {}
        }
    }
    0
}
