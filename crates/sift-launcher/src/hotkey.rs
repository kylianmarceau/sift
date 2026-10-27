//! The keyboard shortcuts ([`Slot`]): one shows and hides the panel, and
//! two optional ones open the panel on clipboard history, or paste the
//! newest clipboard entry and pop it.
//!
//! The panel's is Option+Space unless the user records another one in
//! Settings; the others have none until one is recorded. Each choice is
//! kept in a two-line file in the support directory (`hotkey`,
//! `clipboard-hotkey`, `paste-pop-hotkey`): the shortcut in `global-hotkey`'s notation
//! ("alt+Space"), then the key's label as the keyboard layout showed it
//! when recorded ("Space", "K", "É"). A missing or unreadable file means
//! the default, and an empty one means no shortcut.
//!
//! Shortcuts are registered by physical key, so they work the same on
//! every keyboard layout; the label is only for display.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::channel::mpsc::UnboundedSender;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use gpui::{App, Global};
use sift_macos::key_capture::KeyPress;

/// What a shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Shows and hides the panel.
    Panel,
    /// Opens the panel on clipboard history.
    Clipboard,
    /// Pastes the newest clipboard entry and pops it.
    Pop,
}

impl Slot {
    const ALL: [Slot; 3] = [Slot::Panel, Slot::Clipboard, Slot::Pop];

    fn index(self) -> usize {
        self as usize
    }

    fn file(self) -> &'static str {
        match self {
            Slot::Panel => "hotkey",
            Slot::Clipboard => "clipboard-hotkey",
            Slot::Pop => "paste-pop-hotkey",
        }
    }

    /// What it does, after "That shortcut already …".
    fn purpose(self) -> &'static str {
        match self {
            Slot::Panel => "shows the panel",
            Slot::Clipboard => "opens clipboard history",
            Slot::Pop => "pastes and pops",
        }
    }

    /// The shortcut's name in an error: "Sift's launcher shortcut".
    fn name(self) -> &'static str {
        match self {
            Slot::Panel => "launcher",
            Slot::Clipboard => "clipboard",
            Slot::Pop => "paste-and-pop",
        }
    }
}

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

    /// Whether the two shortcuts press the same physical key.
    pub fn same_key(&self, other: &Shortcut) -> bool {
        self.hotkey == other.hotkey
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

fn state_path(dir: &Path, slot: Slot) -> PathBuf {
    dir.join(slot.file())
}

/// The panel's saved shortcut, or the default when there is none or it
/// is not one Sift accepts.
pub fn load(dir: &Path) -> Shortcut {
    load_slot(dir, Slot::Panel).unwrap_or_else(|| {
        tracing::info!("using ⌥Space to show the panel");
        Shortcut::default()
    })
}

/// A slot's saved shortcut; `None` when there is none (or an empty file)
/// or it is not one Sift accepts.
pub fn load_slot(dir: &Path, slot: Slot) -> Option<Shortcut> {
    let text = std::fs::read_to_string(state_path(dir, slot)).ok()?;
    let mut lines = text.lines();
    let first = lines.next().map(str::trim).unwrap_or("");
    if first.is_empty() {
        return None;
    }
    let Ok(hotkey) = HotKey::from_str(first) else {
        tracing::warn!(?slot, "the saved keyboard shortcut is not readable");
        return None;
    };
    let usable = KEYS.iter().any(|(_, c)| *c == hotkey.key)
        && (is_function_key(hotkey.key)
            || hotkey
                .mods
                .intersects(Modifiers::META | Modifiers::CONTROL | Modifiers::ALT));
    if !usable {
        tracing::warn!(?slot, %hotkey, "the saved keyboard shortcut is not usable");
        return None;
    }
    let saved = lines.next().map(str::trim).unwrap_or("");
    let key =
        if !saved.is_empty() && saved.chars().count() <= 12 && !saved.chars().any(char::is_control)
        {
            saved.to_string()
        } else {
            label(hotkey.key, "")
        };
    Some(Shortcut { hotkey, key })
}

/// Saves a slot's shortcut; `None` saves an empty file (no shortcut). The
/// panel's default is saved too, so the file always says what was chosen
/// last.
pub fn save_slot(dir: &Path, slot: Slot, shortcut: Option<&Shortcut>) -> Result<(), String> {
    let text = shortcut
        .map(|s| format!("{}\n{}\n", s.hotkey.into_string(), s.key))
        .unwrap_or_default();
    std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(state_path(dir, slot), text))
        .map_err(|error| format!("cannot save the shortcut: {error}"))
}

/// One slot's shortcut and whether it is registered.
#[derive(Default)]
struct Binding {
    shortcut: Option<Shortcut>,
    registered: bool,
    /// The id of the registered hotkey (0 for none), read by the event
    /// handler.
    active: Arc<AtomicU32>,
}

/// The registered shortcuts, owned by the app.
pub struct Hotkeys {
    manager: Option<GlobalHotKeyManager>,
    /// By [`Slot::index`].
    bindings: [Binding; 3],
    dir: PathBuf,
    window_keys: Vec<HotKey>,
    window_routes: Arc<Mutex<Option<WindowRoutes>>>,
    window_signature: Option<(
        String,
        bool,
        std::collections::BTreeMap<String, Vec<String>>,
    )>,
    paused: Arc<AtomicBool>,
}

struct WindowRoutes {
    controller: Option<sift_macos::wm::Controller>,
    commands: std::collections::HashMap<u32, sift_wm::Command>,
}

impl Global for Hotkeys {}

/// Registers the saved shortcuts (Option+Space for the panel if its own
/// fails) and sends each press, with its time, to `presses`.
pub fn install(dir: PathBuf, presses: UnboundedSender<(Slot, Instant)>, cx: &mut App) {
    let bindings: [Binding; 3] = Default::default();
    let window_routes: Arc<Mutex<Option<WindowRoutes>>> = Arc::default();
    let paused = Arc::new(AtomicBool::new(false));
    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => {
            let active: Vec<(Slot, Arc<AtomicU32>)> = Slot::ALL
                .iter()
                .map(|&slot| (slot, bindings[slot.index()].active.clone()))
                .collect();
            let routes = window_routes.clone();
            let paused = paused.clone();
            // Events arrive from a Carbon handler on the main thread.
            GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
                if event.state != HotKeyState::Pressed || paused.load(Ordering::Relaxed) {
                    return;
                }
                if let Some((slot, _)) = active
                    .iter()
                    .find(|(_, id)| event.id == id.load(Ordering::Relaxed))
                {
                    let _ = presses.unbounded_send((*slot, Instant::now()));
                } else if let Some(routes) = &*routes.lock().unwrap_or_else(|e| e.into_inner()) {
                    if let Some(command) = routes.commands.get(&event.id) {
                        let result = match &routes.controller {
                            Some(controller) if controller.snapshot().enabled => controller.execute(command.clone()),
                            Some(_) => Err("Window controls are paused. Resume them from Sift's menu.".into()),
                            None => Err("Window controls are unavailable. Grant permission for this Sift app, then choose Retry Window Controls.".into()),
                        };
                        if let Err(error) = result {
                            tracing::warn!(%error, "window shortcut blocked");
                        }
                    }
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
        bindings,
        dir,
        window_keys: Vec::new(),
        window_routes,
        window_signature: None,
        paused,
    };
    hotkeys.bindings[Slot::Panel.index()].shortcut = Some(load(&hotkeys.dir));
    if let Err(error) = hotkeys.register(Slot::Panel)
        && hotkeys
            .current(Slot::Panel)
            .is_some_and(|s| !s.is_default())
    {
        tracing::warn!(%error, "falling back to ⌥Space");
        hotkeys.bindings[Slot::Panel.index()].shortcut = Some(Shortcut::default());
        let _ = hotkeys.register(Slot::Panel);
    }
    // An optional shortcut that one before it (or the panel's fallback)
    // took is dropped.
    for slot in [Slot::Clipboard, Slot::Pop] {
        let shortcut = load_slot(&hotkeys.dir, slot).filter(|s| {
            !Slot::ALL
                .iter()
                .any(|&o| hotkeys.current(o).is_some_and(|c| c.hotkey == s.hotkey))
        });
        hotkeys.bindings[slot.index()].shortcut = shortcut;
        let _ = hotkeys.register(slot);
    }
    cx.set_global(hotkeys);
}

impl Hotkeys {
    /// Register the current WM binding mode on the main thread. The same
    /// signature is a no-op, so window geometry updates never touch Carbon.
    pub fn configure_windows(
        &mut self,
        config: &sift_wm::Config,
        mode: &str,
        controller: Option<sift_macos::wm::Controller>,
    ) -> Result<(), String> {
        // Leave system shortcuts alone when Sift cannot manage windows.
        if !config.enabled || controller.as_ref().is_none_or(|c| !c.snapshot().enabled) {
            if let Some(manager) = &self.manager {
                for key in self.window_keys.drain(..) {
                    let _ = manager.unregister(key);
                }
            }
            self.window_signature = None;
            *self.window_routes.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Ok(());
        }
        let bindings = config.bindings_for_mode(mode).map_err(|e| e.to_string())?;
        let signature = (mode.to_owned(), controller.is_some(), bindings.clone());
        if self.window_signature.as_ref() == Some(&signature) {
            if let Some(routes) = self
                .window_routes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
            {
                routes.controller = controller;
            }
            return Ok(());
        }
        let mut keys = Vec::new();
        let mut commands = std::collections::HashMap::new();
        for (text, values) in &bindings {
            let key = window_mode_hotkey(text, mode)?;
            if let Some(slot) = Slot::ALL
                .into_iter()
                .find(|&slot| self.current(slot).is_some_and(|s| s.hotkey == key))
            {
                return Err(format!(
                    "{text} conflicts with Sift's {} shortcut",
                    slot.name()
                ));
            }
            let sequence = values
                .iter()
                .map(|s| sift_wm::parse(s).map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            if commands
                .insert(key.id(), sift_wm::Command::Sequence(sequence))
                .is_some()
            {
                return Err(format!("duplicate physical window shortcut: {text}"));
            }
            keys.push(key);
        }
        if mode != "main" {
            let escape = HotKey::new(None, Code::Escape);
            if let std::collections::hash_map::Entry::Vacant(entry) = commands.entry(escape.id()) {
                entry.insert(sift_wm::Command::Mode("main".into()));
                keys.push(escape);
            }
        }
        let manager = self
            .manager
            .as_ref()
            .ok_or("keyboard shortcuts are unavailable")?;
        let previous_keys = self.window_keys.clone();
        for key in &previous_keys {
            let _ = manager.unregister(*key);
        }
        let mut registered = Vec::new();
        if !self.paused.load(Ordering::Relaxed) {
            for key in &keys {
                if let Err(error) = manager.register(*key) {
                    for key in registered {
                        let _ = manager.unregister(key);
                    }
                    for key in &previous_keys {
                        let _ = manager.register(*key);
                    }
                    return Err(format!("could not register window shortcut: {error}"));
                }
                registered.push(*key);
            }
        }
        self.window_keys = keys;
        self.window_signature = Some(signature);
        tracing::info!(
            mode,
            shortcuts = self.window_keys.len(),
            controls_running = controller.is_some(),
            "window shortcuts configured"
        );
        *self.window_routes.lock().unwrap_or_else(|e| e.into_inner()) = Some(WindowRoutes {
            controller,
            commands,
        });
        Ok(())
    }

    /// The slot's shortcut; the panel always has one.
    pub fn current(&self, slot: Slot) -> Option<&Shortcut> {
        self.bindings[slot.index()].shortcut.as_ref()
    }

    /// Whether the slot's shortcut works right now (false while recording,
    /// or if macOS refused it).
    pub fn is_registered(&self, slot: Slot) -> bool {
        self.bindings[slot.index()].registered
    }

    fn register(&mut self, slot: Slot) -> Result<(), String> {
        let binding = &mut self.bindings[slot.index()];
        let Some(current) = &binding.shortcut else {
            return Ok(());
        };
        let shortcut = current.display();
        let manager = self
            .manager
            .as_ref()
            .ok_or("keyboard shortcuts are not available")?;
        match manager.register(current.hotkey) {
            Ok(()) => {
                binding.registered = true;
                binding.active.store(current.hotkey.id(), Ordering::Relaxed);
                tracing::info!(?slot, shortcut, "registered the keyboard shortcut");
                Ok(())
            }
            Err(error) => {
                tracing::error!(?slot, shortcut, %error, "cannot register the keyboard shortcut");
                Err(format!("macOS did not accept {shortcut}. Choose another."))
            }
        }
    }

    fn unregister(&mut self, slot: Slot) {
        let binding = &mut self.bindings[slot.index()];
        if !binding.registered {
            return;
        }
        binding.active.store(0, Ordering::Relaxed);
        binding.registered = false;
        if let (Some(manager), Some(current)) = (&self.manager, &binding.shortcut)
            && let Err(error) = manager.unregister(current.hotkey)
        {
            tracing::warn!(?slot, %error, "cannot unregister the keyboard shortcut");
        }
    }

    /// Turns the shortcuts off while a new one is recorded, so pressing
    /// one reaches the recorder instead of opening the panel.
    pub fn pause(&mut self) {
        self.paused.store(true, Ordering::Relaxed);
        if let Some(manager) = &self.manager {
            for key in &self.window_keys {
                let _ = manager.unregister(*key);
            }
        }
        for slot in Slot::ALL {
            self.unregister(slot);
        }
    }

    /// Turns the shortcuts back on after [`pause`](Self::pause).
    pub fn resume(&mut self) {
        let was_paused = self.paused.swap(false, Ordering::Relaxed);
        for slot in Slot::ALL {
            if !self.bindings[slot.index()].registered {
                let _ = self.register(slot);
            }
        }
        if was_paused {
            if let Some(manager) = &self.manager {
                for key in &self.window_keys {
                    let _ = manager.register(*key);
                }
            }
        }
    }

    /// Replaces the slot's shortcut (`None` removes it, except for the
    /// panel) and saves it. If another slot or a window command uses it,
    /// or macOS refuses it, the previous one stays and the reason is
    /// returned.
    pub fn set(&mut self, slot: Slot, shortcut: Option<Shortcut>) -> Result<(), String> {
        if slot == Slot::Panel && shortcut.is_none() {
            return Err("The panel needs a shortcut.".into());
        }
        if let Some(new) = &shortcut {
            if self.window_keys.contains(&new.hotkey) {
                return Err("This shortcut is assigned to a window-management command.".into());
            }
            if let Some(other) = Slot::ALL
                .into_iter()
                .filter(|&s| s != slot)
                .find(|&o| self.current(o).is_some_and(|s| s.hotkey == new.hotkey))
            {
                return Err(format!("That shortcut already {}.", other.purpose()));
            }
        }
        if shortcut.as_ref() == self.current(slot) {
            self.resume();
            return Ok(());
        }
        self.unregister(slot);
        let previous = std::mem::replace(&mut self.bindings[slot.index()].shortcut, shortcut);
        if let Err(reason) = self.register(slot) {
            self.bindings[slot.index()].shortcut = previous;
            let _ = self.register(slot);
            return Err(reason);
        }
        save_slot(&self.dir, slot, self.current(slot))
    }
}

pub(crate) fn window_mode_hotkey(text: &str, mode: &str) -> Result<HotKey, String> {
    let text = text.replace('-', "+");
    let key = HotKey::from_str(&text).map_err(|e| e.to_string())?;
    if !KEYS.iter().any(|(_, code)| *code == key.key)
        || (mode == "main"
            && !is_function_key(key.key)
            && !key
                .mods
                .intersects(Modifiers::ALT | Modifiers::CONTROL | Modifiers::META))
    {
        return Err(
            "Main-mode window shortcuts need Command, Control or Option, or a function key.".into(),
        );
    }
    Ok(key)
}

/// The shortcut a window-management binding key stands for, with a label
/// for display; `None` when the binding is not usable in the main mode.
pub fn from_binding(text: &str) -> Option<Shortcut> {
    let hotkey = window_mode_hotkey(text, "main").ok()?;
    Some(Shortcut {
        hotkey,
        key: label(hotkey.key, ""),
    })
}

/// The binding key Sift writes to config.toml for `shortcut`, in the same
/// notation `[window_manager.bindings]` uses.
pub fn binding_text(shortcut: &Shortcut) -> String {
    shortcut.hotkey.into_string()
}

/// Which launcher slot's shortcut would `shortcut` collide with, if any.
pub fn slot_conflict(hotkeys: &Hotkeys, shortcut: &Shortcut) -> Option<&'static str> {
    Slot::ALL
        .into_iter()
        .find(|&slot| {
            hotkeys
                .current(slot)
                .is_some_and(|s| s.hotkey == shortcut.hotkey)
        })
        .map(|slot| slot.purpose())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_window_shortcuts_are_valid_and_secondary_modes_can_use_bare_keys() {
        for shortcut in sift_wm::Config::default().bindings["main"].keys() {
            window_mode_hotkey(shortcut, "main").expect("default binding");
        }
        assert!(window_mode_hotkey("h", "main").is_err());
        assert!(window_mode_hotkey("h", "resize").is_ok());
        assert!(window_mode_hotkey("escape", "resize").is_ok());
        assert!(window_mode_hotkey("ctrl-alt-h", "main").is_ok());
    }

    #[test]
    fn binding_keys_round_trip_between_text_and_display() {
        // Every default binding converts to a displayable shortcut, and
        // the written binding form parses back to the same key.
        for key in sift_wm::Config::default().bindings["main"].keys() {
            let shortcut = from_binding(key).expect("usable default binding");
            let text = binding_text(&shortcut);
            let again = from_binding(&text).expect("round trip");
            assert!(shortcut.same_key(&again), "{key} -> {text}");
        }
        assert!(from_binding("h").is_none());
        assert!(from_binding("not a key").is_none());
    }

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
        save_slot(dir.path(), Slot::Panel, Some(&s)).expect("save");
        assert_eq!(load(dir.path()), s);

        // Damaged, unusable or hostile files fall back to the default.
        for text in [
            "",
            "garbage\n",
            "KeyA\nA\n",            // no modifier
            "alt+CapsLock\nCaps\n", // not allowed
            "Escape\n⎋\n",          // Escape alone cancels recording
        ] {
            std::fs::write(state_path(dir.path(), Slot::Panel), text).expect("write");
            assert!(load(dir.path()).is_default(), "{text:?}");
        }
        // A bad label is replaced, the shortcut kept.
        std::fs::write(state_path(dir.path(), Slot::Panel), "alt+KeyK\n\u{7}bell\n")
            .expect("write");
        assert_eq!(load(dir.path()).display(), "⌥K");
        std::fs::write(
            state_path(dir.path(), Slot::Panel),
            "alt+Escape\n\u{1b}[31m\n",
        )
        .expect("write");
        assert_eq!(load(dir.path()).display(), "⌥⎋");
    }

    #[test]
    fn the_clipboard_shortcut_is_optional_and_saved_apart() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(load_slot(dir.path(), Slot::Clipboard), None);
        let s = shortcut(interpret(&press(0x09, "⌃⌥", "v")));
        save_slot(dir.path(), Slot::Clipboard, Some(&s)).expect("save");
        assert_eq!(load_slot(dir.path(), Slot::Clipboard), Some(s));
        // The panel keeps its own.
        assert!(load(dir.path()).is_default());
        // Removing it leaves an empty file, which means none.
        save_slot(dir.path(), Slot::Clipboard, None).expect("save");
        assert_eq!(load_slot(dir.path(), Slot::Clipboard), None);
        assert_eq!(
            std::fs::read_to_string(state_path(dir.path(), Slot::Clipboard)).expect("read"),
            ""
        );
    }

    #[test]
    fn each_slot_is_saved_in_its_own_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clip = shortcut(interpret(&press(0x09, "⌃⌥", "v")));
        let pop = shortcut(interpret(&press(0x09, "⌃⌥⇧", "v")));
        save_slot(dir.path(), Slot::Clipboard, Some(&clip)).expect("save");
        save_slot(dir.path(), Slot::Pop, Some(&pop)).expect("save");
        assert_eq!(load_slot(dir.path(), Slot::Clipboard), Some(clip));
        assert_eq!(load_slot(dir.path(), Slot::Pop), Some(pop));
        assert!(load(dir.path()).is_default());
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
