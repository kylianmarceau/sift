//! Reading key presses in Sift's own windows, to record a keyboard
//! shortcut.
//!
//! A local event monitor sees each key press before any window does and
//! can keep it from going further. Only Sift's own windows are watched, so
//! this needs no permission (watching other apps would need Input
//! Monitoring).
//!
//! Each press reports the physical key code, which is what registering a
//! global shortcut needs and which does not depend on the keyboard layout,
//! and the character the key types in the current layout, for display.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags};

/// One key press.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyPress {
    /// The hardware key code (`kVK_…`), the same on every layout.
    pub key_code: u16,
    pub command: bool,
    pub control: bool,
    pub option: bool,
    pub shift: bool,
    /// What the key types with no modifiers in the current layout ("a",
    /// "é", "-"); empty for keys that type nothing.
    pub character: String,
}

/// While alive, every key press in Sift's windows goes to the callback
/// and nowhere else. Dropping it stops the capture.
pub struct KeyCapture {
    monitor: Retained<AnyObject>,
}

impl KeyCapture {
    /// Starts capturing. Main thread only: returns `None` elsewhere, or if
    /// AppKit refuses the monitor. Held keys (auto-repeat) are swallowed
    /// without being reported.
    pub fn start(on_key: impl Fn(KeyPress) + 'static) -> Option<Self> {
        MainThreadMarker::new()?;
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit passes a valid key-down event that lives for the
            // duration of this call.
            let event = unsafe { event.as_ref() };
            if !event.isARepeat() {
                let flags = event.modifierFlags();
                let character = event
                    .charactersByApplyingModifiers(NSEventModifierFlags::empty())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                on_key(KeyPress {
                    key_code: event.keyCode(),
                    command: flags.contains(NSEventModifierFlags::Command),
                    control: flags.contains(NSEventModifierFlags::Control),
                    option: flags.contains(NSEventModifierFlags::Option),
                    shift: flags.contains(NSEventModifierFlags::Shift),
                    character,
                });
            }
            // nil: the event stops here, so the key does not also type or
            // trigger a menu item.
            std::ptr::null_mut()
        });
        // SAFETY: the mask selects key-down events only, which is what the
        // handler reads; the handler has the signature AppKit expects and
        // AppKit copies the block, keeping it until the monitor is removed.
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &handler)
        }?;
        Some(Self { monitor })
    }
}

impl Drop for KeyCapture {
    fn drop(&mut self) {
        // SAFETY: `monitor` is the object AppKit returned when adding it, and
        // it is removed exactly once. KeyCapture is not Send, so this runs on
        // the main thread where it was created.
        unsafe { NSEvent::removeMonitor(&self.monitor) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_the_main_thread() {
        // Test threads are not the main thread, so AppKit is never touched.
        assert!(KeyCapture::start(|_| {}).is_none());
    }
}
