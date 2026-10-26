//! Pressing ⌘V in the frontmost app, for paste-and-pop.
//!
//! Posting key events needs the Accessibility permission (see
//! [`crate::wm::trusted`]). The V key is found on the current keyboard
//! layout with ⌘ held, so ⌘V pastes on Dvorak, AZERTY or "Dvorak – QWERTY
//! ⌘" alike.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::time::{Duration, Instant};

use objc2::MainThreadMarker;

type Ref = *const c_void;

/// `kVK_ANSI_V`: V on a US layout, used when the layout cannot be read.
const ANSI_V: u16 = 0x09;
/// `kCGEventSourceStateCombinedSessionState`.
const COMBINED_SESSION_STATE: i32 = 0;
/// `kCGEventSourceStateHIDSystemState`: the keys held right now.
const HID_SYSTEM_STATE: i32 = 1;
/// `kCGSessionEventTap`.
const SESSION_EVENT_TAP: u32 = 1;
/// `kCGEventFlagMaskCommand`.
const COMMAND: u64 = 1 << 20;
/// Shift, Control, Option and Command (`kCGEventFlagMask…`).
const MODIFIERS: u64 = (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20);
/// `cmdKey >> 8`, the modifier state `UCKeyTranslate` takes for ⌘.
const UC_COMMAND: u32 = 1;
/// `kUCKeyActionDown`.
const UC_KEY_DOWN: u16 = 0;
/// `kUCKeyTranslateNoDeadKeysBit`.
const UC_NO_DEAD_KEYS: u32 = 1;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state: i32) -> Ref;
    fn CGEventSourceFlagsState(state: i32) -> u64;
    fn CGEventCreateKeyboardEvent(source: Ref, key: u16, down: bool) -> Ref;
    fn CGEventSetFlags(event: Ref, flags: u64);
    fn CGEventPost(tap: u32, event: Ref);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: Ref);
    fn CFDataGetBytePtr(data: Ref) -> *const u8;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyUnicodeKeyLayoutData: Ref;
    fn TISCopyCurrentKeyboardLayoutInputSource() -> Ref;
    fn TISGetInputSourceProperty(source: Ref, key: Ref) -> Ref;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const c_void,
        key: u16,
        action: u16,
        modifiers: u32,
        keyboard_type: u32,
        options: u32,
        dead_key_state: *mut u32,
        max_length: usize,
        length: *mut usize,
        chars: *mut u16,
    ) -> i32;
}

/// The key code that types "v" with ⌘ held on the current layout. macOS
/// reads the layout only on the main thread; elsewhere this is V's US
/// position.
pub fn paste_key() -> u16 {
    if MainThreadMarker::new().is_none() {
        return ANSI_V;
    }
    // SAFETY: the Copy result is released below; the property and its
    // bytes are owned by the input source and used while it is alive.
    unsafe {
        let source = TISCopyCurrentKeyboardLayoutInputSource();
        if source.is_null() {
            return ANSI_V;
        }
        let data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
        let key = if data.is_null() {
            ANSI_V
        } else {
            let layout = CFDataGetBytePtr(data).cast::<c_void>();
            let keyboard_type = u32::from(LMGetKbdType());
            find_key(|key| {
                let mut dead = 0u32;
                let mut length = 0usize;
                let mut chars = [0u16; 4];
                let status = UCKeyTranslate(
                    layout,
                    key,
                    UC_KEY_DOWN,
                    UC_COMMAND,
                    keyboard_type,
                    UC_NO_DEAD_KEYS,
                    &mut dead,
                    chars.len(),
                    &mut length,
                    chars.as_mut_ptr(),
                );
                (status == 0 && length == 1).then(|| chars[0])
            })
        };
        CFRelease(source);
        key
    }
}

/// The first key code (V's US position first) whose character is "v".
fn find_key(char_of: impl Fn(u16) -> Option<u16>) -> u16 {
    let v = [u16::from(b'v'), u16::from(b'V')];
    std::iter::once(ANSI_V)
        .chain(0..128)
        .find(|&key| char_of(key).is_some_and(|c| v.contains(&c)))
        .unwrap_or(ANSI_V)
}

/// Presses ⌘V (with `key` from [`paste_key`]) once the modifier keys are
/// let go, waiting at most `wait`, so that the keys of the shortcut that
/// asked for it do not mix in. Blocks while waiting: call it off the main
/// thread. Without Accessibility, macOS drops the events.
pub fn press_paste(key: u16, wait: Duration) {
    let deadline = Instant::now() + wait;
    // SAFETY: reads the current modifier state; no pointers involved.
    while unsafe { CGEventSourceFlagsState(HID_SYSTEM_STATE) } & MODIFIERS != 0
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: each Create result is checked and released once posted.
    unsafe {
        let source = CGEventSourceCreate(COMBINED_SESSION_STATE);
        for down in [true, false] {
            let event = CGEventCreateKeyboardEvent(source, key, down);
            if event.is_null() {
                continue;
            }
            CGEventSetFlags(event, COMMAND);
            CGEventPost(SESSION_EVENT_TAP, event);
            CFRelease(event);
        }
        if !source.is_null() {
            CFRelease(source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_v_wherever_the_layout_puts_it() {
        let qwerty = |key| (key == 0x09).then_some(u16::from(b'v'));
        assert_eq!(find_key(qwerty), 0x09);
        // Dvorak types "v" at QWERTY's "." position.
        let dvorak = |key| match key {
            0x09 => Some(u16::from(b'.')),
            0x2f => Some(u16::from(b'v')),
            _ => None,
        };
        assert_eq!(find_key(dvorak), 0x2f);
        assert_eq!(find_key(|_| None), ANSI_V);
    }
}
