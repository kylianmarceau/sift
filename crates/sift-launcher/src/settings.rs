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

use crate::hotkey::{self, Hotkeys, Recorded, Shortcut};
use crate::launcher::{IndexStatusChanged, Launcher, UpdateStateChanged, UserDataChanged};
use crate::theme::Theme;
use crate::updates::UpdateState;
use sift_macos::a11y::{self, Urgency};
use sift_macos::key_capture::{KeyCapture, KeyPress};
use sift_macos::login_item::{self, Status as LoginStatus};
use sift_state::Setting;

/// History sizes offered in the settings window. Another value set in
/// config.toml is shown as an extra choice.
const CLIP_SIZES: &[usize] = &[50, 200, 500, 1000];

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
    /// While a new shortcut is being recorded: the capture of key presses.
    recording: Option<KeyCapture>,
    /// A note under the shortcut, and whether it reports a problem.
    shortcut_note: Option<(SharedString, bool)>,
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
                cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
                cx.observe_window_activation(window, |this, window, cx| {
                    if window.is_window_active() {
                        this.login = login_item::status();
                        // Picks up hand edits to config.toml (one `stat`).
                        this.launcher
                            .update(cx, |launcher, cx| launcher.reload_config(cx));
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

    /// Starts recording a new shortcut: the next key press in Sift's
    /// windows becomes it. The current shortcut is off meanwhile, so it can
    /// be pressed too.
    fn start_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording.is_some() {
            return;
        }
        let (tx, mut presses) = unbounded::<KeyPress>();
        let Some(capture) = KeyCapture::start(move |press| {
            let _ = tx.unbounded_send(press);
        }) else {
            self.shortcut_note = Some(("Cannot read the keyboard right now.".into(), true));
            cx.notify();
            return;
        };
        if let Some(hotkeys) = cx.try_global::<Hotkeys>().is_some().then_some(()) {
            let () = hotkeys;
            cx.update_global::<Hotkeys, _>(|hotkeys, _| hotkeys.pause());
        }
        self.recording = Some(capture);
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
        if self.recording.is_none() {
            return;
        }
        match hotkey::interpret(&press) {
            Recorded::Cancel => {
                self.stop_recording(cx);
                speak("Cancelled.");
            }
            Recorded::Rejected(reason) => {
                self.shortcut_note = Some((reason.into(), true));
                speak(reason);
                cx.notify();
            }
            Recorded::Shortcut(shortcut) => {
                self.recording = None;
                self.use_shortcut(shortcut, cx);
            }
        }
    }

    /// Makes `shortcut` the one that shows the panel.
    fn use_shortcut(&mut self, shortcut: Shortcut, cx: &mut Context<Self>) {
        let result = if cx.try_global::<Hotkeys>().is_some() {
            cx.update_global::<Hotkeys, _>(|hotkeys, _| hotkeys.set(shortcut.clone()))
        } else {
            Err("Keyboard shortcuts are not available.".to_string())
        };
        match result {
            Ok(()) => {
                tracing::info!(shortcut = shortcut.display(), "keyboard shortcut changed");
                speak(&format!("Shortcut set to {}.", shortcut.spoken()));
                self.shortcut_note = shortcut.system_conflict().map(|note| (note.into(), false));
            }
            Err(reason) => {
                speak(&reason);
                self.shortcut_note = Some((reason.into(), true));
            }
        }
        cx.notify();
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
        let shortcut_row = self.render_shortcut(theme, cx);
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

        let detail = if on {
            "Type clip in the panel to search text you copied, or clip and a word. \
             Text that password managers mark as private is never kept."
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
            (false, _) => "Deletes what was kept while history was on.".into(),
            (true, 0) => "Nothing kept yet.".into(),
            (true, 1) => "1 entry kept on this Mac.".into(),
            (true, n) => format!("{} entries kept on this Mac.", thousands(n as u64)).into(),
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
                speak("Delete the clipboard history? Choose Delete History or Cancel.");
                cx.notify();
            }))
            .into_any_element()
        };
        let kept: SharedString = if self.confirm_clear {
            "Delete every entry? This cannot be undone.".into()
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
                card.child(hairline()).child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(row(
                            "History Size",
                            "The newest entries are kept; older ones are deleted.".into(),
                        ))
                        .child(choices),
                )
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

    /// "Keyboard Shortcut": the current shortcut as key caps, which start
    /// recording when clicked.
    fn render_shortcut(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (shortcut, registered) = cx
            .try_global::<Hotkeys>()
            .map(|h| (h.current().clone(), h.is_registered()))
            .unwrap_or((Shortcut::default(), false));
        let recording = self.recording.is_some();
        let (detail, problem): (SharedString, bool) = match &self.shortcut_note {
            Some((note, problem)) => (note.clone(), *problem),
            None if recording => ("Type the new shortcut. Esc cancels.".into(), false),
            None if !registered => (
                format!(
                    "{} is not working: macOS did not accept it. Choose another.",
                    shortcut.display()
                )
                .into(),
                true,
            ),
            None => ("Shows and hides the Sift panel.".into(), false),
        };
        let recorder = div()
            .id("shortcut-recorder")
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
                    el.border_color(gpui::transparent_black())
                        .bg(theme.key_background)
                        .text_color(theme.key_text)
                        .hover(move |style| style.bg(hover))
                        .children(shortcut.parts().into_iter().map(|part| div().child(part)))
                }
            })
            .on_click(cx.listener(|this, _, _, cx| {
                if this.recording.is_some() {
                    this.stop_recording(cx);
                } else {
                    this.start_recording(cx);
                }
            }));
        let reset = (!recording && !shortcut.is_default()).then(|| {
            button("shortcut-reset", "Use ⌥Space", Kind::Link, theme).on_click(cx.listener(
                |this, _, _, cx| {
                    this.stop_recording(cx);
                    this.use_shortcut(Shortcut::default(), cx);
                },
            ))
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
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Keyboard Shortcut"),
                    )
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
