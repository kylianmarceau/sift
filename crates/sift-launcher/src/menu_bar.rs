//! Sift's native status menu. Reads cached state and subscribes to publications;
//! no timer, shell command, AX call or process launch is used to build the menu.
use super::{
    IndexStatusChanged, Launcher, UpdateStateChanged, UserDataChanged, WindowManagerChanged,
};
use crate::{commands, updates::UpdateState};
use futures::{StreamExt, channel::mpsc::unbounded};
use gpui::{App, AppContext, Context, Entity, Global, Subscription, WindowHandle};
use serde::{Deserialize, Serialize};
use sift_macos::status_bar::{self, Indicator, Item, MenuBar};
use sift_state::Setting;
use sift_wm::{Command, Snapshot, Window as ManagedWindow};
use std::{
    collections::BTreeSet,
    hash::{Hash, Hasher},
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
enum Action {
    Search(String),
    Builtin(String),
    Setting(SettingAction),
    WindowCommand(Command, Option<WindowIdentity>),
    AssignApp {
        workspace: String,
        app_id: String,
        open: Option<PathBuf>,
    },
    UnassignApp {
        workspace: String,
        app_id: String,
    },
    AddWorkspace(String),
    AddWorkspaceShortcuts,
    Reload,
    Permission,
    Login(bool),
    OpenLogs,
    ShowApp,
    ClearError,
    Help(String),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
enum SettingAction {
    Controls(bool),
    Tiling(bool),
    Clipboard(bool),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct WindowIdentity {
    id: u32,
    pid: i32,
    app_id: String,
}
impl From<&ManagedWindow> for WindowIdentity {
    fn from(w: &ManagedWindow) -> Self {
        Self {
            id: w.id,
            pid: w.pid,
            app_id: w.app_id.clone(),
        }
    }
}
impl WindowIdentity {
    fn exists(&self, snapshot: &Snapshot) -> bool {
        snapshot
            .windows
            .iter()
            .any(|w| w.id == self.id && w.pid == self.pid && w.app_id == self.app_id)
    }
}
fn action(title: impl Into<String>, value: Action) -> Item {
    let mut item = Item::label(title);
    item.action = Some(serde_json::to_string(&value).expect("menu action serialization"));
    item
}
fn check(title: &str, value: Action, checked: bool) -> Item {
    let mut item = action(title, value);
    item.checked = checked;
    item
}
fn submenu(title: impl Into<String>, children: Vec<Item>) -> Item {
    let mut item = Item::label(title);
    item.children = children;
    item
}
fn icon(mut item: Item, symbol: &str) -> Item {
    item.symbol = Some(symbol.into());
    item
}
fn detail(mut item: Item, text: impl Into<String>) -> Item {
    let text = text.into();
    item.detail = (!text.is_empty()).then_some(text);
    item
}
fn builtin(title: &str, id: &str) -> Item {
    let mut item = action(title, Action::Builtin(id.into()));
    item.key = match id {
        commands::SETTINGS => ",",
        commands::QUIT => "q",
        _ => "",
    }
    .into();
    item
}
fn short(text: &str, limit: usize) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.chars().count() > limit {
        format!("{}…", clean.chars().take(limit).collect::<String>())
    } else {
        clean
    }
}

/// A binding such as `alt-1` as a menu key equivalent. Keys that are not a
/// single character (arrows, `minus`) are left out of the menu.
fn key_equivalent(binding: &str) -> Option<(String, u8)> {
    let mut parts: Vec<&str> = binding.split('-').collect();
    let key = parts.pop()?;
    if key.chars().count() != 1 {
        return None;
    }
    let mut modifiers = 0;
    for part in parts {
        modifiers |= match part {
            "cmd" | "command" => status_bar::COMMAND,
            "alt" | "opt" | "option" => status_bar::OPTION,
            "ctrl" | "control" => status_bar::CONTROL,
            "shift" => status_bar::SHIFT,
            _ => return None,
        };
    }
    (modifiers != 0).then(|| (key.to_lowercase(), modifiers))
}

/// The main-mode shortcut that does exactly `workspace <name>`, if any.
fn workspace_key(
    bindings: &std::collections::BTreeMap<String, Vec<String>>,
    name: &str,
) -> Option<(String, u8)> {
    let quoted = serde_json::to_string(name).ok()?;
    bindings.iter().find_map(|(key, commands)| {
        matches!(commands.as_slice(), [only] if *only == format!("workspace {name}") || *only == format!("workspace {quoted}"))
            .then(|| key_equivalent(key))
            .flatten()
    })
}

fn assignment_notice(app: &str, workspace: &str, active: bool) -> String {
    if active {
        format!("{app} assigned to Workspace {workspace}.")
    } else {
        format!(
            "{app} assigned to Workspace {workspace}. Nothing moved: window controls need permission or must be enabled."
        )
    }
}

fn saved_assignment<'a>(config: &'a sift_wm::Config, app_id: &str) -> Option<&'a str> {
    let exact = format!("^{}$", regex::escape(app_id));
    config
        .rules
        .iter()
        .find(|r| {
            r.app_id.as_deref() == Some(exact.as_str()) && r.title.is_none() && r.app_name.is_none()
        })
        .and_then(|r| r.workspace.as_deref())
}

struct State {
    native: Option<MenuBar>,
    launcher: Entity<Launcher>,
    window: WindowHandle<Launcher>,
    error: Option<String>,
    notice: Option<String>,
    key: Option<u64>,
    login: sift_macos::login_item::Status,
    /// The app-assignment tree (every installed app), rebuilt only when the
    /// catalog, workspace names or assignments change.
    picker: Option<(u64, Vec<Item>)>,
    _subscriptions: Vec<Subscription>,
}
struct KeepAlive {
    _state: Entity<State>,
}
impl Global for KeepAlive {}

pub(crate) fn install(window: WindowHandle<Launcher>, cx: &mut App) {
    let Ok(launcher) = window.update(cx, |_, _, cx| cx.entity()) else {
        return;
    };
    let state = cx.new(|cx: &mut Context<State>| {
        let (tx, mut rx) = unbounded::<String>();
        let native = MenuBar::new(move |action| {
            let _ = tx.unbounded_send(action);
        });
        cx.spawn(async move |this, cx| {
            while let Some(value) = rx.next().await {
                if this
                    .update(cx, |this, cx| {
                        if value.is_empty() {
                            this.login = sift_macos::login_item::status();
                            this.key = None;
                            // Recheck after returning from System Settings. Do
                            // not prompt or keep a permission polling timer.
                            this.launcher.update(cx, |launcher, cx| {
                                if launcher.window_manager.is_none() && sift_macos::wm::trusted() {
                                    launcher.retry_window_management(cx);
                                }
                            });
                        }
                        // A close notification flushes updates deferred while tracking.
                        if !value.is_empty() {
                            match serde_json::from_str(&value)
                                .map_err(|e| e.to_string())
                                .and_then(|action| this.perform(action, cx))
                            {
                                Ok(()) => this.error = None,
                                Err(error) => {
                                    tracing::warn!(%error, "menu bar action failed");
                                    this.error = Some(error);
                                }
                            }
                        }
                        this.refresh(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        State {
            native,
            launcher: launcher.clone(),
            window,
            error: None,
            notice: None,
            key: None,
            login: sift_macos::login_item::status(),
            picker: None,
            _subscriptions: vec![
                cx.subscribe(&launcher, |this, _, _: &IndexStatusChanged, cx| {
                    this.refresh(cx)
                }),
                cx.subscribe(&launcher, |this, _, _: &UserDataChanged, cx| {
                    this.refresh(cx)
                }),
                cx.subscribe(&launcher, |this, _, _: &UpdateStateChanged, cx| {
                    this.refresh(cx)
                }),
                cx.subscribe(&launcher, |this, _, _: &WindowManagerChanged, cx| {
                    this.refresh(cx)
                }),
            ],
        }
    });
    state.update(cx, |this, cx| this.refresh(cx));
    cx.set_global(KeepAlive { _state: state });
}

impl State {
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.native.is_none() {
            return;
        }
        let shortcuts = cx
            .try_global::<crate::hotkey::Hotkeys>()
            .map(|hotkeys| {
                [crate::hotkey::Slot::Panel, crate::hotkey::Slot::Clipboard]
                    .map(|slot| hotkeys.current(slot).map(|s| s.display()))
            })
            .unwrap_or_default();
        let launcher = self.launcher.read(cx);
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        (
            menu_key(launcher, self.error.as_deref(), self.login),
            &self.notice,
            &shortcuts,
        )
            .hash(&mut hash);
        let key = hash.finish();
        if self.key == Some(key) {
            return;
        }
        let config = launcher.user.window_manager_config();
        let names = workspace_names(config, &launcher.window_snapshot);
        let picker_key = {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            (launcher.catalog_loaded_at, &names).hash(&mut hash);
            for rule in &config.rules {
                (&rule.app_id, &rule.workspace, &rule.title, &rule.app_name).hash(&mut hash);
            }
            hash.finish()
        };
        if self.picker.as_ref().is_none_or(|(k, _)| *k != picker_key) {
            self.picker = Some((
                picker_key,
                app_picker(launcher.apps.as_deref(), config, &names),
            ));
        }
        let picker = self
            .picker
            .as_ref()
            .map(|(_, p)| p.clone())
            .unwrap_or_default();
        let (indicator, items) = model(
            launcher,
            &Extras {
                error: self.error.as_deref(),
                notice: self.notice.as_deref(),
                login: self.login,
                panel_shortcut: shortcuts[0].clone(),
                clipboard_shortcut: shortcuts[1].clone(),
                picker,
            },
        );
        let tooltip = format!(
            "Sift {} · {}",
            env!("CARGO_PKG_VERSION"),
            self.notice
                .as_deref()
                .unwrap_or(&launcher.window_management_status())
        );
        if let Some(native) = &self.native {
            native.update(indicator, tooltip, items);
        }
        self.key = Some(key);
    }
    fn perform(&mut self, action: Action, cx: &mut Context<Self>) -> Result<(), String> {
        self.notice = None;
        if action == Action::ClearError {
            self.error = None;
            return Ok(());
        }
        if let Action::Login(on) = &action {
            self.login = sift_macos::login_item::set(*on)?;
            return Ok(());
        }
        if let Action::WindowCommand(command, identity) = &action {
            let runtime = self
                .launcher
                .read(cx)
                .window_manager
                .as_ref()
                .ok_or("Window controls are not running. Enable them in Settings.")?;
            let controller = runtime.controller();
            if let Some(identity) = identity
                && !identity.exists(&controller.snapshot())
            {
                return Err(
                    "That window has closed. Reopen the menu and choose another window.".into(),
                );
            }
            return controller.execute(command.clone());
        }
        if action == Action::AddWorkspaceShortcuts {
            let count = self
                .launcher
                .update(cx, |launcher, cx| -> Result<usize, String> {
                    let count =
                        sift_state::Config::save_workspace_shortcuts(launcher.user.config_path())?;
                    launcher.reload_config(cx);
                    Ok(count)
                })?;
            self.notice = Some(format!(
                "Added {count} workspace shortcuts. Existing bindings were kept."
            ));
            return Ok(());
        }
        if let Action::AssignApp {
            workspace,
            app_id,
            open,
        } = &action
        {
            let app_name = self
                .launcher
                .read(cx)
                .apps
                .as_ref()
                .and_then(|apps| {
                    apps.catalog()
                        .apps()
                        .iter()
                        .find(|app| app.bundle_id.as_deref() == Some(app_id.as_str()))
                })
                .map(|app| app.name.to_string())
                .unwrap_or_else(|| app_id.clone());
            self.launcher
                .update(cx, |launcher, cx| -> Result<(), String> {
                    sift_state::Config::save_workspace(
                        launcher.user.config_path(),
                        workspace,
                        Some(app_id),
                    )?;
                    launcher.reload_config(cx);
                    Ok(())
                })?;
            let controller = self
                .launcher
                .read(cx)
                .window_manager
                .as_ref()
                .map(|r| r.controller());
            let controller = controller.filter(|c| {
                let snapshot = c.snapshot();
                snapshot.enabled
            });
            let active = controller.is_some();
            if let Some(controller) = controller {
                // Queue after configuration and after the UI update. Existing
                // windows move now; later windows use the persisted rule.
                let mut commands: Vec<_> = controller
                    .snapshot()
                    .windows
                    .iter()
                    .filter(|w| &w.app_id == app_id)
                    .map(|w| {
                        Command::Target(
                            w.id,
                            Box::new(Command::MoveToWorkspace(workspace.clone(), false)),
                        )
                    })
                    .collect();
                commands.push(Command::Workspace(workspace.clone()));
                controller.execute(Command::Sequence(commands))?;
            }
            // The workspace command launches a missing assigned app. Avoid
            // sending a second launch request for the same selection.
            let _ = open;
            let message = assignment_notice(&app_name, workspace, active);
            tracing::info!(app = %app_id, workspace, active, "workspace assignment saved");
            self.notice = Some(message.clone());
            return Ok(());
        }
        if let Action::UnassignApp { workspace, app_id } = &action {
            let app_name = self
                .launcher
                .read(cx)
                .apps
                .as_ref()
                .and_then(|apps| {
                    apps.catalog()
                        .apps()
                        .iter()
                        .find(|app| app.bundle_id.as_deref() == Some(app_id.as_str()))
                })
                .map(|app| app.name.to_string())
                .unwrap_or_else(|| app_id.clone());
            let removed = self
                .launcher
                .update(cx, |launcher, cx| -> Result<usize, String> {
                    let removed = sift_state::Config::remove_workspace_app(
                        launcher.user.config_path(),
                        app_id,
                    )?;
                    launcher.reload_config(cx);
                    Ok(removed)
                })?;
            if removed == 0 {
                self.notice = Some(format!(
                    "{app_name} is not assigned to Workspace {workspace}."
                ));
            } else {
                tracing::info!(app = %app_id, workspace, "workspace assignment removed");
                self.notice = Some(format!(
                    "{app_name} removed from Workspace {workspace}. Its windows stay where they are."
                ));
            }
            return Ok(());
        }
        self.window
            .update(cx, |launcher, window, cx| -> Result<(), String> {
                match action {
                    Action::Search(query) => {
                        launcher.show(window, cx);
                        launcher
                            .input
                            .update(cx, |input, cx| input.set_text(&query, cx));
                    }
                    Action::Builtin(id) => {
                        if id == commands::CHECK_UPDATES {
                            launcher.show(window, cx);
                        }
                        launcher.run_command(&id, cx);
                    }
                    Action::Setting(setting) => {
                        if matches!(setting, SettingAction::Tiling(true))
                            && !launcher.window_management_enabled()
                        {
                            launcher.change_setting(Setting::WindowManager(true), cx)?;
                        }
                        let setting = match setting {
                            SettingAction::Controls(on) => Setting::WindowManager(on),
                            SettingAction::Tiling(on) => Setting::WindowManagerAutomaticTiling(on),
                            SettingAction::Clipboard(on) => Setting::ClipboardEnabled(on),
                        };
                        launcher.change_setting(setting, cx)?;
                    }
                    Action::WindowCommand(..) => unreachable!("handled after the window update"),
                    Action::AddWorkspace(name) => {
                        sift_state::Config::save_workspace(
                            launcher.user.config_path(),
                            &name,
                            None,
                        )?;
                        launcher.reload_config(cx);
                    }
                    Action::AddWorkspaceShortcuts => unreachable!("handled with the menu state"),
                    Action::AssignApp { .. } => unreachable!("handled after the window update"),
                    Action::UnassignApp { .. } => unreachable!("handled with the menu state"),
                    Action::Reload => {
                        launcher.reload_config(cx);
                        launcher.retry_window_management(cx);
                    }
                    Action::Permission => launcher.request_window_permission(cx),
                    Action::Login(_) => unreachable!("handled with the menu state"),
                    Action::OpenLogs => {
                        let path =
                            crate::logging::log_dir().ok_or("Logs directory is unavailable")?;
                        let failures = crate::launch_failures();
                        sift_macos::open_path(&path, move |e| {
                            let _ = failures.unbounded_send(e);
                        })
                        .map_err(|e| e.to_string())?;
                    }
                    Action::ShowApp => {
                        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
                        let app = executable
                            .ancestors()
                            .find(|p| p.extension().is_some_and(|e| e == "app"))
                            .unwrap_or(&executable);
                        sift_macos::reveal_in_finder(app).map_err(|e| e.to_string())?;
                    }
                    Action::Help(anchor) => {
                        let url =
                            format!("https://github.com/kylianmarceau/sift/blob/main/{anchor}");
                        let failures = crate::launch_failures();
                        sift_macos::open_url(&url, move |e| {
                            let _ = failures.unbounded_send(e);
                        })
                        .map_err(|e| e.to_string())?;
                    }
                    Action::ClearError => {}
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?
    }
}

// Geometry publications are frequent during drags. Hash only information shown
// by this menu so those events do not rebuild it.
fn menu_key(
    launcher: &Launcher,
    error: Option<&str>,
    login: sift_macos::login_item::Status,
) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    error.hash(&mut hash);
    format!("{login:?}").hash(&mut hash);
    let snapshot = &launcher.window_snapshot;
    (
        snapshot.enabled,
        snapshot.automatic_tiling,
        snapshot.focused,
        snapshot.active_monitor,
        &snapshot.mode,
        &snapshot.visible,
        &snapshot.message,
    )
        .hash(&mut hash);
    for w in &snapshot.windows {
        (
            w.id,
            w.pid,
            &w.app_id,
            &w.app_name,
            &w.title,
            &w.workspace,
            w.minimized,
        )
            .hash(&mut hash);
    }
    for ws in &snapshot.workspaces {
        (&ws.name, ws.monitor).hash(&mut hash);
    }
    let config = launcher.user.window_manager_config();
    (
        config.enabled,
        config.automatic_tiling,
        &config.workspaces,
        config.bindings.get("main"),
    )
        .hash(&mut hash);
    for rule in &config.rules {
        (&rule.app_id, &rule.workspace, &rule.title, &rule.app_name).hash(&mut hash);
    }
    launcher.window_error.hash(&mut hash);
    launcher.window_manager.is_some().hash(&mut hash);
    launcher.clipboard_enabled().hash(&mut hash);
    launcher.clipboard_count().hash(&mut hash);
    launcher.catalog_loaded_at.hash(&mut hash);
    format!("{:?}", launcher.update_state()).hash(&mut hash);
    for root in &launcher.index_status.roots {
        (&root.path, root.entries, root.errors).hash(&mut hash);
        format!("{:?}", root.state).hash(&mut hash);
    }
    launcher.index_status.message.hash(&mut hash);
    hash.finish()
}

/// Menu state that lives outside the launcher.
struct Extras<'a> {
    error: Option<&'a str>,
    notice: Option<&'a str>,
    login: sift_macos::login_item::Status,
    panel_shortcut: Option<String>,
    clipboard_shortcut: Option<String>,
    picker: Vec<Item>,
}

/// The whole menu, top to bottom:
///
/// - messages, then Open Sift and clipboard search with their shortcuts;
/// - the workspaces, each a click away, with their apps and shortcuts, plus
///   every window and the focused window's actions;
/// - Window Management, Clipboard History and Search Folders settings;
/// - Settings, updates, login item, help and Quit.
fn model(launcher: &Launcher, extras: &Extras) -> (Indicator, Vec<Item>) {
    let snapshot = &launcher.window_snapshot;
    let config = launcher.user.window_manager_config();
    let mut items = Vec::new();
    if let Some(error) = extras.error {
        items.push(icon(
            Item::label(short(error, 100)),
            "exclamationmark.triangle",
        ));
        items.push(action("Dismiss", Action::ClearError));
        items.push(Item::separator());
    } else if let Some(notice) = extras.notice {
        items.push(icon(Item::label(short(notice, 100)), "checkmark.circle"));
        items.push(Item::separator());
    }
    items.push(detail(
        icon(
            action("Open Sift", Action::Search(String::new())),
            "magnifyingglass",
        ),
        extras.panel_shortcut.clone().unwrap_or_default(),
    ));
    if launcher.clipboard_enabled() {
        items.push(detail(
            icon(
                action("Search Clipboard History", Action::Search("clip ".into())),
                "doc.on.clipboard",
            ),
            extras.clipboard_shortcut.clone().unwrap_or_default(),
        ));
    }
    items.push(Item::separator());

    let running = launcher.window_manager.is_some();
    let mut indicator = Indicator::Glyph;
    if launcher.window_management_enabled() {
        if running {
            let names = workspace_names(config, snapshot);
            indicator = workspace_indicator(snapshot, &names);
            items.extend(workspace_section(snapshot, config, &names));
        } else {
            items.push(Item::header("Window Management"));
            items.push(icon(
                Item::label(short(&launcher.window_management_status(), 80)),
                "exclamationmark.triangle",
            ));
            items.push(action(
                "Grant Window Control Permission…",
                Action::Permission,
            ));
            items.push(action("Retry Window Controls", Action::Reload));
            items.push(action("Show This Sift App in Finder", Action::ShowApp));
        }
        items.push(Item::separator());
    }

    items.push(icon(
        window_management_menu(launcher, snapshot, config, extras.picker.clone()),
        "macwindow.on.rectangle",
    ));
    items.push(icon(
        detail(
            submenu(
                "Clipboard History",
                vec![
                    action("Search Clipboard History", Action::Search("clip ".into())),
                    check(
                        "Record Clipboard History",
                        Action::Setting(SettingAction::Clipboard(!launcher.clipboard_enabled())),
                        launcher.clipboard_enabled(),
                    ),
                    builtin("Clear Unpinned History", commands::CLEAR_CLIPBOARD),
                ],
            ),
            format!("{}", launcher.clipboard_count()),
        ),
        "clipboard",
    ));
    let roots = &launcher.index_status.roots;
    let mut index = vec![Item::label(format!(
        "{} items in {} folders",
        roots.iter().map(|r| r.entries).sum::<u64>(),
        roots.len()
    ))];
    if let Some(message) = &launcher.index_status.message {
        index.push(Item::label(short(message, 100)));
    }
    if !roots.is_empty() {
        index.push(Item::separator());
    }
    for root in roots {
        index.push(detail(
            Item::label(short(&root.path.display().to_string(), 55)),
            match &root.state {
                sift_files::RootState::Watching => format!("{} items", root.entries),
                sift_files::RootState::Indexing { entries } => format!("indexing {entries}…"),
                sift_files::RootState::Unavailable => "unavailable".into(),
                sift_files::RootState::Failed(e) => short(e, 40),
            },
        ));
    }
    index.push(Item::separator());
    index.push(builtin("Manage Search Folders…", commands::SETTINGS));
    items.push(icon(submenu("Search Folders", index), "folder"));
    items.push(Item::separator());

    items.push(builtin("Settings…", commands::SETTINGS));
    items.push(match launcher.update_state() {
        UpdateState::Checking => Item::label("Checking for Updates…"),
        UpdateState::Available(version) => builtin(
            &format!("Update to Sift {version}…"),
            commands::CHECK_UPDATES,
        ),
        _ => builtin("Check for Updates…", commands::CHECK_UPDATES),
    });
    items.push(match extras.login {
        sift_macos::login_item::Status::Unavailable => {
            detail(Item::label("Open at Login"), "install Sift.app to enable")
        }
        sift_macos::login_item::Status::NeedsApproval => detail(
            builtin("Open at Login…", commands::SETTINGS),
            "needs approval",
        ),
        login => check(
            "Open at Login",
            Action::Login(login != sift_macos::login_item::Status::On),
            login == sift_macos::login_item::Status::On,
        ),
    });
    let build = sift_macos::status_bar::build_number()
        .map(|n| format!(" (build {n})"))
        .unwrap_or_default();
    items.push(submenu(
        "Help",
        vec![
            Item::label(format!("Sift {}{build}", env!("CARGO_PKG_VERSION"))),
            Item::separator(),
            action(
                "Sift Guide and Keyboard Shortcuts",
                Action::Help("README.md#using-sift".into()),
            ),
            action(
                "Window Management Guide",
                Action::Help("docs/window-manager.md".into()),
            ),
            Item::separator(),
            builtin("Edit Configuration File…", commands::EDIT_CONFIG),
            action("Open Logs Folder", Action::OpenLogs),
            action("Show This Sift App in Finder", Action::ShowApp),
        ],
    ));
    items.push(Item::separator());
    items.push(builtin("Quit Sift", commands::QUIT));
    (indicator, items)
}

/// The current workspace, plus the others that have windows, in menu order.
fn workspace_indicator(snapshot: &Snapshot, names: &[String]) -> Indicator {
    let current = snapshot.visible.get(&snapshot.active_monitor);
    let mut labels: Vec<(String, bool)> = names
        .iter()
        .filter(|name| {
            current == Some(*name)
                || snapshot
                    .windows
                    .iter()
                    .any(|w| &w.workspace == *name && !w.minimized)
        })
        .map(|name| (short(name, 6), current == Some(name)))
        .collect();
    // A long row would crowd other menu bar items; keep the current one.
    if labels.len() > 8 {
        labels.retain(|(_, current)| *current);
    }
    Indicator::Workspaces {
        labels,
        paused: !snapshot.enabled,
    }
}

/// App names on a workspace, in the order their windows were found.
fn workspace_apps(snapshot: &Snapshot, name: &str) -> String {
    let mut seen = BTreeSet::new();
    snapshot
        .windows
        .iter()
        .filter(|w| w.workspace == name && seen.insert(w.app_name.as_str()))
        .map(|w| w.app_name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn workspace_section(snapshot: &Snapshot, config: &sift_wm::Config, names: &[String]) -> Vec<Item> {
    let paused = !snapshot.enabled;
    let mut items = vec![Item::header(match (paused, snapshot.mode.as_str()) {
        (true, _) => "Workspaces · Paused".to_owned(),
        (false, "main" | "") => "Workspaces".to_owned(),
        (false, mode) => format!("Workspaces · {} mode", short(mode, 12)),
    })];
    let bindings = config.bindings_for_mode("main").unwrap_or_default();
    for name in names {
        let mut item = detail(
            if paused {
                Item::label(name.clone())
            } else {
                action(
                    name.clone(),
                    Action::WindowCommand(Command::Workspace(name.clone()), None),
                )
            },
            short(&workspace_apps(snapshot, name), 40),
        );
        item.checked = snapshot.visible.values().any(|v| v == name);
        if let Some((key, modifiers)) = workspace_key(&bindings, name) {
            item.key = key;
            item.modifiers = modifiers;
        }
        items.push(item);
    }
    if paused {
        items.push(icon(
            action(
                "Resume Window Management",
                Action::WindowCommand(Command::Enable(true), None),
            ),
            "play",
        ));
        return items;
    }
    items.push(icon(windows_menu(snapshot, names), "macwindow"));
    if let Some(focused) = focused_menu(snapshot, config, names) {
        items.push(icon(focused, "scope"));
    }
    items
}

/// Every managed window, grouped by workspace. Choosing one focuses it.
fn windows_menu(snapshot: &Snapshot, names: &[String]) -> Item {
    let mut children = vec![action("Search Windows…", Action::Search("win ".into()))];
    for name in names {
        let windows: Vec<_> = snapshot
            .windows
            .iter()
            .filter(|w| &w.workspace == name)
            .collect();
        if windows.is_empty() {
            continue;
        }
        children.push(Item::separator());
        children.push(Item::header(format!("Workspace {name}")));
        for w in windows {
            let mut item = detail(
                action(
                    w.app_name.clone(),
                    Action::WindowCommand(Command::FocusId(w.id), Some(w.into())),
                ),
                format!(
                    "{}{}",
                    short(&w.title, 50),
                    if w.minimized { " (minimized)" } else { "" }
                ),
            );
            item.checked = snapshot.focused == Some(w.id);
            children.push(item);
        }
    }
    let count = snapshot.windows.len();
    detail(submenu("Windows", children), format!("{count}"))
}

/// Actions for the focused window: move it, assign its app, and arrange it.
fn focused_menu(snapshot: &Snapshot, config: &sift_wm::Config, names: &[String]) -> Option<Item> {
    let w = snapshot
        .windows
        .iter()
        .find(|w| Some(w.id) == snapshot.focused)?;
    let target = |command: Command| {
        Action::WindowCommand(Command::Target(w.id, Box::new(command)), Some(w.into()))
    };
    let mut children = vec![
        Item::label(short(
            &if w.title.is_empty() {
                w.app_name.clone()
            } else {
                format!("{} — {}", w.app_name, w.title)
            },
            70,
        )),
        Item::separator(),
    ];
    let mut moves: Vec<Item> = names
        .iter()
        .map(|name| {
            let mut item = if *name == w.workspace {
                Item::label(format!("Workspace {name}"))
            } else {
                action(
                    format!("Workspace {name}"),
                    target(Command::MoveToWorkspace(name.clone(), false)),
                )
            };
            item.checked = *name == w.workspace;
            item
        })
        .collect();
    let new = next_workspace(names);
    moves.push(Item::separator());
    moves.push(action(
        format!("New Workspace {new}"),
        target(Command::MoveToWorkspace(new, false)),
    ));
    children.push(submenu("Move to Workspace", moves));
    if !w.app_id.is_empty() {
        children.push(submenu(
            format!("Always Open {} In", short(&w.app_name, 30)),
            assign_menu(&w.app_name, &w.app_id, config, names, None),
        ));
    }
    children.push(Item::separator());
    let arrange: &[(&str, &str)] = if snapshot.automatic_tiling {
        &[
            ("Toggle Floating", "layout floating tiling"),
            ("Toggle Fullscreen", "fullscreen"),
        ]
    } else {
        &[
            ("Snap Left", "snap left"),
            ("Snap Right", "snap right"),
            ("Center", "snap center"),
            ("Maximize", "snap maximize"),
            ("Restore Size", "snap restore"),
            ("Toggle Fullscreen", "fullscreen"),
        ]
    };
    for (label, text) in arrange {
        if let Ok(command) = sift_wm::parse(text) {
            children.push(action(*label, target(command)));
        }
    }
    Some(detail(
        submenu("Focused Window", children),
        short(&w.app_name, 30),
    ))
}

/// Workspaces an app can be assigned to, with its current one checked and
/// a way to remove the assignment.
fn assign_menu(
    app_name: &str,
    app_id: &str,
    config: &sift_wm::Config,
    names: &[String],
    open: Option<PathBuf>,
) -> Vec<Item> {
    let current = saved_assignment(config, app_id);
    let mut items: Vec<Item> = names
        .iter()
        .map(|name| {
            check(
                &format!("Workspace {name}"),
                Action::AssignApp {
                    workspace: name.clone(),
                    app_id: app_id.to_owned(),
                    open: open.clone(),
                },
                current == Some(name.as_str()),
            )
        })
        .collect();
    if let Some(workspace) = current {
        items.push(Item::separator());
        items.push(action(
            format!("Don't Assign {}", short(app_name, 30)),
            Action::UnassignApp {
                workspace: workspace.to_owned(),
                app_id: app_id.to_owned(),
            },
        ));
    }
    items
}

/// Assigned apps first, then every installed app by first letter; each
/// opens its list of workspaces. Submenus are built when opened, so the
/// size of this tree does not slow the menu down.
fn app_picker(
    apps: Option<&sift_apps::AppProvider>,
    config: &sift_wm::Config,
    names: &[String],
) -> Vec<Item> {
    let catalog_name = |app_id: &str| {
        apps.and_then(|apps| {
            apps.catalog()
                .apps()
                .iter()
                .find(|app| app.bundle_id.as_deref() == Some(app_id))
        })
        .map(|app| app.name.to_string())
    };
    let mut items = Vec::new();
    let mut assigned: Vec<(String, String, String)> = names
        .iter()
        .flat_map(|name| {
            config.assigned_apps(name).into_iter().map(|app_id| {
                let app_name = catalog_name(&app_id).unwrap_or_else(|| app_id.clone());
                (app_name, app_id, name.clone())
            })
        })
        .collect();
    assigned.sort();
    assigned.dedup_by(|a, b| a.1 == b.1);
    if !assigned.is_empty() {
        items.push(Item::header("Assigned"));
        for (app_name, app_id, workspace) in &assigned {
            items.push(detail(
                submenu(
                    short(app_name, 35),
                    assign_menu(app_name, app_id, config, names, None),
                ),
                format!("Workspace {workspace}"),
            ));
        }
        items.push(Item::separator());
    }
    let Some(apps) = apps else {
        items.push(Item::label("Loading installed apps…"));
        return items;
    };
    items.push(Item::header("All Apps"));
    let mut groups = std::collections::BTreeMap::<String, Vec<Item>>::new();
    for app in apps.catalog().apps() {
        let Some(app_id) = &app.bundle_id else {
            continue;
        };
        if app_id == "io.github.kylianmarceau.sift" {
            continue;
        }
        let group = app
            .name
            .chars()
            .next()
            .filter(|c| c.is_alphabetic())
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "#".into());
        let mut entry = submenu(
            app.name.as_ref(),
            assign_menu(&app.name, app_id, config, names, Some(app.path.clone())),
        );
        if let Some(workspace) = saved_assignment(config, app_id) {
            entry = detail(entry, format!("Workspace {workspace}"));
        }
        groups.entry(group).or_default().push(entry);
    }
    for (letter, mut apps) in groups {
        apps.sort_by_cached_key(|a| a.title.to_lowercase());
        items.push(submenu(letter, apps));
    }
    items
}

fn window_management_menu(
    launcher: &Launcher,
    snapshot: &Snapshot,
    config: &sift_wm::Config,
    picker: Vec<Item>,
) -> Item {
    let running = launcher.window_manager.is_some();
    let mut children = vec![
        check(
            "Window Shortcuts and Controls",
            Action::Setting(SettingAction::Controls(
                !launcher.window_management_enabled(),
            )),
            launcher.window_management_enabled(),
        ),
        check(
            "Automatic Tiling",
            Action::Setting(SettingAction::Tiling(!launcher.automatic_tiling_enabled())),
            launcher.automatic_tiling_enabled(),
        ),
    ];
    if running {
        children.push(if snapshot.enabled {
            action(
                "Pause Window Management",
                Action::WindowCommand(Command::Enable(false), None),
            )
        } else {
            action(
                "Resume Window Management",
                Action::WindowCommand(Command::Enable(true), None),
            )
        });
    }
    children.push(Item::separator());
    children.push(submenu("Assign Apps to Workspaces", picker));
    let names = workspace_names(config, snapshot);
    if names.len() < 256 {
        let next = next_workspace(&names);
        children.push(action(
            format!("Add Workspace {next}"),
            Action::AddWorkspace(next),
        ));
    }
    children.push(action(
        "Add Missing Workspace Shortcuts",
        Action::AddWorkspaceShortcuts,
    ));
    children.push(Item::separator());
    children.push(builtin("Edit Rules and Shortcuts…", commands::EDIT_CONFIG));
    children.push(action("Reload Configuration", Action::Reload));
    children.push(action(
        "Window Management Guide",
        Action::Help("docs/window-manager.md".into()),
    ));
    let mut item = submenu("Window Management", children);
    if !running && launcher.window_management_enabled() {
        item = detail(item, "not running");
    } else if !launcher.window_management_enabled() {
        item = detail(item, "off");
    }
    item
}

/// Configured workspaces then any others in use: numbers first, in order.
fn workspace_names(config: &sift_wm::Config, snapshot: &Snapshot) -> Vec<String> {
    let mut names: Vec<_> = config
        .workspaces
        .iter()
        .cloned()
        .chain(snapshot.workspaces.iter().map(|ws| ws.name.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    names.sort_by(|a, b| match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        _ => a.cmp(b),
    });
    names
}

fn next_workspace(names: &[String]) -> String {
    (1u32..)
        .find(|n| !names.iter().any(|s| s == &n.to_string()))
        .expect("finite workspace list")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(workspaces: &[&str]) -> sift_wm::Engine {
        sift_wm::Engine::new(
            sift_wm::Config {
                automatic_tiling: true,
                workspaces: workspaces.iter().map(|w| w.to_string()).collect(),
                ..Default::default()
            },
            vec![sift_wm::Monitor {
                id: 1,
                name: "Display".into(),
                main: true,
                frame: sift_wm::Rect::new(0., 0., 1200., 800.),
                usable: sift_wm::Rect::new(0., 25., 1200., 775.),
            }],
        )
        .unwrap()
    }

    fn decode(item: &Item) -> Action {
        serde_json::from_str(item.action.as_ref().expect("an action")).unwrap()
    }

    fn find<'a>(items: &'a [Item], title: &str) -> &'a Item {
        items.iter().find(|i| i.title == title).unwrap_or_else(|| {
            panic!(
                "no {title:?} in {:?}",
                items.iter().map(|i| &i.title).collect::<Vec<_>>()
            )
        })
    }

    #[test]
    fn stale_window_identity_cannot_target_a_reused_id() {
        let mut w = ManagedWindow::new(42, 10, sift_wm::Rect::default());
        w.app_id = "com.example.one".into();
        let identity = WindowIdentity::from(&w);
        let mut snapshot = Snapshot {
            windows: vec![w],
            ..Default::default()
        };
        assert!(identity.exists(&snapshot));
        snapshot.windows[0].pid = 11;
        assert!(!identity.exists(&snapshot));
    }

    #[test]
    fn workspace_section_switches_with_shortcuts_and_preserves_unicode() {
        let mut engine = engine(&["1", "10", "dev \"é\""]);
        let config = sift_wm::Config::default();
        let snapshot = engine.snapshot();
        let names = workspace_names(&engine.config, &snapshot);
        let section = workspace_section(&snapshot, &config, &names);
        assert!(section[0].header);
        let entry = find(&section, "dev \"é\"");
        assert_eq!(
            decode(entry),
            Action::WindowCommand(Command::Workspace("dev \"é\"".into()), None)
        );
        // The default bindings put workspace 1 on Option-1.
        let one = find(&section, "1");
        assert_eq!((one.key.as_str(), one.modifiers), ("1", status_bar::OPTION));
        engine.enabled = false;
        let paused = workspace_section(&engine.snapshot(), &config, &names);
        assert!(paused[0].title.contains("Paused"));
        assert!(
            paused
                .iter()
                .filter(|i| names.contains(&i.title))
                .all(|i| i.action.is_none())
        );
        assert!(paused.iter().any(|i| i.title == "Resume Window Management"));
    }

    #[test]
    fn workspace_items_show_their_apps_and_the_current_one() {
        let mut a = ManagedWindow::new(1, 2, sift_wm::Rect::default());
        a.app_name = "Safari".into();
        a.workspace = "2".into();
        let mut b = a.clone();
        b.id = 3;
        b.app_name = "Terminal".into();
        let mut c = a.clone();
        c.id = 4;
        let snapshot = Snapshot {
            enabled: true,
            active_monitor: 1,
            visible: [(1, "2".to_owned())].into(),
            windows: vec![a, b, c],
            ..Default::default()
        };
        let names: Vec<String> = ["1", "2", "3"].map(String::from).into();
        let section = workspace_section(&snapshot, &sift_wm::Config::default(), &names);
        let two = find(&section, "2");
        assert!(two.checked);
        assert_eq!(two.detail.as_deref(), Some("Safari, Terminal"));
        assert_eq!(find(&section, "1").detail, None);
        assert_eq!(
            workspace_indicator(&snapshot, &names),
            Indicator::Workspaces {
                labels: vec![("2".into(), true)],
                paused: false
            }
        );
    }

    #[test]
    fn numeric_workspaces_extend_past_nine() {
        assert_eq!(
            next_workspace(&(1..=9).map(|n| n.to_string()).collect::<Vec<_>>()),
            "10"
        );
    }

    #[test]
    fn key_equivalents_only_for_single_keys_with_modifiers() {
        assert_eq!(
            key_equivalent("alt-1"),
            Some(("1".into(), status_bar::OPTION))
        );
        assert_eq!(
            key_equivalent("ctrl-alt-shift-K"),
            Some((
                "k".into(),
                status_bar::CONTROL | status_bar::OPTION | status_bar::SHIFT
            ))
        );
        assert_eq!(key_equivalent("ctrl-alt-left"), None);
        assert_eq!(key_equivalent("1"), None);
        assert_eq!(key_equivalent("hyper-1"), None);
    }

    #[test]
    fn manual_and_tiling_menus_offer_supported_actions() {
        let mut w = ManagedWindow::new(1, 2, sift_wm::Rect::default());
        w.workspace = "1".into();
        w.app_id = "com.example.editor".into();
        w.app_name = "Editor".into();
        let mut snapshot = Snapshot {
            enabled: true,
            focused: Some(1),
            windows: vec![w],
            ..Default::default()
        };
        let names: Vec<String> = ["1", "2"].map(String::from).into();
        let config = sift_wm::Config::default();
        let menu = focused_menu(&snapshot, &config, &names).unwrap();
        assert!(menu.children.iter().any(|i| i.title == "Snap Left"));
        assert!(!menu.children.iter().any(|i| i.title == "Toggle Floating"));
        let moves = find(&menu.children, "Move to Workspace");
        assert!(find(&moves.children, "Workspace 1").action.is_none());
        assert_eq!(
            decode(find(&moves.children, "Workspace 2")),
            Action::WindowCommand(
                Command::Target(1, Box::new(Command::MoveToWorkspace("2".into(), false))),
                Some(WindowIdentity {
                    id: 1,
                    pid: 2,
                    app_id: "com.example.editor".into()
                })
            )
        );
        assert!(moves.children.iter().any(|i| i.title == "New Workspace 3"));
        assert!(
            menu.children
                .iter()
                .any(|i| i.title == "Always Open Editor In")
        );
        snapshot.automatic_tiling = true;
        let menu = focused_menu(&snapshot, &config, &names).unwrap();
        assert!(!menu.children.iter().any(|i| i.title == "Snap Left"));
        assert!(menu.children.iter().any(|i| i.title == "Toggle Floating"));
        snapshot.focused = None;
        assert!(focused_menu(&snapshot, &config, &names).is_none());
    }

    fn catalog(dir: &std::path::Path, name: &str, id: &str) -> sift_apps::AppProvider {
        let contents = dir.join(format!("{name}.app/Contents"));
        std::fs::create_dir_all(&contents).unwrap();
        std::fs::write(contents.join("Info.plist"), format!(r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>{id}</string><key>CFBundleName</key><string>{name}</string></dict></plist>"#)).unwrap();
        sift_apps::AppProvider::new(sift_apps::AppCatalog::discover(&[
            sift_apps::Location::Folder(dir.into()),
        ]))
    }

    #[test]
    fn apps_are_assigned_from_one_picker_without_a_running_manager() {
        let dir = tempfile::tempdir().unwrap();
        let apps = catalog(dir.path(), "Test Editor", "com.example.editor");
        let names: Vec<String> = ["1", "2"].map(String::from).into();
        let picker = app_picker(Some(&apps), &sift_wm::Config::default(), &names);
        assert!(!picker.iter().any(|i| i.title == "Assigned"));
        let app = find(&find(&picker, "T").children, "Test Editor");
        assert_eq!(
            decode(find(&app.children, "Workspace 2")),
            Action::AssignApp {
                workspace: "2".into(),
                app_id: "com.example.editor".into(),
                open: Some(dir.path().join("Test Editor.app")),
            }
        );
        assert!(!app.children.iter().any(|i| i.title.starts_with("Don't")));
        assert!(assignment_notice("Editor", "1", false).contains("Nothing moved"));
    }

    #[test]
    fn assigned_apps_are_listed_first_and_removable_even_without_a_catalog() {
        let config = sift_state::Config::parse(
            "[[window_manager.rules]]\napp_id = '^com\\.example\\.gone$'\nworkspace = '2'\n[[window_manager.rules]]\napp_id = '^com\\.example\\.editor$'\nworkspace = '1'\n",
        );
        assert!(config.problems.is_empty());
        let config = &config.config.window_manager;
        let names: Vec<String> = ["1", "2", "3"].map(String::from).into();
        let picker = app_picker(None, config, &names);
        assert!(picker[0].header);
        let editor = find(&picker, "com.example.editor");
        assert_eq!(editor.detail.as_deref(), Some("Workspace 1"));
        assert!(find(&editor.children, "Workspace 1").checked);
        assert_eq!(
            decode(find(&editor.children, "Don't Assign com.example.editor")),
            Action::UnassignApp {
                workspace: "1".into(),
                app_id: "com.example.editor".into(),
            }
        );
        assert_eq!(
            find(&picker, "com.example.gone").detail.as_deref(),
            Some("Workspace 2")
        );
        assert!(picker.iter().any(|i| i.title == "Loading installed apps…"));

        let dir = tempfile::tempdir().unwrap();
        let apps = catalog(dir.path(), "Editor", "com.example.editor");
        let picker = app_picker(Some(&apps), config, &names);
        // The catalog name replaces the bundle identifier.
        assert_eq!(
            find(&picker, "Editor").detail.as_deref(),
            Some("Workspace 1")
        );
        let listed = find(&find(&picker, "E").children, "Editor");
        assert_eq!(listed.detail.as_deref(), Some("Workspace 1"));
    }
}
