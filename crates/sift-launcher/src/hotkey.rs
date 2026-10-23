//! The keyboard shortcut that shows and hides the panel.
//!
//! Option+Space unless the user records another one in Settings. The
//! choice is kept in `hotkey`, a two-line file in the support directory: the
//! shortcut in `global-hotkey`'s notation ("alt+Space"), then the key's
//! label as the keyboard layout showed it when recorded ("Space", "K",
//! "É"). A missing or unreadable file means the default.
//!
//! Shortcuts are registered by physical key, so they work the same on
//! every keyboard layout; the label is only for display.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use futures::channel::mpsc::UnboundedSender;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use gpui::{App, Global};
use sift_macos::key_capture::KeyPress;

const STATE_FILE: &str = "hotkey";
/// Escape (`kVK_Escape`) without modifiers cancels recording.
const ESCAPE: u16 = 0x35;

/// A shortcut and how to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    hotkey: HotKey,
    /// The key's label: "Space", "K", "F5", "←".
    key: String,
}

impl Default for Shortcut {
    /// Option+Space.
    fn default() -> Self {
        Self {
            hotkey: HotKey::new(Some(Modifiers::ALT), Code::Space),
            key: "Space".into(),
        }
    }
}

impl Shortcut {
    pub fn is_default(&self) -> bool {
        self.hotkey == Self::default().hotkey
    }

    /// The modifier symbols in macOS order (⌃⌥⇧⌘), then the key.
    pub fn parts(&self) -> Vec<String> {
        let mods = self.hotkey.mods;
        [
            (Modifiers::CONTROL, "⌃"),
            (Modifiers::ALT, "⌥"),
            (Modifiers::SHIFT, "⇧"),
            (Modifiers::META, "⌘"),
        ]
        .into_iter()
        .filter(|(m, _)| mods.contains(*m))
        .map(|(_, symbol)| symbol.to_string())
        .chain(std::iter::once(self.key.clone()))
        .collect()
    }

    /// "⌥Space", "⌃⌥K".
    pub fn display(&self) -> String {
        self.parts().concat()
    }

    /// For VoiceOver: "Option Space", "Control Option K".
    pub fn spoken(&self) -> String {
        let mods = self.hotkey.mods;
        let mut words: Vec<&str> = [
            (Modifiers::CONTROL, "Control"),
            (Modifiers::ALT, "Option"),
            (Modifiers::SHIFT, "Shift"),
            (Modifiers::META, "Command"),
        ]
        .into_iter()
        .filter(|(m, _)| mods.contains(*m))
        .map(|(_, word)| word)
        .collect();
        words.push(&self.key);
        words.join(" ")
    }

    /// A note for shortcuts that macOS also uses by default.
    pub fn system_conflict(&self) -> Option<&'static str> {
        if self.hotkey.key != Code::Space {
            return None;
        }
        let mods = self.hotkey.mods;
        if mods == Modifiers::META {
            Some(
                "⌘Space also opens Spotlight. Turn that off in System Settings › Keyboard › \
                 Keyboard Shortcuts › Spotlight.",
            )
        } else if mods == Modifiers::CONTROL || mods == Modifiers::CONTROL | Modifiers::ALT {
            Some(
                "macOS may also use this to switch input sources (System Settings › Keyboard \
                 › Keyboard Shortcuts › Input Sources).",
            )
        } else {
            None
        }
    }
}

/// What a key press means while recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    Cancel,
    Shortcut(Shortcut),
    /// Not usable; the reason, for the user.
    Rejected(&'static str),
}

/// Interprets a key press made while recording a shortcut.
pub fn interpret(press: &KeyPress) -> Recorded {
    let plain = !(press.command || press.control || press.option || press.shift);
    if press.key_code == ESCAPE && plain {
        return Recorded::Cancel;
    }
    let Some(code) = code_for(press.key_code) else {
        return Recorded::Rejected("Sift cannot use that key in a shortcut.");
    };
    if !(press.command || press.control || press.option) && !is_function_key(code) {
        return Recorded::Rejected(
            "Include ⌘, ⌃ or ⌥ so the shortcut does not get in the way of typing.",
        );
    }
    let mut mods = Modifiers::empty();
    for (on, m) in [
        (press.command, Modifiers::META),
        (press.control, Modifiers::CONTROL),
        (press.option, Modifiers::ALT),
        (press.shift, Modifiers::SHIFT),
    ] {
        if on {
            mods |= m;
        }
    }
    Recorded::Shortcut(Shortcut {
        hotkey: HotKey::new(Some(mods), code),
        key: label(code, &press.character),
    })
}

fn is_function_key(code: Code) -> bool {
    use Code::*;
    matches!(
        code,
        F1 | F2
            | F3
            | F4
            | F5
            | F6
            | F7
            | F8
            | F9
            | F10
            | F11
            | F12
            | F13
            | F14
            | F15
            | F16
            | F17
            | F18
            | F19
            | F20
    )
}

/// The physical keys a shortcut may use, by hardware key code. The same
/// table as `global-hotkey`'s, minus media keys (which it watches through
/// an event tap that needs a permission), Caps Lock, Help and the input
/// source keys.
const KEYS: &[(u16, Code)] = &[
    (0x00, Code::KeyA),
    (0x01, Code::KeyS),
    (0x02, Code::KeyD),
    (0x03, Code::KeyF),
    (0x04, Code::KeyH),
    (0x05, Code::KeyG),
    (0x06, Code::KeyZ),
    (0x07, Code::KeyX),
    (0x08, Code::KeyC),
    (0x09, Code::KeyV),
    (0x0a, Code::IntlBackslash),
    (0x0b, Code::KeyB),
    (0x0c, Code::KeyQ),
    (0x0d, Code::KeyW),
    (0x0e, Code::KeyE),
    (0x0f, Code::KeyR),
    (0x10, Code::KeyY),
    (0x11, Code::KeyT),
    (0x12, Code::Digit1),
    (0x13, Code::Digit2),
    (0x14, Code::Digit3),
    (0x15, Code::Digit4),
    (0x16, Code::Digit6),
    (0x17, Code::Digit5),
    (0x18, Code::Equal),
    (0x19, Code::Digit9),
    (0x1a, Code::Digit7),
    (0x1b, Code::Minus),
    (0x1c, Code::Digit8),
    (0x1d, Code::Digit0),
    (0x1e, Code::BracketRight),
    (0x1f, Code::KeyO),
    (0x20, Code::KeyU),
    (0x21, Code::BracketLeft),
    (0x22, Code::KeyI),
    (0x23, Code::KeyP),
    (0x24, Code::Enter),
    (0x25, Code::KeyL),
    (0x26, Code::KeyJ),
    (0x27, Code::Quote),
    (0x28, Code::KeyK),
    (0x29, Code::Semicolon),
    (0x2a, Code::Backslash),
    (0x2b, Code::Comma),
    (0x2c, Code::Slash),
    (0x2d, Code::KeyN),
    (0x2e, Code::KeyM),
    (0x2f, Code::Period),
    (0x30, Code::Tab),
    (0x31, Code::Space),
    (0x32, Code::Backquote),
    (0x33, Code::Backspace),
    (0x35, Code::Escape),
    (0x40, Code::F17),
    (0x41, Code::NumpadDecimal),
    (0x43, Code::NumpadMultiply),
    (0x45, Code::NumpadAdd),
    (0x47, Code::NumLock),
    (0x4b, Code::NumpadDivide),
    (0x4c, Code::NumpadEnter),
    (0x4e, Code::NumpadSubtract),
    (0x4f, Code::F18),
    (0x50, Code::F19),
    (0x51, Code::NumpadEqual),
    (0x52, Code::Numpad0),
    (0x53, Code::Numpad1),
    (0x54, Code::Numpad2),
    (0x55, Code::Numpad3),
    (0x56, Code::Numpad4),
    (0x57, Code::Numpad5),
    (0x58, Code::Numpad6),
    (0x59, Code::Numpad7),
    (0x5a, Code::F20),
    (0x5b, Code::Numpad8),
    (0x5c, Code::Numpad9),
    (0x5d, Code::IntlYen),
    (0x5e, Code::IntlRo),
    (0x5f, Code::NumpadComma),
    (0x60, Code::F5),
    (0x61, Code::F6),
    (0x62, Code::F7),
    (0x63, Code::F3),
    (0x64, Code::F8),
    (0x65, Code::F9),
    (0x67, Code::F11),
    (0x69, Code::F13),
    (0x6a, Code::F16),
    (0x6b, Code::F14),
    (0x6d, Code::F10),
    (0x6f, Code::F12),
    (0x71, Code::F15),
    (0x73, Code::Home),
    (0x74, Code::PageUp),
    (0x75, Code::Delete),
    (0x76, Code::F4),
    (0x77, Code::End),
    (0x78, Code::F2),
    (0x79, Code::PageDown),
    (0x7a, Code::F1),
    (0x7b, Code::ArrowLeft),
    (0x7c, Code::ArrowRight),
    (0x7d, Code::ArrowDown),
    (0x7e, Code::ArrowUp),
];

fn code_for(key_code: u16) -> Option<Code> {
    KEYS.iter().find(|(k, _)| *k == key_code).map(|(_, c)| *c)
}

/// The label for a key: a symbol or name for keys that type nothing
/// visible, otherwise the character the layout gives it, upper-cased.
fn label(code: Code, character: &str) -> String {
    let named = match code {
        Code::Space => Some("Space"),
        Code::Enter => Some("↩"),
        Code::Tab => Some("⇥"),
        Code::Backspace => Some("⌫"),
        Code::Delete => Some("⌦"),
        Code::Escape => Some("⎋"),
        Code::ArrowLeft => Some("←"),
        Code::ArrowRight => Some("→"),
        Code::ArrowUp => Some("↑"),
        Code::ArrowDown => Some("↓"),
        Code::Home => Some("↖"),
        Code::End => Some("↘"),
        Code::PageUp => Some("⇞"),
        Code::PageDown => Some("⇟"),
        Code::NumLock => Some("Clear"),
        Code::NumpadEnter => Some("Keypad ↩"),
        _ => None,
    };
    if let Some(name) = named {
        return name.into();
    }
    let name = code.to_string();
    if is_function_key(code) {
        return name;
    }
    let typed = character.trim();
    let typed =
        (!typed.is_empty() && typed.chars().count() <= 2 && !typed.chars().any(char::is_control))
            .then(|| typed.to_uppercase());
    let base = typed.unwrap_or_else(|| fallback_label(&name));
    if name.starts_with("Numpad") {
        format!("Keypad {base}")
    } else {
        base
    }
}

/// A US-layout label from the key's name ("KeyA" → "A", "Minus" → "-").
fn fallback_label(name: &str) -> String {
    let name = name
        .strip_prefix("Numpad")
        .or_else(|| name.strip_prefix("Key"))
        .or_else(|| name.strip_prefix("Digit"))
        .unwrap_or(name);
    match name {
        "Minus" | "Subtract" => "-",
        "Equal" => "=",
        "BracketLeft" => "[",
        "BracketRight" => "]",
        "Backslash" | "IntlBackslash" => "\\",
        "Semicolon" => ";",
        "Quote" => "'",
        "Comma" => ",",
        "Period" | "Decimal" => ".",
        "Slash" | "Divide" => "/",
        "Backquote" => "`",
        "Multiply" => "*",
        "Add" => "+",
        other => other,
    }
    .to_string()
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

/// The saved shortcut, or the default when there is none or it is not
/// one Sift accepts.
pub fn load(dir: &Path) -> Shortcut {
    let Ok(text) = std::fs::read_to_string(state_path(dir)) else {
        return Shortcut::default();
    };
    let mut lines = text.lines();
    let Some(hotkey) = lines.next().and_then(|l| HotKey::from_str(l.trim()).ok()) else {
        tracing::warn!("the saved keyboard shortcut is not readable; using ⌥Space");
        return Shortcut::default();
    };
    let usable = KEYS.iter().any(|(_, c)| *c == hotkey.key)
        && (is_function_key(hotkey.key)
            || hotkey
                .mods
                .intersects(Modifiers::META | Modifiers::CONTROL | Modifiers::ALT));
    if !usable {
        tracing::warn!(%hotkey, "the saved keyboard shortcut is not usable; using ⌥Space");
        return Shortcut::default();
    }
    let saved = lines.next().map(str::trim).unwrap_or("");
    let key =
        if !saved.is_empty() && saved.chars().count() <= 12 && !saved.chars().any(char::is_control)
        {
            saved.to_string()
        } else {
            label(hotkey.key, "")
        };
    Shortcut { hotkey, key }
}

/// Saves the shortcut. The default is saved too, so the file always says
/// what was chosen last.
pub fn save(dir: &Path, shortcut: &Shortcut) -> Result<(), String> {
    let text = format!("{}\n{}\n", shortcut.hotkey.into_string(), shortcut.key);
    std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(state_path(dir), text))
        .map_err(|error| format!("cannot save the shortcut: {error}"))
}

/// The registered shortcut, owned by the app.
pub struct Hotkeys {
    manager: Option<GlobalHotKeyManager>,
    current: Shortcut,
    registered: bool,
    /// The id of the registered hotkey (0 for none), read by the event
    /// handler.
    active: Arc<AtomicU32>,
    dir: PathBuf,
}

impl Global for Hotkeys {}

/// Registers the saved shortcut (or Option+Space if that fails) and sends
/// the time of each press to `presses`.
pub fn install(dir: PathBuf, presses: UnboundedSender<Instant>, cx: &mut App) {
    let active = Arc::new(AtomicU32::new(0));
    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => {
            let active = active.clone();
            // Events arrive from a Carbon handler on the main thread.
            GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
                if event.state == HotKeyState::Pressed && event.id == active.load(Ordering::Relaxed)
                {
                    let _ = presses.unbounded_send(Instant::now());
                }
            }));
            Some(manager)
        }
        Err(error) => {
            tracing::error!(%error, "cannot set up keyboard shortcuts");
            None
        }
    };
    let mut hotkeys = Hotkeys {
        manager,
        current: load(&dir),
        registered: false,
        active,
        dir,
    };
    if let Err(error) = hotkeys.register()
        && !hotkeys.current.is_default()
    {
        tracing::warn!(%error, "falling back to ⌥Space");
        hotkeys.current = Shortcut::default();
        let _ = hotkeys.register();
    }
    cx.set_global(hotkeys);
}

impl Hotkeys {
    pub fn current(&self) -> &Shortcut {
        &self.current
    }

    /// Whether the shortcut works right now (false while recording, or if
    /// macOS refused it).
    pub fn is_registered(&self) -> bool {
        self.registered
    }

    fn register(&mut self) -> Result<(), String> {
        let shortcut = self.current.display();
        let manager = self
            .manager
            .as_ref()
            .ok_or("keyboard shortcuts are not available")?;
        match manager.register(self.current.hotkey) {
            Ok(()) => {
                self.registered = true;
                self.active
                    .store(self.current.hotkey.id(), Ordering::Relaxed);
                tracing::info!(shortcut, "registered the keyboard shortcut");
                Ok(())
            }
            Err(error) => {
                tracing::error!(shortcut, %error, "cannot register the keyboard shortcut");
                Err(format!("macOS did not accept {shortcut}. Choose another."))
            }
        }
    }

    fn unregister(&mut self) {
        if !self.registered {
            return;
        }
        self.active.store(0, Ordering::Relaxed);
        self.registered = false;
        if let Some(manager) = &self.manager
            && let Err(error) = manager.unregister(self.current.hotkey)
        {
            tracing::warn!(%error, "cannot unregister the keyboard shortcut");
        }
    }

    /// Turns the shortcut off while a new one is recorded, so pressing it
    /// reaches the recorder instead of toggling the panel.
    pub fn pause(&mut self) {
        self.unregister();
    }

    /// Turns the shortcut back on after [`pause`](Self::pause).
    pub fn resume(&mut self) {
        if !self.registered {
            let _ = self.register();
        }
    }

    /// Replaces the shortcut and saves it. If macOS refuses the new one,
    /// the previous one stays and the reason is returned.
    pub fn set(&mut self, shortcut: Shortcut) -> Result<(), String> {
        if shortcut == self.current {
            self.resume();
            return Ok(());
        }
        self.unregister();
        let previous = std::mem::replace(&mut self.current, shortcut);
        if let Err(reason) = self.register() {
            self.current = previous;
            let _ = self.register();
            return Err(reason);
        }
        save(&self.dir, &self.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key_code: u16, mods: &str, character: &str) -> KeyPress {
        KeyPress {
            key_code,
            command: mods.contains('⌘'),
            control: mods.contains('⌃'),
            option: mods.contains('⌥'),
            shift: mods.contains('⇧'),
            character: character.into(),
        }
    }

    fn shortcut(recorded: Recorded) -> Shortcut {
        match recorded {
            Recorded::Shortcut(s) => s,
            other => panic!("expected a shortcut, got {other:?}"),
        }
    }

    #[test]
    fn records_shortcuts_by_physical_key() {
        let s = shortcut(interpret(&press(0x31, "⌥", " ")));
        assert!(s.is_default());
        assert_eq!(s.display(), "⌥Space");
        assert_eq!(s.spoken(), "Option Space");

        // Modifiers in macOS order whatever was pressed.
        let s = shortcut(interpret(&press(0x28, "⌘⇧⌃", "k")));
        assert_eq!(s.parts(), ["⌃", "⇧", "⌘", "K"]);
        assert_eq!(
            s.hotkey,
            HotKey::from_str("shift+control+meta+KeyK").expect("parse")
        );

        // AZERTY: the key at the QWERTY "Q" position types "a". The label
        // follows the layout, the registration the physical key.
        let s = shortcut(interpret(&press(0x0c, "⌥", "a")));
        assert_eq!(s.display(), "⌥A");
        assert_eq!(s.hotkey.key, Code::KeyQ);

        // Function keys may stand alone; named keys get symbols.
        assert_eq!(shortcut(interpret(&press(0x60, "", ""))).display(), "F5");
        assert_eq!(shortcut(interpret(&press(0x7e, "⌃", ""))).display(), "⌃↑");
        assert_eq!(
            shortcut(interpret(&press(0x53, "⌥", "1"))).display(),
            "⌥Keypad 1"
        );
    }

    #[test]
    fn rejects_shortcuts_that_would_get_in_the_way() {
        assert_eq!(interpret(&press(ESCAPE, "", "\u{1b}")), Recorded::Cancel);
        // Escape with a modifier is a shortcut, not a cancel.
        assert!(matches!(
            interpret(&press(ESCAPE, "⌥", "\u{1b}")),
            Recorded::Shortcut(_)
        ));
        for plain in [
            press(0x00, "", "a"),
            press(0x00, "⇧", "a"),
            press(0x31, "", " "),
        ] {
            assert!(
                matches!(interpret(&plain), Recorded::Rejected(_)),
                "{plain:?}"
            );
        }
        // Caps Lock and media keys are not in the table.
        assert!(matches!(
            interpret(&press(0x39, "⌥", "")),
            Recorded::Rejected(_)
        ));
        assert!(matches!(
            interpret(&press(0x48, "⌥", "")),
            Recorded::Rejected(_)
        ));
    }

    #[test]
    fn labels_ignore_unprintable_or_odd_characters() {
        assert_eq!(label(Code::KeyK, "\u{10}"), "K");
        assert_eq!(label(Code::Minus, ""), "-");
        assert_eq!(label(Code::KeyE, "é"), "É");
        assert_eq!(label(Code::KeyA, "a long string"), "A");
    }

    #[test]
    fn the_key_table_has_one_code_per_key() {
        for (i, (k, c)) in KEYS.iter().enumerate() {
            assert!(
                KEYS[i + 1..].iter().all(|(k2, c2)| k2 != k && c2 != c),
                "{k:#x} {c:?}"
            );
            // Every key round-trips through the saved notation.
            let hotkey = HotKey::new(Some(Modifiers::ALT), *c);
            assert_eq!(HotKey::from_str(&hotkey.into_string()).ok(), Some(hotkey));
        }
    }

    #[test]
    fn saves_and_loads_the_shortcut() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load(dir.path()).is_default());
        let s = shortcut(interpret(&press(0x0c, "⌃⌥", "a")));
        save(dir.path(), &s).expect("save");
        assert_eq!(load(dir.path()), s);

        // Damaged, unusable or hostile files fall back to the default.
        for text in [
            "",
            "garbage\n",
            "KeyA\nA\n",            // no modifier
            "alt+CapsLock\nCaps\n", // not allowed
            "Escape\n⎋\n",          // Escape alone cancels recording
        ] {
            std::fs::write(state_path(dir.path()), text).expect("write");
            assert!(load(dir.path()).is_default(), "{text:?}");
        }
        // A bad label is replaced, the shortcut kept.
        std::fs::write(state_path(dir.path()), "alt+KeyK\n\u{7}bell\n").expect("write");
        assert_eq!(load(dir.path()).display(), "⌥K");
        std::fs::write(state_path(dir.path()), "alt+Escape\n\u{1b}[31m\n").expect("write");
        assert_eq!(load(dir.path()).display(), "⌥⎋");
    }

    #[test]
    fn warns_about_shortcuts_macos_uses() {
        let cmd_space = shortcut(interpret(&press(0x31, "⌘", " ")));
        assert!(cmd_space.system_conflict().is_some());
        let ctrl_space = shortcut(interpret(&press(0x31, "⌃", " ")));
        assert!(ctrl_space.system_conflict().is_some());
        assert!(Shortcut::default().system_conflict().is_none());
    }
}
