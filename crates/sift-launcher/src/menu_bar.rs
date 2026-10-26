//! Sift's native status menu. Reads cached state and subscribes to publications;
//! no timer, shell command, AX call or process launch is used to build the menu.
use super::{
    IndexStatusChanged, Launcher, UpdateStateChanged, UserDataChanged, WindowManagerChanged,
};
use crate::{commands, updates::UpdateState};
use futures::{StreamExt, channel::mpsc::unbounded};
use gpui::{App, AppContext, Context, Entity, Global, Subscription, WindowHandle};
use serde::{Deserialize, Serialize};
use sift_macos::status_bar::{Item, MenuBar};
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
    AddWorkspace(String),
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

struct State {
    native: Option<MenuBar>,
    launcher: Entity<Launcher>,
    window: WindowHandle<Launcher>,
    error: Option<String>,
    key: Option<u64>,
    login: sift_macos::login_item::Status,
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
            key: None,
            login: sift_macos::login_item::status(),
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
        if let Some(native) = &self.native {
            let launcher = self.launcher.read(cx);
            let key = menu_key(launcher, self.error.as_deref(), self.login);
            if self.key == Some(key) {
                return;
            }
            let (title, items) = model(launcher, self.error.as_deref(), self.login);
            native.update(
                title,
                format!(
                    "Sift {} · {}",
                    env!("CARGO_PKG_VERSION"),
                    launcher.window_management_status()
                ),
                items,
            );
            self.key = Some(key);
        }
    }
    fn perform(&mut self, action: Action, cx: &mut Context<Self>) -> Result<(), String> {
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
            self.window
                .update(cx, |launcher, _, cx| launcher.hide(cx))
                .map_err(|e| e.to_string())?;
            // The window update has finished before native focus can change.
            return controller.execute(command.clone());
        }
        if let Action::AssignApp {
            workspace,
            app_id,
            open,
        } = &action
        {
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
            self.window
                .update(cx, |launcher, _, cx| launcher.hide(cx))
                .map_err(|e| e.to_string())?;
            if let Some(controller) = controller.filter(|c| {
                let snapshot = c.snapshot();
                snapshot.enabled && snapshot.automatic_tiling
            }) {
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
            if let Some(path) = open {
                let failures = crate::launch_failures();
                sift_macos::open_application(path, move |e| {
                    let _ = failures.unbounded_send(e);
                })
                .map_err(|e| e.to_string())?;
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
                    Action::AssignApp { .. } => unreachable!("handled after the window update"),
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
// by this menu so those events do not rebuild thousands of app-menu entries.
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
            w.floating,
            w.native_fullscreen,
        )
            .hash(&mut hash);
    }
    for ws in &snapshot.workspaces {
        (&ws.name, ws.monitor).hash(&mut hash);
    }
    serde_json::to_string(launcher.user.window_manager_config())
        .ok()
        .hash(&mut hash);
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

fn model(
    launcher: &Launcher,
    error: Option<&str>,
    login: sift_macos::login_item::Status,
) -> (String, Vec<Item>) {
    let snapshot = &launcher.window_snapshot;
    let mut items = vec![Item::label(format!("Sift {}", env!("CARGO_PKG_VERSION")))];
    if let Some(error) = error {
        items.push(Item::label(short(error, 100)));
        items.push(action("Dismiss Message", Action::ClearError));
    }
    items.extend([
        icon(
            action("Open Sift", Action::Search(String::new())),
            "magnifyingglass",
        ),
        icon(builtin("Settings…", commands::SETTINGS), "gearshape"),
        Item::separator(),
    ]);
    let mut title = String::new();
    if launcher.window_management_enabled() {
        if launcher.window_manager.is_none() {
            items.push(Item::label("Window controls are not running"));
            items.push(action(
                "Grant Window Control Permission…",
                Action::Permission,
            ));
            items.push(action("Show This Sift App in Finder", Action::ShowApp));
            items.push(action("Retry Window Controls", Action::Reload));
        } else {
            let paused = !snapshot.enabled;
            if paused {
                title = " Ⅱ".into();
            } else if snapshot.automatic_tiling {
                title = snapshot
                    .visible
                    .get(&snapshot.active_monitor)
                    .map(|s| format!(" {}", short(s, 10)))
                    .unwrap_or_default();
                if snapshot.mode != "main" {
                    title.push_str(&format!(" · {}", short(&snapshot.mode, 8)));
                }
            }
            items.push(Item::label(if paused {
                "Window Management Paused".into()
            } else {
                launcher.window_management_status()
            }));
            if snapshot.automatic_tiling {
                items.push(workspace_menu(snapshot));
            }
            items.push(window_menu(snapshot));
            items.push(action("Search Open Windows", Action::Search("win ".into())));
            items.push(action(
                if paused {
                    "Resume Window Management"
                } else {
                    "Pause Window Management"
                },
                Action::WindowCommand(Command::Enable(paused), None),
            ));
        }
    }
    let can_control =
        launcher.window_manager.is_some() && snapshot.enabled && snapshot.automatic_tiling;
    items.push(manage_workspaces(
        snapshot,
        launcher.user.window_manager_config(),
        launcher.apps.as_deref(),
        can_control,
    ));
    items.push(submenu(
        "Window Management",
        vec![
            check(
                "Window Shortcuts and Controls",
                Action::Setting(SettingAction::Controls(
                    !launcher.window_management_enabled(),
                )),
                launcher.window_management_enabled(),
            ),
            check(
                "Automatic Tiling and Workspaces",
                Action::Setting(SettingAction::Tiling(!launcher.automatic_tiling_enabled())),
                launcher.automatic_tiling_enabled(),
            ),
            builtin("Edit Rules and Shortcuts…", commands::EDIT_CONFIG),
            action("Reload Configuration", Action::Reload),
        ],
    ));
    items.push(Item::separator());
    items.push(submenu(
        format!("Clipboard History · {} entries", launcher.clipboard_count()),
        vec![
            action("Search Clipboard History", Action::Search("clip ".into())),
            check(
                "Record Clipboard History",
                Action::Setting(SettingAction::Clipboard(!launcher.clipboard_enabled())),
                launcher.clipboard_enabled(),
            ),
            builtin("Clear Unpinned History", commands::CLEAR_CLIPBOARD),
            builtin("Clipboard Settings…", commands::SETTINGS),
        ],
    ));
    let roots = &launcher.index_status.roots;
    let mut index = vec![Item::label(format!(
        "{} indexed items · {} folders",
        roots.iter().map(|r| r.entries).sum::<u64>(),
        roots.len()
    ))];
    if let Some(message) = &launcher.index_status.message {
        index.push(Item::label(short(message, 100)));
    }
    for root in roots {
        index.push(Item::label(format!(
            "{} · {}",
            short(&root.path.display().to_string(), 55),
            match &root.state {
                sift_files::RootState::Watching => format!("{} items", root.entries),
                sift_files::RootState::Indexing { entries } => format!("indexing {entries}…"),
                sift_files::RootState::Unavailable => "unavailable".into(),
                sift_files::RootState::Failed(e) => short(e, 40),
            }
        )));
    }
    index.push(builtin("Manage Search Folders…", commands::SETTINGS));
    items.push(submenu("File Index", index));
    items.push(Item::separator());
    items.push(match login {
        sift_macos::login_item::Status::Unavailable => {
            Item::label("Open at Login · install Sift.app to enable")
        }
        sift_macos::login_item::Status::NeedsApproval => {
            builtin("Open at Login · Approval Needed…", commands::SETTINGS)
        }
        _ => check(
            "Open at Login",
            Action::Login(login != sift_macos::login_item::Status::On),
            login == sift_macos::login_item::Status::On,
        ),
    });
    items.push(match launcher.update_state() {
        UpdateState::Checking => Item::label("Checking for Updates…"),
        UpdateState::Available(version) => builtin(
            &format!("Sift {version} Available…"),
            commands::CHECK_UPDATES,
        ),
        _ => builtin("Check for Updates…", commands::CHECK_UPDATES),
    });
    items.push(submenu(
        "Help",
        vec![
            action(
                "Sift Guide and Keyboard Shortcuts",
                Action::Help("README.md#using-sift".into()),
            ),
            action(
                "Window Management Guide",
                Action::Help("docs/window-manager.md".into()),
            ),
            builtin("Edit Configuration…", commands::EDIT_CONFIG),
            action("Reload Configuration", Action::Reload),
            action("Open Logs Folder", Action::OpenLogs),
            action("Show This Sift App in Finder", Action::ShowApp),
        ],
    ));
    items.push(Item::separator());
    items.push(builtin("Quit Sift", commands::QUIT));
    (title, items)
}

fn workspace_menu(snapshot: &Snapshot) -> Item {
    submenu(
        "Workspaces",
        ordered_workspaces(snapshot)
            .into_iter()
            .map(|ws| {
                let apps: BTreeSet<_> = snapshot
                    .windows
                    .iter()
                    .filter(|w| w.workspace == ws.name)
                    .map(|w| w.app_name.as_str())
                    .collect();
                let names = apps.into_iter().collect::<Vec<_>>().join(", ");
                let visible = snapshot.visible.get(&ws.monitor) == Some(&ws.name);
                let label = format!(
                    "{}{}{}",
                    ws.name,
                    if names.is_empty() { "" } else { " — " },
                    short(&names, 48)
                );
                let mut entry = if snapshot.enabled {
                    action(
                        label,
                        Action::WindowCommand(Command::Workspace(ws.name.clone()), None),
                    )
                } else {
                    Item::label(label)
                };
                entry.checked = visible;
                entry
            })
            .collect(),
    )
}

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

fn manage_workspaces(
    snapshot: &Snapshot,
    config: &sift_wm::Config,
    apps: Option<&sift_apps::AppProvider>,
    can_control: bool,
) -> Item {
    let mut workspaces = Vec::new();
    if !can_control {
        workspaces.push(Item::label(
            "Setup is available; workspace control is inactive",
        ));
    }
    let names = workspace_names(config, snapshot);
    for name in &names {
        let windows: Vec<_> = snapshot
            .windows
            .iter()
            .filter(|w| &w.workspace == name)
            .collect();
        let mut children = vec![Item::label(format!("{} windows", windows.len()))];
        if can_control {
            if let Some(focused) = snapshot
                .windows
                .iter()
                .find(|w| Some(w.id) == snapshot.focused)
            {
                children.push(action(
                    "Move Focused Window Here",
                    Action::WindowCommand(
                        Command::Target(
                            focused.id,
                            Box::new(Command::MoveToWorkspace(name.clone(), false)),
                        ),
                        Some(focused.into()),
                    ),
                ));
                if !focused.app_id.is_empty() {
                    children.push(action(
                        format!("Always Put {} Here", short(&focused.app_name, 35)),
                        Action::AssignApp {
                            workspace: name.clone(),
                            app_id: focused.app_id.clone(),
                            open: None,
                        },
                    ));
                }
            }
        }
        if let Some(apps) = apps {
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
                    .unwrap_or('#')
                    .to_uppercase()
                    .to_string();
                groups.entry(group).or_default().push(action(
                    app.name.as_ref(),
                    Action::AssignApp {
                        workspace: name.clone(),
                        app_id: app_id.clone(),
                        open: can_control.then(|| app.path.clone()),
                    },
                ));
            }
            children.push(submenu(
                if can_control {
                    "Add and Open App…"
                } else {
                    "Assign App…"
                },
                groups
                    .into_iter()
                    .map(|(name, mut apps)| {
                        apps.sort_by(|a, b| a.title.cmp(&b.title));
                        submenu(name, apps)
                    })
                    .collect(),
            ));
        } else {
            children.push(Item::label("Loading installed apps…"));
        }
        children.push(Item::separator());
        for w in windows {
            let label = format!(
                "{} — {}{}",
                w.app_name,
                short(&w.title, 50),
                if w.minimized { " (minimized)" } else { "" }
            );
            children.push(if can_control {
                action(
                    label,
                    Action::WindowCommand(Command::FocusId(w.id), Some(w.into())),
                )
            } else {
                Item::label(label)
            });
        }
        workspaces.push(submenu(format!("Workspace {name}"), children));
    }
    let name = next_workspace(&names);
    workspaces.push(Item::separator());
    if names.len() < 256 {
        workspaces.push(action(
            format!("Add Workspace {name}"),
            Action::AddWorkspace(name),
        ));
    }
    workspaces.push(builtin(
        "Edit Workspace Names and Rules…",
        commands::EDIT_CONFIG,
    ));
    submenu("Workspace Apps and Windows", workspaces)
}
fn next_workspace(names: &[String]) -> String {
    (1u32..)
        .find(|n| !names.iter().any(|s| s == &n.to_string()))
        .expect("finite workspace list")
        .to_string()
}
fn ordered_workspaces(snapshot: &Snapshot) -> Vec<&sift_wm::Workspace> {
    let mut workspaces: Vec<_> = snapshot.workspaces.iter().collect();
    workspaces.sort_by(
        |a, b| match (a.name.parse::<u64>(), b.name.parse::<u64>()) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        },
    );
    workspaces
}
fn window_menu(snapshot: &Snapshot) -> Item {
    let Some(w) = snapshot
        .windows
        .iter()
        .find(|w| Some(w.id) == snapshot.focused)
    else {
        return submenu(
            "Focused Window",
            vec![Item::label("No managed window is focused")],
        );
    };
    let mut entries = vec![Item::label(short(
        &format!("{} — {}", w.app_name, w.title),
        70,
    ))];
    if snapshot.enabled {
        let commands = [
            ("Snap Left", "snap left"),
            ("Snap Right", "snap right"),
            ("Center", "snap center"),
            ("Maximize", "snap maximize"),
            ("Restore Size", "snap restore"),
            ("Toggle Floating", "layout floating tiling"),
            ("Toggle Fullscreen", "fullscreen"),
        ];
        for (label, text) in commands {
            if snapshot.automatic_tiling && text.starts_with("snap") {
                continue;
            }
            if !snapshot.automatic_tiling && text.starts_with("layout") {
                continue;
            }
            if let Ok(command) = sift_wm::parse(text) {
                entries.push(action(
                    label,
                    Action::WindowCommand(Command::Target(w.id, Box::new(command)), Some(w.into())),
                ));
            }
        }
    }
    submenu("Focused Window", entries)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn workspace_labels_and_commands_preserve_unicode_and_quotes() {
        let mut engine = sift_wm::Engine::new(
            sift_wm::Config {
                automatic_tiling: true,
                workspaces: vec!["10".into(), "dev \"é\"".into()],
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
        .unwrap();
        let snapshot = engine.snapshot();
        let menu = workspace_menu(&snapshot);
        let entry = menu
            .children
            .iter()
            .find(|i| i.title.starts_with("dev"))
            .unwrap();
        let action: Action = serde_json::from_str(entry.action.as_ref().unwrap()).unwrap();
        assert_eq!(
            action,
            Action::WindowCommand(Command::Workspace("dev \"é\"".into()), None)
        );
        engine.enabled = false;
        assert!(
            workspace_menu(&engine.snapshot())
                .children
                .iter()
                .all(|i| i.action.is_none())
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
    fn manual_and_tiling_menus_offer_supported_actions() {
        let w = ManagedWindow::new(1, 2, sift_wm::Rect::default());
        let mut snapshot = Snapshot {
            enabled: true,
            focused: Some(1),
            windows: vec![w],
            ..Default::default()
        };
        assert!(
            window_menu(&snapshot)
                .children
                .iter()
                .any(|i| i.title == "Snap Left")
        );
        assert!(
            !window_menu(&snapshot)
                .children
                .iter()
                .any(|i| i.title == "Toggle Floating")
        );
        snapshot.automatic_tiling = true;
        assert!(
            !window_menu(&snapshot)
                .children
                .iter()
                .any(|i| i.title == "Snap Left")
        );
        assert!(
            window_menu(&snapshot)
                .children
                .iter()
                .any(|i| i.title == "Toggle Floating")
        );
    }
    #[test]
    fn workspace_setup_is_available_before_window_controls_start() {
        let config = sift_wm::Config::default();
        let menu = manage_workspaces(&Snapshot::default(), &config, None, false);
        assert!(menu.children.iter().any(|i| i.title == "Workspace 1"));
        let add = menu
            .children
            .iter()
            .find(|i| i.title == "Add Workspace 10")
            .expect("add workspace action");
        assert_eq!(
            serde_json::from_str::<Action>(add.action.as_ref().unwrap()).unwrap(),
            Action::AddWorkspace("10".into())
        );
    }

    #[test]
    fn offline_app_assignment_saves_without_launching() {
        let dir = tempfile::tempdir().unwrap();
        let contents = dir.path().join("Test Editor.app/Contents");
        std::fs::create_dir_all(&contents).unwrap();
        std::fs::write(contents.join("Info.plist"), r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.example.editor</string><key>CFBundleName</key><string>Test Editor</string></dict></plist>"#).unwrap();
        let catalog =
            sift_apps::AppCatalog::discover(&[sift_apps::Location::Folder(dir.path().into())]);
        let apps = sift_apps::AppProvider::new(catalog);
        let menu = manage_workspaces(
            &Snapshot::default(),
            &sift_wm::Config::default(),
            Some(&apps),
            false,
        );
        let workspace = menu
            .children
            .iter()
            .find(|i| i.title == "Workspace 1")
            .unwrap();
        let assign = workspace
            .children
            .iter()
            .find(|i| i.title == "Assign App…")
            .unwrap();
        let entry = &assign.children[0].children[0];
        assert_eq!(
            serde_json::from_str::<Action>(entry.action.as_ref().unwrap()).unwrap(),
            Action::AssignApp {
                workspace: "1".into(),
                app_id: "com.example.editor".into(),
                open: None,
            }
        );
        assert!(
            !workspace
                .children
                .iter()
                .any(|i| i.title == "Move Focused Window Here")
        );
    }
}
