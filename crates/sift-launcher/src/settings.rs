//! The settings window: the keyboard shortcut, Open at Login, updates,
//! clipboard history, and which folders the file search covers.
//!
//! The update and clipboard options live in `config.toml`. Changing them
//! here edits that file in place (see `sift_state::Setting`), so the file
//! and this window always agree.
//!
//! It shows each indexed folder with its state and size, and sends changes
//! to the index service. Folders are chosen with the standard macOS folder
//! picker, which is also how macOS grants access to protected locations
//! such as Documents.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{
    AnyElement, App, Bounds, Context, Entity, FocusHandle, Focusable, FontWeight, Global,
    SharedString, Subscription, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowKind,
    WindowOptions, actions, div, prelude::*, px, size, svg,
};
use sift_files::{Command, RootState, RootStatus};
use sift_indexer::IndexOptions;

use crate::hotkey::{self, Hotkeys, Recorded, Shortcut, Slot};
use crate::launcher::{
    IndexStatusChanged, Launcher, UpdateStateChanged, UserDataChanged, WindowManagerChanged,
};
use crate::theme::Theme;
use crate::updates::UpdateState;
use sift_macos::a11y::{self, Urgency};
use sift_macos::key_capture::{KeyCapture, KeyPress};
use sift_macos::login_item::{self, Status as LoginStatus};
use sift_state::Setting;

/// History sizes offered in the settings window. Another value set in
/// config.toml is shown as an extra choice.
const CLIP_SIZES: &[usize] = &[50, 200, 500, 1000];

/// What a recording will change: a launcher shortcut or a
/// window-management binding (by its key in `config.toml`).
#[derive(Clone, PartialEq, Eq)]
enum Recording {
    Slot(Slot),
    Binding(String),
}

/// A human description of what a binding's commands do, for the settings
/// list. Falls back to the command text itself.
fn describe_commands(commands: Vec<String>) -> String {
    let text = commands.first().map(String::as_str).unwrap_or("");
    let mut words = text.split_whitespace();
    let described = match words.next() {
        Some("workspace") => words.next().map(|n| format!("Switch to Workspace {n}")),
        Some("move-node-to-workspace") => words
            .next()
            .map(|n| format!("Move the Window to Workspace {n}")),
        Some("workspace-back-and-forth") => Some("Switch to the Previous Workspace".into()),
        Some("focus") => match words.next() {
            Some("left") => Some("Focus the Window to the Left".into()),
            Some("down") => Some("Focus the Window Below".into()),
            Some("up") => Some("Focus the Window Above".into()),
            Some("right") => Some("Focus the Window to the Right".into()),
            Some("--target") => words.next().map(|t| format!("Focus Target {t}")),
            _ => None,
        },
        Some("move") => match words.next() {
            Some("left") => Some("Move the Window Left".into()),
            Some("down") => Some("Move the Window Down".into()),
            Some("up") => Some("Move the Window Up".into()),
            Some("right") => Some("Move the Window Right".into()),
            _ => None,
        },
        Some("snap") => words.next().map(|direction| match direction {
            "left" => "Snap the Window to the Left Half".into(),
            "right" => "Snap the Window to the Right Half".into(),
            "center" => "Center the Window".into(),
            "maximize" => "Maximize the Window".into(),
            "restore" => "Restore the Window's Size".into(),
            other => format!("Snap {other}"),
        }),
        Some("fullscreen") => Some("Toggle Fullscreen".into()),
        Some("layout") => Some("Change the Layout".into()),
        Some("resize") => Some("Resize the Window".into()),
        Some("balance") => Some("Balance the Layout".into()),
        _ => None,
    };
    described.unwrap_or_else(|| text.to_string())
}

actions!(settings, [CloseSettings]);

/// The open settings window, so a second request focuses it instead of
/// opening another.
struct OpenSettings(WindowHandle<Settings>);

impl Global for OpenSettings {}

/// Whether the settings window is open.
pub fn is_open(cx: &App) -> bool {
    cx.try_global::<OpenSettings>().is_some_and(|open| {
        let id = open.0.window_id();
        cx.windows().iter().any(|w| w.window_id() == id)
    })
}

/// Opens the settings window, or brings the open one to the front.
///
/// Must not run inside an update of the launcher: the new window draws
/// immediately, and drawing reads the launcher (see `Launcher::run_command`).
pub fn open(launcher: Entity<Launcher>, cx: &mut App) {
    // Activation re-enters GPUI through AppKit callbacks, so it runs after
    // the current update (see `Panel::set_height` for the same issue).
    let activate = || {
        sift_macos::activate_app();
    };
    if let Some(open) = cx.try_global::<OpenSettings>().map(|open| open.0)
        && open
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        cx.spawn(async move |_| activate()).detach();
        return;
    }
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Sift Settings".into()),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(680.), px(720.)),
            cx,
        ))),
        kind: WindowKind::Normal,
        focus: true,
        show: true,
        is_resizable: true,
        app_id: Some("io.github.kylianmarceau.sift".into()),
        ..Default::default()
    };
    match cx.open_window(options, |window, cx| {
        cx.new(|cx| Settings::new(launcher, window, cx))
    }) {
        Ok(handle) => {
            tracing::info!("settings window opened");
            cx.set_global(OpenSettings(handle));
            cx.spawn(async move |_| activate()).detach();
        }
        Err(error) => tracing::error!(%error, "cannot open the settings window"),
    }
}

/// Results of the (asynchronous) folder picker.
enum Picked {
    AddRoot(PathBuf),
    Exclude { root: PathBuf, path: PathBuf },
}

pub struct Settings {
    launcher: Entity<Launcher>,
    focus_handle: FocusHandle,
    picked: UnboundedSender<Picked>,
    message: Option<SharedString>,
    home: Option<PathBuf>,
    /// Open at Login, read when the window opens or becomes active (it can
    /// also be changed in System Settings).
    login: LoginStatus,
    /// While a new shortcut is being recorded: for which slot or binding,
    /// and the capture of key presses.
    recording: Option<(Recording, KeyCapture)>,
    /// A note under a shortcut, and whether it reports a problem.
    shortcut_note: Option<(Recording, SharedString, bool)>,
    /// Clear History was clicked once and waits for confirmation.
    confirm_clear: bool,
    _subscriptions: Vec<Subscription>,
}

impl Settings {
    fn new(launcher: Entity<Launcher>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (picked, mut picks) = unbounded::<Picked>();
        cx.spawn(async move |this, cx| {
            while let Some(pick) = picks.next().await {
                if this
                    .update(cx, |this, cx| this.on_picked(pick, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let login = login_item::status();
        tracing::info!(?login, "open at login status");
        Self {
            _subscriptions: vec![
                cx.subscribe(&launcher, |_, _, _: &IndexStatusChanged, cx| cx.notify()),
                cx.subscribe(&launcher, |_, _, _: &UpdateStateChanged, cx| cx.notify()),
                cx.subscribe(&launcher, |_, _, _: &UserDataChanged, cx| cx.notify()),
                cx.subscribe(&launcher, |_, _, _: &WindowManagerChanged, cx| cx.notify()),
                cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
                cx.observe_window_activation(window, |this, window, cx| {
                    if window.is_window_active() {
                        this.login = login_item::status();
                        // Picks up hand edits to config.toml (one `stat`).
                        this.launcher.update(cx, |launcher, cx| {
                            launcher.reload_config(cx);
                            launcher.retry_window_management(cx);
                        });
                        cx.notify();
                    } else {
                        this.confirm_clear = false;
                        // Key presses in other apps never reach the
                        // recorder, so stop rather than leave the shortcut
                        // switched off.
                        this.stop_recording(cx);
                    }
                }),
                // Closing the window mid-recording must not leave the
                // shortcut switched off.
                cx.on_release(|this, cx| {
                    if this.recording.take().is_some() {
                        resume_shortcut(cx);
                    }
                }),
            ],
            launcher,
            focus_handle,
            picked,
            message: None,
            home: std::env::home_dir(),
            login,
            recording: None,
            shortcut_note: None,
            confirm_clear: false,
        }
    }

    /// Saves an option to config.toml; a problem is shown in the banner.
    fn change(&mut self, setting: Setting, cx: &mut Context<Self>) {
        let result = self
            .launcher
            .update(cx, |launcher, cx| launcher.change_setting(setting, cx));
        match result {
            Ok(()) => self.message = None,
            Err(reason) => {
                tracing::warn!(%reason, "cannot save setting");
                speak(&reason);
                self.message = Some(reason.into());
            }
        }
        cx.notify();
    }

    fn open_config_file(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.launcher.read(cx).open_config_file() {
            self.message = Some(error.to_string().into());
            cx.notify();
        }
    }

    /// Starts recording a new shortcut for `target`: the next key press in
    /// Sift's windows becomes it. The current shortcuts are off meanwhile,
    /// so they can be pressed too.
    fn start_recording(&mut self, target: Recording, cx: &mut Context<Self>) {
        if self.recording.is_some() {
            return;
        }
        let (tx, mut presses) = unbounded::<KeyPress>();
        let Some(capture) = KeyCapture::start(move |press| {
            let _ = tx.unbounded_send(press);
        }) else {
            self.shortcut_note = Some((target, "Cannot read the keyboard right now.".into(), true));
            cx.notify();
            return;
        };
        if let Some(hotkeys) = cx.try_global::<Hotkeys>().is_some().then_some(()) {
            let () = hotkeys;
            cx.update_global::<Hotkeys, _>(|hotkeys, _| hotkeys.pause());
        }
        self.recording = Some((target, capture));
        self.shortcut_note = None;
        speak("Recording a shortcut. Press the new shortcut, or Escape to cancel.");
        // The task ends when the capture is dropped, which drops the sender.
        cx.spawn(async move |this, cx| {
            while let Some(press) = presses.next().await {
                if this
                    .update(cx, |this, cx| this.on_key_press(press, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Stops recording without changing the shortcut.
    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording.take().is_some() {
            resume_shortcut(cx);
            self.shortcut_note = None;
            cx.notify();
        }
    }

    fn on_key_press(&mut self, press: KeyPress, cx: &mut Context<Self>) {
        let Some((target, _)) = &self.recording else {
            return;
        };
        let target = target.clone();
        match hotkey::interpret(&press) {
            Recorded::Cancel => {
                self.stop_recording(cx);
                speak("Cancelled.");
            }
            Recorded::Rejected(reason) => {
                self.shortcut_note = Some((target, reason.into(), true));
                speak(reason);
                cx.notify();
            }
            Recorded::Shortcut(shortcut) => {
                // Dropping the capture before applying resumes the other
                // shortcuts.
                self.recording = None;
                match target {
                    Recording::Slot(slot) => self.use_shortcut(slot, Some(shortcut), cx),
                    Recording::Binding(key) => self.use_binding(&key, shortcut, cx),
                }
            }
        }
    }

    /// Makes `shortcut` the one for `slot`; `None` removes it.
    fn use_shortcut(&mut self, slot: Slot, shortcut: Option<Shortcut>, cx: &mut Context<Self>) {
        let result = if cx.try_global::<Hotkeys>().is_some() {
            cx.update_global::<Hotkeys, _>(|hotkeys, _| {
                let result = hotkeys.set(slot, shortcut.clone());
                // On a refusal, turn the others back on too.
                hotkeys.resume();
                result
            })
        } else {
            Err("Keyboard shortcuts are not available.".to_string())
        };
        match (result, shortcut) {
            (Ok(()), Some(shortcut)) => {
                tracing::info!(
                    ?slot,
                    shortcut = shortcut.display(),
                    "keyboard shortcut changed"
                );
                speak(&format!("Shortcut set to {}.", shortcut.spoken()));
                self.shortcut_note = shortcut
                    .system_conflict()
                    .map(|note| (Recording::Slot(slot), note.into(), false));
                if slot == Slot::Pop && !sift_macos::wm::trusted() {
                    // Apple's prompt, after the user chose to use it.
                    sift_macos::wm::request_accessibility();
                }
            }
            (Ok(()), None) => {
                tracing::info!(?slot, "keyboard shortcut removed");
                speak("Shortcut removed.");
                self.shortcut_note = None;
            }
            (Err(reason), _) => {
                speak(&reason);
                self.shortcut_note = Some((Recording::Slot(slot), reason.into(), true));
            }
        }
        cx.notify();
    }

    /// Makes `shortcut` the key for the window-management binding `key`,
    /// replacing its previous key, and saves the mode's binding table to
    /// config.toml.
    fn use_binding(&mut self, key: &str, shortcut: Shortcut, cx: &mut Context<Self>) {
        match self.apply_binding_change(key, Some(&shortcut), cx) {
            Ok(()) => {
                speak(&format!("Shortcut set to {}.", shortcut.spoken()));
            }
            Err(reason) => {
                speak(&reason);
                self.shortcut_note =
                    Some((Recording::Binding(key.to_string()), reason.into(), true));
            }
        }
        cx.notify();
    }

    /// Removes the window-management binding `key`, or resets it to Sift's
    /// default when there is one.
    fn remove_binding(&mut self, key: &str, cx: &mut Context<Self>) {
        match self.apply_binding_change(key, None, cx) {
            Ok(()) => speak("Shortcut removed."),
            Err(reason) => {
                speak(&reason);
                self.shortcut_note =
                    Some((Recording::Binding(key.to_string()), reason.into(), true));
            }
        }
        cx.notify();
    }

    /// Writes a change to the `main` binding mode and reloads. A key's
    /// whole mode table is saved, because a bindings table in config.toml
    /// replaces the default shortcuts entirely.
    fn apply_binding_change(
        &mut self,
        key: &str,
        shortcut: Option<&Shortcut>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let launcher = self.launcher.read(cx);
        let config = launcher.user.window_manager_config().clone();
        let path = launcher.user.config_path().to_path_buf();
        let mode = config
            .bindings
            .get("main")
            .ok_or("The main binding mode is missing.")?;
        let commands = mode
            .get(key)
            .cloned()
            .ok_or("That shortcut is no longer configured. Reopen Settings.")?;
        let mut bindings = mode.clone();
        if let Some(shortcut) = shortcut {
            let text = hotkey::binding_text(shortcut);
            // Main-mode rules: the recorder already requires ⌘, ⌃ or ⌥
            // (or a function key); this keeps hand-written keys honest too.
            if hotkey::from_binding(&text).is_none() {
                return Err(
                    "Main-mode window shortcuts need Command, Control or Option, or a \
                     function key."
                        .into(),
                );
            }
            if let Some(other) = mode
                .keys()
                .filter(|k| k.as_str() != key)
                .find(|k| hotkey::from_binding(k).is_some_and(|s| s.same_key(shortcut)))
            {
                return Err(format!(
                    "{} is already used by {}.",
                    shortcut.display(),
                    describe_commands(mode.get(other).cloned().unwrap_or_default())
                ));
            }
            if let Some(hotkeys) = cx.try_global::<Hotkeys>()
                && let Some(purpose) = hotkey::slot_conflict(hotkeys, shortcut)
            {
                return Err(format!(
                    "{} is already used by Sift's shortcut, which {}.",
                    shortcut.display(),
                    purpose
                ));
            }
            bindings.remove(key);
            bindings.insert(text.clone(), commands.clone());
        } else {
            // A default shortcut comes back when it is reset; a custom one
            // is deleted.
            let default = sift_wm::Config::default();
            match default.bindings.get("main").and_then(|m| m.get(key)) {
                Some(default_commands) => {
                    bindings.insert(key.to_string(), default_commands.clone());
                }
                None => {
                    bindings.remove(key);
                }
            }
        }
        sift_state::Config::save_bindings(&path, "main", &bindings)?;
        self.launcher
            .update(cx, |launcher, cx| launcher.reload_config(cx));
        Ok(())
    }

    fn send(&self, command: Command, cx: &App) {
        match self.launcher.read(cx).index_service() {
            Some(service) => service.send(command),
            None => tracing::error!("the file index service is not running"),
        }
    }

    fn pick_folder(
        &self,
        prompt: &'static str,
        message: &'static str,
        start: Option<PathBuf>,
        cx: &mut Context<Self>,
        wrap: impl Fn(PathBuf) -> Picked + 'static,
    ) {
        let tx = self.picked.clone();
        cx.spawn(async move |_, _| {
            sift_macos::choose_folder(prompt, message, start.as_deref(), move |path| {
                if let Some(path) = path {
                    let _ = tx.unbounded_send(wrap(path));
                }
            });
        })
        .detach();
    }

    fn add_folder(&mut self, cx: &mut Context<Self>) {
        let start = self.home.clone();
        self.pick_folder(
            "Add",
            "Choose a folder for Sift to search. Only names and dates are indexed.",
            start,
            cx,
            Picked::AddRoot,
        );
    }

    fn exclude_folder(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let start = Some(root.clone());
        self.pick_folder(
            "Exclude",
            "Choose a folder inside this location that Sift should skip.",
            start,
            cx,
            move |path| Picked::Exclude {
                root: root.clone(),
                path,
            },
        );
    }

    fn on_picked(&mut self, pick: Picked, cx: &mut Context<Self>) {
        self.message = None;
        match pick {
            Picked::AddRoot(path) => {
                let path = path.canonicalize().unwrap_or(path);
                if sift_indexer::is_broad_root(&path) {
                    self.message = Some(
                        "Sift does not index your whole home folder or the startup disk, which \
                         would include system and app data. Choose specific folders, such as \
                         Documents or Projects."
                            .into(),
                    );
                } else {
                    self.send(
                        Command::AddRoot {
                            path,
                            options: IndexOptions::default(),
                        },
                        cx,
                    );
                }
            }
            Picked::Exclude { root, path } => {
                let path = path.canonicalize().unwrap_or(path);
                if !path.starts_with(&root) || path == root {
                    self.message =
                        Some(format!("Choose a folder inside {}.", self.display(&root)).into());
                } else if let Some(mut options) = self.options_of(&root, cx) {
                    if !options.exclude_paths.contains(&path) {
                        options.exclude_paths.push(path);
                        self.send(
                            Command::SetOptions {
                                path: root,
                                options,
                            },
                            cx,
                        );
                    }
                }
            }
        }
        cx.notify();
    }

    fn options_of(&self, root: &Path, cx: &App) -> Option<IndexOptions> {
        self.launcher
            .read(cx)
            .index_status()
            .roots
            .iter()
            .find(|r| r.path == root)
            .map(|r| r.options.clone())
    }

    fn display(&self, path: &Path) -> String {
        match self.home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
            Some(rest) if rest.as_os_str().is_empty() => "~".into(),
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        }
    }

    fn close(&mut self, _: &CloseSettings, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    /// Turns Open at Login on or off. Runs after the current update: the
    /// system may post notifications back into AppKit.
    fn toggle_login(&mut self, cx: &mut Context<Self>) {
        let turn_on = matches!(self.login, LoginStatus::Off);
        cx.spawn(async move |this, cx| {
            let result = login_item::set(turn_on);
            this.update(cx, |this, cx| {
                match result {
                    Ok(status) => {
                        tracing::info!(?status, "open at login changed");
                        this.login = status;
                    }
                    Err(reason) => {
                        this.message =
                            Some(format!("Could not change Open at Login: {reason}").into());
                        this.login = login_item::status();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_general(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (login_detail, login_on, login_enabled) = match self.login {
            LoginStatus::On => ("Sift starts when you log in.", true, true),
            LoginStatus::Off => ("Start Sift when you log in.", false, true),
            LoginStatus::NeedsApproval => (
                "Allow Sift in System Settings › General › Login Items.",
                true,
                true,
            ),
            LoginStatus::Unavailable => ("Available when Sift runs from Sift.app.", false, false),
        };
        let update = self.launcher.read(cx).update_state().clone();
        let current = crate::updates::CURRENT;
        let (update_detail, update_button) = match &update {
            UpdateState::Unknown => (format!("Sift {current}"), "Check for Updates"),
            UpdateState::Checking => (format!("Sift {current} · checking…"), "Check for Updates"),
            UpdateState::UpToDate => (
                format!("Sift {current} is the latest version."),
                "Check for Updates",
            ),
            UpdateState::Available(version) => (
                format!("Sift {version} is available (you have {current})."),
                "Download…",
            ),
            UpdateState::Failed(reason) => (
                format!("Sift {current} · could not check: {reason}"),
                "Check for Updates",
            ),
        };
        let checks_on = self.launcher.read(cx).update_checks_enabled();
        let row = |title: &'static str, detail: SharedString| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .gap(px(2.))
                .child(div().font_weight(FontWeight::MEDIUM).child(title))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted)
                        .child(detail),
                )
        };
        let login_switch = div()
            .id("open-at-login")
            .flex_none()
            .when(login_enabled, |s| {
                s.cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_login(cx)))
            })
            .when(!login_enabled, |s| s.opacity(0.45))
            .child(switch(login_on, theme));
        let launcher = self.launcher.clone();
        let check = button("check-updates", update_button, Kind::Secondary, theme).on_click(
            cx.listener(move |_, _, _, cx| {
                launcher.update(cx, |launcher, cx| match launcher.update_state() {
                    UpdateState::Available(_) => launcher.open_release_page(cx),
                    _ => launcher.check_for_updates(true, cx),
                });
            }),
        );
        let checks_switch =
            div()
                .id("check-automatically")
                .flex_none()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.change(Setting::UpdateChecks(!checks_on), cx)
                }))
                .child(switch(checks_on, theme));
        let edit_config = button("edit-config", "Edit Configuration…", Kind::Secondary, theme)
            .on_click(cx.listener(|this, _, _, cx| this.open_config_file(cx)));
        let hairline = || div().h(px(1.)).bg(theme.divider);
        let shortcut_row = self.render_shortcut(Slot::Panel, theme, cx);
        div()
            .p(px(14.))
            .rounded(px(10.))
            .border_1()
            .border_color(theme.divider)
            .bg(theme.card)
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(shortcut_row)
            .child(hairline())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(row("Open at Login", login_detail.into()))
                    .child(login_switch),
            )
            .child(hairline())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(row("Updates", update_detail.into()))
                    .child(check),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(row(
                        "Check Automatically",
                        "Once a day, ask GitHub whether a newer Sift is out. Nothing is \
                         downloaded, and nothing about you is sent."
                            .into(),
                    ))
                    .child(checks_switch),
            )
            .child(hairline())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(row(
                        "Snippets and Custom Commands",
                        "Written in config.toml, which opens in your text editor.".into(),
                    ))
                    .child(edit_config),
            )
            .into_any_element()
    }

    /// "Clipboard History": on or off, how much to keep, and clearing it.
    fn render_clipboard(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let launcher = self.launcher.read(cx);
        let on = launcher.clipboard_enabled();
        let max = launcher.clipboard_max_items();
        let count = launcher.clipboard_count();
        let row = |title: &'static str, detail: SharedString| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .gap(px(2.))
                .child(div().font_weight(FontWeight::MEDIUM).child(title))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted)
                        .child(detail),
                )
        };
        let hairline = || div().h(px(1.)).bg(theme.divider);
        let shortcut_row = self.render_shortcut(Slot::Clipboard, theme, cx);
        let pop_row = self.render_shortcut(Slot::Pop, theme, cx);

        let detail = if on {
            "Type clip in the panel to search text you copied, or clip and a word. \
             Text that password managers mark as private is never kept, nor copies \
             from apps listed in ignore_apps in config.toml."
        } else {
            "Keep a history of the text you copy, on this Mac only. Off by default, \
             because copied text can be private."
        };
        let enable = div()
            .id("clipboard-enabled")
            .flex_none()
            .cursor_pointer()
            .on_click(
                cx.listener(move |this, _, _, cx| this.change(Setting::ClipboardEnabled(!on), cx)),
            )
            .child(switch(on, theme));

        let mut sizes: Vec<usize> = CLIP_SIZES.to_vec();
        if !sizes.contains(&max) {
            sizes.push(max);
            sizes.sort_unstable();
        }
        let choices =
            div()
                .flex_none()
                .flex()
                .gap(px(4.))
                .children(sizes.into_iter().map(|n| {
                    let label = thousands(n as u64).into();
                    choice(("clip-size", n), label, n == max, theme).on_click(cx.listener(
                        move |this, _, _, cx| this.change(Setting::ClipboardMaxItems(n), cx),
                    ))
                }));

        let kept: SharedString = match (on, count) {
            (false, _) => {
                "Deletes what was kept while history was on, except pinned entries.".into()
            }
            (true, 0) => "Nothing kept yet.".into(),
            (true, 1) => "1 entry kept on this Mac. Pinned entries are not deleted.".into(),
            (true, n) => format!(
                "{} entries kept on this Mac. Pinned entries are not deleted.",
                thousands(n as u64)
            )
            .into(),
        };
        let clear: AnyElement = if self.confirm_clear {
            div()
                .flex_none()
                .flex()
                .gap(px(8.))
                .child(
                    button("clear-cancel", "Cancel", Kind::Secondary, theme).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.confirm_clear = false;
                            cx.notify();
                        },
                    )),
                )
                .child(
                    button("clear-confirm", "Delete History", Kind::Destructive, theme).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.confirm_clear = false;
                            this.launcher
                                .update(cx, |launcher, cx| launcher.clear_clipboard(cx));
                            speak("Clipboard history deleted.");
                            cx.notify();
                        }),
                    ),
                )
                .into_any_element()
        } else {
            button(
                "clear-clipboard",
                "Clear History…",
                Kind::Destructive,
                theme,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.confirm_clear = true;
                speak(
                    "Delete the clipboard history, except pinned entries? Choose Delete \
                     History or Cancel.",
                );
                cx.notify();
            }))
            .into_any_element()
        };
        let kept: SharedString = if self.confirm_clear {
            "Delete every entry that is not pinned? This cannot be undone.".into()
        } else {
            kept
        };

        div()
            .p(px(14.))
            .rounded(px(10.))
            .border_1()
            .border_color(theme.divider)
            .bg(theme.card)
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(row("Keep Clipboard History", detail.into()))
                    .child(enable),
            )
            .when(on, |card| {
                card.child(hairline())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(row(
                                "History Size",
                                "The newest entries are kept; older ones are deleted. \
                             Pinned entries (⌘D in the panel) do not count."
                                    .into(),
                            ))
                            .child(choices),
                    )
                    .child(hairline())
                    .child(shortcut_row)
                    .child(hairline())
                    .child(pop_row)
            })
            .child(hairline())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(row("Clear History", kept))
                    .child(clear),
            )
            .into_any_element()
    }

    fn render_window_management(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let launcher = self.launcher.read(cx);
        let enabled = launcher.window_management_enabled();
        let automatic = launcher.automatic_tiling_enabled();
        let status = launcher.window_management_status();
        div().p(px(14.)).rounded(px(10.)).border_1().border_color(theme.divider).bg(theme.card)
            .flex().flex_col().gap(px(12.))
            .child(div().flex().items_center().gap_3()
                .child(div().flex_1().min_w(px(0.)).flex().flex_col().gap(px(4.))
                    .child(div().font_weight(FontWeight::MEDIUM).child("Window Shortcuts and Controls"))
                    .child(div().text_size(px(12.)).text_color(theme.muted).child(status)))
                .child(div().id("window-management-enabled").cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.change(Setting::WindowManager(!enabled), cx)))
                    .child(switch(enabled, theme))))
            .child(div().h(px(1.)).bg(theme.divider))
            .child(div().flex().items_center().gap_3()
                .child(div().flex_1().min_w(px(0.)).flex().flex_col().gap(px(4.))
                    .child(div().font_weight(FontWeight::MEDIUM).child("Automatic Tiling"))
                    .child(div().text_size(px(12.)).text_color(theme.muted).child("Automatically arrange windows. Workspaces are available with tiling on or off.")))
                .child(div().id("window-management-automatic-tiling").cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.change(Setting::WindowManagerAutomaticTiling(!automatic), cx)))
                    .child(switch(automatic, theme))))
            .child(div().h(px(1.)).bg(theme.divider))
            .child(div().text_size(px(12.)).text_color(theme.muted)
                .child("Requires Device Control and Data Access on macOS 27, or Accessibility on older macOS. Control–Option with H/J/K/L focuses windows; arrow keys snap the selected window. The shortcuts below are saved to config.toml and can be changed here."))
            .child(div().flex().gap(px(8.))
                .child(button("window-management-permission", "Grant Window Control…", Kind::Secondary, theme)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.launcher.update(cx, |launcher, cx| launcher.request_window_permission(cx));
                        cx.notify();
                    })))
                .child(button("window-management-config", "Edit Rules and Shortcuts…", Kind::Secondary, theme)
                    .on_click(cx.listener(|this, _, _, cx| this.open_config_file(cx)))))
            .child(div().h(px(1.)).bg(theme.divider))
            .child(self.render_window_bindings(theme, cx))
            .into_any_element()
    }

    /// The window-management shortcuts, each with a key-cap recorder that
    /// saves the change to config.toml and re-registers the shortcuts.
    fn render_window_bindings(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mode = self
            .launcher
            .read(cx)
            .user
            .window_manager_config()
            .bindings
            .get("main")
            .cloned()
            .unwrap_or_default();
        let default_main = sift_wm::Config::default()
            .bindings
            .get("main")
            .cloned()
            .unwrap_or_default();
        let mut entries: Vec<(String, String)> = mode
            .iter()
            .map(|(key, commands)| (key.clone(), describe_commands(commands.clone())))
            .collect();
        entries.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        let rows: Vec<AnyElement> = entries
            .into_iter()
            .enumerate()
            .map(|(index, (key, description))| {
                let shortcut = hotkey::from_binding(&key);
                let recording =
                    matches!(&self.recording, Some((Recording::Binding(k), _)) if k == &key);
                let (detail, problem) = self
                    .shortcut_note
                    .as_ref()
                    .filter(|(t, ..)| matches!(t, Recording::Binding(k) if k == &key))
                    .map(|(_, note, problem)| (note.clone(), *problem))
                    .unwrap_or_else(|| {
                        let unusable = shortcut.is_none();
                        (
                            if unusable {
                                format!("{description} (not usable)").into()
                            } else {
                                description.clone().into()
                            },
                            unusable,
                        )
                    });
                let record_key = key.clone();
                let recorder = div()
                    .id(("binding-recorder", index))
                    .flex_none()
                    .min_w(px(92.))
                    .h(px(26.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(3.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .map(|el| {
                        if recording {
                            el.border_color(theme.accent)
                                .bg(theme.card)
                                .text_color(theme.accent)
                                .child("Type shortcut…")
                        } else {
                            let hover = theme.selected;
                            let el = el
                                .border_color(gpui::transparent_black())
                                .bg(theme.key_background)
                                .text_color(theme.key_text)
                                .hover(move |style| style.bg(hover));
                            match &shortcut {
                                Some(shortcut) => el.children(
                                    shortcut.parts().into_iter().map(|part| div().child(part)),
                                ),
                                None => el.child("Not usable"),
                            }
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if recording {
                            this.stop_recording(cx);
                        } else {
                            this.stop_recording(cx);
                            this.start_recording(Recording::Binding(record_key.clone()), cx);
                        }
                    }));
                // A default shortcut can be reset to Sift's default; a
                // custom one can be removed. Neither makes sense when the
                // binding already shows the default.
                let reset_key = key.clone();
                let reset = (!recording
                    && shortcut.is_some()
                    && default_main
                        .get(&key)
                        .is_some_and(|default| mode.get(&key) != Some(default)))
                .then(|| {
                    button(("binding-reset", index), "Use Default", Kind::Link, theme).on_click(
                        cx.listener(move |this, _, _, cx| this.remove_binding(&reset_key, cx)),
                    )
                });
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(div().font_weight(FontWeight::MEDIUM).child(description))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(if problem { theme.error } else { theme.muted })
                                    .child(detail),
                            ),
                    )
                    .children(reset)
                    .child(recorder)
                    .into_any_element()
            })
            .collect();
        div().flex().flex_col().gap(px(12.))
            .child(div().font_weight(FontWeight::MEDIUM).child("Shortcuts"))
            .child(div().text_size(px(12.)).text_color(theme.muted)
                .child("Choose a key cap and press the new shortcut. Escape cancels. Shortcuts are saved to config.toml and work while Window Shortcuts and Controls is on."))
            .children(rows)
            .into_any_element()
    }

    /// "Keyboard Shortcut": a slot's shortcut as key caps, which start
    /// recording when clicked.
    fn render_shortcut(&self, slot: Slot, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (shortcut, registered) = cx
            .try_global::<Hotkeys>()
            .map(|h| (h.current(slot).cloned(), h.is_registered(slot)))
            .unwrap_or((None, false));
        let shortcut = match slot {
            Slot::Panel => Some(shortcut.unwrap_or_default()),
            Slot::Clipboard | Slot::Pop => shortcut,
        };
        let recording = matches!(&self.recording, Some((Recording::Slot(s), _)) if *s == slot);
        let note = self
            .shortcut_note
            .as_ref()
            .filter(|(t, ..)| matches!(t, Recording::Slot(s) if *s == slot))
            .map(|(_, note, problem)| (note.clone(), *problem));
        let (detail, problem): (SharedString, bool) = match (note, &shortcut) {
            (Some(note), _) => note,
            (None, _) if recording => ("Type the new shortcut. Esc cancels.".into(), false),
            (None, Some(shortcut)) if !registered => (
                format!(
                    "{} is not working: macOS did not accept it. Choose another.",
                    shortcut.display()
                )
                .into(),
                true,
            ),
            (None, _) => match (slot, &shortcut) {
                (Slot::Panel, _) => ("Shows and hides the Sift panel.".into(), false),
                (Slot::Clipboard, Some(_)) => {
                    ("Opens the panel on your clipboard history.".into(), false)
                }
                (Slot::Clipboard, None) => (
                    "Open the panel on your clipboard history with a shortcut of its own.".into(),
                    false,
                ),
                (Slot::Pop, Some(_)) if !sift_macos::wm::trusted() => (
                    "Needs Accessibility to paste: allow Sift in System Settings › Privacy & \
                     Security."
                        .into(),
                    true,
                ),
                (Slot::Pop, Some(_)) => (
                    "Pastes the newest entry into the app in front and removes it, leaving \
                     the one before on the clipboard."
                        .into(),
                    false,
                ),
                (Slot::Pop, None) => (
                    "Paste the newest entry and remove it, so the next paste gets the one \
                     before: a stack. Needs Accessibility."
                        .into(),
                    false,
                ),
            },
        };
        let recorder = div()
            .id(("shortcut-recorder", slot as usize))
            .flex_none()
            .min_w(px(92.))
            .h(px(26.))
            .px(px(8.))
            .rounded(px(6.))
            .border_1()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(3.))
            .text_size(px(12.))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .map(|el| {
                if recording {
                    el.border_color(theme.accent)
                        .bg(theme.card)
                        .text_color(theme.accent)
                        .child("Type shortcut…")
                } else {
                    let hover = theme.selected;
                    let el = el
                        .border_color(gpui::transparent_black())
                        .bg(theme.key_background)
                        .text_color(theme.key_text)
                        .hover(move |style| style.bg(hover));
                    match &shortcut {
                        Some(shortcut) => {
                            el.children(shortcut.parts().into_iter().map(|part| div().child(part)))
                        }
                        None => el.child("Record Shortcut"),
                    }
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if recording {
                    this.stop_recording(cx);
                } else {
                    this.stop_recording(cx);
                    this.start_recording(Recording::Slot(slot), cx);
                }
            }));
        let reset = match (slot, &shortcut) {
            _ if recording => None,
            (Slot::Panel, Some(shortcut)) if !shortcut.is_default() => Some(
                button(
                    ("shortcut-reset", slot as usize),
                    "Use ⌥Space",
                    Kind::Link,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.stop_recording(cx);
                    this.use_shortcut(Slot::Panel, Some(Shortcut::default()), cx);
                })),
            ),
            (Slot::Clipboard | Slot::Pop, Some(_)) => Some(
                button(
                    ("shortcut-reset", slot as usize),
                    "Remove",
                    Kind::Link,
                    theme,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.stop_recording(cx);
                    this.use_shortcut(slot, None, cx);
                })),
            ),
            _ => None,
        };
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.))
                    .child(div().font_weight(FontWeight::MEDIUM).child(match slot {
                        Slot::Panel | Slot::Clipboard => "Keyboard Shortcut",
                        Slot::Pop => "Paste and Pop",
                    }))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(if problem { theme.error } else { theme.muted })
                            .child(detail),
                    ),
            )
            .children(reset)
            .child(recorder)
            .into_any_element()
    }

    fn render_root(
        &self,
        index: usize,
        root: &RootStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (state, state_color) = match &root.state {
            RootState::Watching => ("Up to date".to_string(), theme.success),
            RootState::Indexing { entries } => {
                (format!("Indexing… {}", thousands(*entries)), theme.accent)
            }
            RootState::Unavailable => ("Not available".to_string(), theme.error),
            RootState::Failed(_) => ("Failed".to_string(), theme.error),
        };
        let problem = match &root.state {
            RootState::Unavailable => Some("Moved, deleted or not mounted.".to_string()),
            RootState::Failed(reason) => Some(reason.clone()),
            _ => None,
        };
        let details = format!(
            "{} items · indexed {}{}",
            thousands(root.entries),
            ago(root.indexed_at),
            if root.errors > 0 {
                format!(" · {} unreadable", thousands(root.errors))
            } else {
                String::new()
            }
        );
        let skip_deps = root.options.skip_dependency_dirs;
        let path = root.path.clone();

        let reindex = {
            let path = path.clone();
            button(("reindex", index), "Re-index", Kind::Secondary, theme).on_click(
                cx.listener(move |this, _, _, cx| this.send(Command::Reindex(path.clone()), cx)),
            )
        };
        let exclude = {
            let path = path.clone();
            button(
                ("exclude", index),
                "Exclude Folder…",
                Kind::Secondary,
                theme,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.exclude_folder(path.clone(), cx)))
        };
        let toggle = {
            let path = path.clone();
            let options = root.options.clone();
            div()
                .id(("deps", index))
                .flex()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .child(switch(skip_deps, theme))
                .child(
                    div()
                        .text_size(px(12.))
                        .child("Skip dependency and cache folders"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    let mut options = options.clone();
                    options.skip_dependency_dirs = !options.skip_dependency_dirs;
                    this.send(
                        Command::SetOptions {
                            path: path.clone(),
                            options,
                        },
                        cx,
                    );
                }))
        };
        let remove = {
            let path = path.clone();
            button(("remove", index), "Remove", Kind::Destructive, theme).on_click(
                cx.listener(move |this, _, _, cx| this.send(Command::RemoveRoot(path.clone()), cx)),
            )
        };

        let exclusions: Vec<AnyElement> = root
            .options
            .exclude_paths
            .iter()
            .enumerate()
            .map(|(i, excluded)| {
                let mut options = root.options.clone();
                options.exclude_paths.retain(|p| p != excluded);
                let path = path.clone();
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(px(12.))
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(theme.muted)
                            .child(format!("Skipped: {}", self.display(excluded))),
                    )
                    .child(
                        button(
                            ("unexclude", index * 1000 + i),
                            "Include Again",
                            Kind::Link,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send(
                                Command::SetOptions {
                                    path: path.clone(),
                                    options: options.clone(),
                                },
                                cx,
                            )
                        })),
                    )
                    .into_any_element()
            })
            .collect();

        let hairline = || div().h(px(1.)).bg(theme.divider);
        div()
            .p(px(14.))
            .rounded(px(10.))
            .border_1()
            .border_color(theme.divider)
            .bg(theme.card)
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_none()
                            .size(px(32.))
                            .rounded(px(8.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(theme.accent.opacity(0.14))
                            .child(
                                svg()
                                    .path(crate::assets::FOLDER)
                                    .size(px(17.))
                                    .text_color(theme.accent),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(self.display(&root.path)),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(theme.muted)
                                    .child(problem.unwrap_or(details)),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(theme.muted)
                            .child(div().size(px(7.)).rounded_full().bg(state_color))
                            .child(state),
                    ),
            )
            .when(!exclusions.is_empty(), |card| {
                card.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .pl(px(44.))
                        .children(exclusions),
                )
            })
            .child(hairline())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(toggle)
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(reindex)
                            .child(exclude)
                            .child(remove),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for Settings {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        let status = self.launcher.read(cx).index_status().clone();
        let roots: Vec<AnyElement> = status
            .roots
            .iter()
            .enumerate()
            .map(|(i, root)| self.render_root(i, root, &theme, cx))
            .collect();
        let message = self
            .message
            .clone()
            .or_else(|| status.message.clone().map(Into::into));
        let mut facts = vec![format!("Index size {}", bytes(status.index_bytes))];
        if let Some(update) = status.last_update {
            facts.push(format!(
                "last change searchable in {} ms",
                update.event_to_searchable.as_millis()
            ));
        }
        let count = match status.roots.len() {
            0 => String::new(),
            1 => "1 folder".to_string(),
            n => format!("{n} folders"),
        };

        div()
            .key_context("Settings")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close))
            .size_full()
            .bg(theme.background)
            .text_color(theme.text)
            .text_size(px(13.))
            .flex()
            .flex_col()
            .child(
                div()
                    .id("settings-page")
                    .flex_grow()
                    .overflow_y_scroll()
                    .px(px(PAGE_INSET))
                    .pt(px(20.))
                    .pb(px(16.))
                    .flex()
                    .flex_col()
                    .child(section_label("General", &theme))
                    .child(div().pb(px(18.)).child(self.render_general(&theme, cx)))
                    .child(section_label("Window Management", &theme))
                    .child(div().pb(px(18.)).child(self.render_window_management(&theme, cx)))
                    .child(section_label("Clipboard History", &theme))
                    .child(div().pb(px(18.)).child(self.render_clipboard(&theme, cx)))
                    .child(
                        div()
                            .pb(px(8.))
                            .flex()
                            .justify_between()
                            .text_size(px(11.))
                            .text_color(theme.placeholder)
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("Locations"))
                            .child(count),
                    )
                    .child(div().pb(px(10.)).max_w(px(540.)).text_color(theme.muted).child(
                        "Sift finds files and folders by name in these locations and keeps up \
                         with changes as they happen. Only names, sizes and dates are stored, \
                         on this Mac; file contents are never read.",
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .when(roots.is_empty(), |list| {
                                list.child(
                                    div()
                                        .p(px(20.))
                                        .rounded(px(10.))
                                        .border_1()
                                        .border_color(theme.divider)
                                        .bg(theme.card)
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .gap(px(6.))
                                        .text_center()
                                        .child(
                                            div()
                                                .font_weight(FontWeight::MEDIUM)
                                                .child("No folders yet"),
                                        )
                                        .child(div().text_color(theme.muted).child(
                                            "Add the folders you want to search, such as \
                                             Documents or a projects folder.",
                                        )),
                                )
                            })
                            .children(roots),
                    ),
            )
            .when_some(message, |page, message| {
                page.child(
                    div().px(px(PAGE_INSET)).pb(px(12.)).child(
                        div()
                            .px(px(12.))
                            .py(px(8.))
                            .rounded(px(8.))
                            .bg(theme.error.opacity(0.12))
                            .text_color(theme.error)
                            .child(message),
                    ),
                )
            })
            .child(div().h(px(1.)).bg(theme.divider))
            .child(
                div()
                    .h(px(52.))
                    .px(px(PAGE_INSET))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .bg(theme.surface)
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(px(12.))
                            .text_color(theme.muted)
                            .child(facts.join(" · ")),
                    )
                    .child(
                        button("add", "Add Folder…", Kind::Primary, &theme)
                            .on_click(cx.listener(|this, _, _, cx| this.add_folder(cx))),
                    ),
            )
    }
}

/// Turns the shortcut back on after recording.
fn resume_shortcut(cx: &mut App) {
    if cx.try_global::<Hotkeys>().is_some() {
        cx.update_global::<Hotkeys, _>(|hotkeys, _| hotkeys.resume());
    }
}

/// Tells VoiceOver, if it is running (the window has no accessibility
/// tree to read; see `sift_macos::a11y`).
fn speak(text: &str) {
    if a11y::voice_over_enabled() {
        a11y::announce(text, Urgency::Interrupt);
    }
}

/// Left and right margin of the settings page.
const PAGE_INSET: f32 = 24.;

#[derive(Clone, Copy)]
enum Kind {
    /// The one main action on the page.
    Primary,
    Secondary,
    /// Removes something.
    Destructive,
    /// A small text-only action inside a line.
    Link,
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    kind: Kind,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label);
    let (bg, hover, text) = (theme.key_background, theme.selected, theme.text);
    match kind {
        Kind::Primary => base
            .h(px(28.))
            .px(px(14.))
            .bg(theme.accent)
            .text_color(theme.on_accent)
            .hover(|style| style.opacity(0.88)),
        Kind::Secondary => base
            .h(px(26.))
            .px(px(10.))
            .bg(bg)
            .text_color(text)
            .hover(move |style| style.bg(hover)),
        Kind::Destructive => base
            .h(px(26.))
            .px(px(10.))
            .bg(bg)
            .text_color(theme.error)
            .hover(move |style| style.bg(hover)),
        Kind::Link => base
            .px(px(4.))
            .text_color(theme.accent)
            .hover(|style| style.opacity(0.75)),
    }
}

/// A small heading above a card.
fn section_label(text: &'static str, theme: &Theme) -> gpui::Div {
    div()
        .pb(px(8.))
        .text_size(px(11.))
        .text_color(theme.placeholder)
        .font_weight(FontWeight::SEMIBOLD)
        .child(text)
}

/// One option of a set of choices; the selected one is filled.
fn choice(
    id: impl Into<gpui::ElementId>,
    label: SharedString,
    selected: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .flex_none()
        .h(px(24.))
        .min_w(px(40.))
        .px(px(8.))
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label);
    if selected {
        base.bg(theme.accent).text_color(theme.on_accent)
    } else {
        let hover = theme.selected;
        base.bg(theme.key_background)
            .text_color(theme.text)
            .hover(move |style| style.bg(hover))
    }
}

/// An on/off switch, drawn without animation.
fn switch(on: bool, theme: &Theme) -> AnyElement {
    div()
        .flex_none()
        .w(px(30.))
        .h(px(18.))
        .p(px(2.))
        .rounded_full()
        .flex()
        .items_center()
        .when(on, |track| track.justify_end().bg(theme.accent))
        .when(!on, |track| track.justify_start().bg(theme.selected))
        .child(
            div()
                .size(px(14.))
                .rounded_full()
                .bg(gpui::white())
                .shadow_sm(),
        )
        .into_any_element()
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.),
        n => format!("{n} bytes"),
    }
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
fn ago(unix: i64) -> String {
    if unix <= 0 {
        return "never".into();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(unix);
    let secs = (now - unix).max(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_descriptions_cover_the_common_commands() {
        let commands = |text: &str| vec![text.to_string()];
        assert_eq!(
            describe_commands(vec!["workspace 3".into()]),
            "Switch to Workspace 3"
        );
        assert_eq!(
            describe_commands(commands("move-node-to-workspace 7")),
            "Move the Window to Workspace 7"
        );
        assert_eq!(
            describe_commands(vec!["workspace-back-and-forth".into()]),
            "Switch to the Previous Workspace"
        );
        assert_eq!(
            describe_commands(vec!["focus left".into()]),
            "Focus the Window to the Left"
        );
        assert_eq!(
            describe_commands(vec!["focus --target terminal".into()]),
            "Focus Target terminal"
        );
        assert_eq!(
            describe_commands(vec!["move up".into()]),
            "Move the Window Up"
        );
        assert_eq!(
            describe_commands(vec!["snap center".into()]),
            "Center the Window"
        );
        assert_eq!(
            describe_commands(vec!["fullscreen".into()]),
            "Toggle Fullscreen"
        );
        assert_eq!(
            describe_commands(vec!["layout tiles horizontal vertical".into()]),
            "Change the Layout"
        );
        // A sequence describes its first command; unknown text shows itself.
        assert_eq!(
            describe_commands(vec!["focus --target nope".into()]),
            "Focus Target nope"
        );
        assert_eq!(
            describe_commands(vec![
                "layout tiles horizontal vertical".into(),
                "move left".into()
            ]),
            "Change the Layout"
        );
        assert_eq!(describe_commands(vec![]), "");
    }

    #[test]
    fn settings_has_a_usable_default_hotkey_list() {
        // The window-management settings list needs the same descriptions
        // the defaults produce, and every default main-mode key must be
        // usable in the main mode.
        let default = sift_wm::Config::default();
        let main = &default.bindings["main"];
        assert!(main.contains_key("alt-1"));
        assert!(!main.contains_key("ctrl-1"));
        for (key, commands) in main {
            let shortcut = hotkey::from_binding(key).expect("default binding is usable");
            assert!(!describe_commands(commands.clone()).is_empty(), "{key}");
            assert!(shortcut.same_key(&shortcut));
        }
    }

    #[test]
    fn formats_sizes_and_ages() {
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(2 * 1024 * 1024 + 100), "2.0 MB");
        assert_eq!(thousands(54_206), "54,206");
        assert_eq!(ago(0), "never");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 600), "10 min ago");
        assert_eq!(ago(now - 3 * 86400), "3 days ago");
    }
}
