use crate::config::{compile, validate_name};
use crate::tree::Node;
use crate::{Axis, Command, Config, Direction, Error, LayoutRequest, Rect, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[path = "manual.rs"]
mod manual;

pub type WindowId = u32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub id: u32,
    pub name: String,
    /// Entire display and the usable area excluding menu bar and Dock.
    pub frame: Rect,
    pub usable: Rect,
    pub main: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub id: WindowId,
    pub pid: i32,
    pub app_id: String,
    pub app_name: String,
    pub title: String,
    pub frame: Rect,
    pub original_frame: Rect,
    pub workspace: String,
    pub floating: bool,
    pub minimized: bool,
    pub native_fullscreen: bool,
    pub hidden: bool,
}

impl Window {
    pub fn is_tiled(&self) -> bool {
        !self.floating && self.available()
    }
    pub fn available(&self) -> bool {
        !self.minimized && !self.native_fullscreen && !self.hidden
    }
    pub fn new(id: WindowId, pid: i32, frame: Rect) -> Self {
        Self {
            id,
            pid,
            frame,
            original_frame: frame,
            app_id: String::new(),
            app_name: String::new(),
            title: String::new(),
            workspace: String::new(),
            floating: false,
            minimized: false,
            native_fullscreen: false,
            hidden: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub monitor: u32,
    pub focused: Option<WindowId>,
    pub fullscreen: Option<WindowId>,
    pub(crate) tree: Node,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: u64,
    pub enabled: bool,
    #[serde(default)]
    pub automatic_tiling: bool,
    pub mode: String,
    pub focused: Option<WindowId>,
    pub active_monitor: u32,
    pub windows: Vec<Window>,
    pub workspaces: Vec<Workspace>,
    pub monitors: Vec<Monitor>,
    pub visible: BTreeMap<u32, String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    LaunchTarget {
        name: String,
        app_id: String,
        command: Box<Command>,
    },
    Cursor(Rect, bool),
    Frame(WindowId, Rect),
    Focus(WindowId),
    Raise(WindowId),
    Close(WindowId),
    Minimize(WindowId),
    NativeFullscreen(WindowId, bool),
    Exec(String),
    Mode(String),
}

struct CompiledRule {
    app_id: Option<regex::Regex>,
    app_name: Option<regex::Regex>,
    title: Option<regex::Regex>,
    workspace: Option<String>,
    floating: Option<bool>,
    further: bool,
}

impl CompiledRule {
    fn matches(&self, w: &Window) -> bool {
        self.app_id.as_ref().is_none_or(|r| r.is_match(&w.app_id))
            && self
                .app_name
                .as_ref()
                .is_none_or(|r| r.is_match(&w.app_name))
            && self.title.as_ref().is_none_or(|r| r.is_match(&w.title))
    }
}

pub struct Engine {
    revision: u64,
    pub config: Config,
    pub windows: BTreeMap<WindowId, Window>,
    pub workspaces: BTreeMap<String, Workspace>,
    pub monitors: BTreeMap<u32, Monitor>,
    pub visible: BTreeMap<u32, String>,
    pub focused: Option<WindowId>,
    pub active_monitor: u32,
    pub mode: String,
    pub enabled: bool,
    previous_window: Option<WindowId>,
    previous_workspace: BTreeMap<u32, String>,
    rules: Vec<CompiledRule>,
    include_apps: Vec<regex::Regex>,
    exclude_apps: Vec<regex::Regex>,
    targets: BTreeMap<String, CompiledTarget>,
    focus_order: BTreeMap<WindowId, u64>,
    focus_serial: u64,
    manual_restore: BTreeMap<WindowId, Rect>,
    manual_fullscreen: BTreeMap<WindowId, Rect>,
}

struct CompiledTarget {
    app_id: Option<String>,
    app_name: Option<regex::Regex>,
    title: Option<regex::Regex>,
}

impl Engine {
    pub fn new(config: Config, monitors: Vec<Monitor>) -> Result<Self> {
        config.validate()?;
        let mut engine = Self {
            revision: 0,
            enabled: true,
            config,
            windows: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            monitors: BTreeMap::new(),
            visible: BTreeMap::new(),
            focused: None,
            active_monitor: 0,
            mode: "main".into(),
            previous_window: None,
            previous_workspace: BTreeMap::new(),
            rules: Vec::new(),
            include_apps: Vec::new(),
            exclude_apps: Vec::new(),
            targets: BTreeMap::new(),
            focus_order: BTreeMap::new(),
            focus_serial: 0,
            manual_restore: BTreeMap::new(),
            manual_fullscreen: BTreeMap::new(),
        };
        engine.compile_rules()?;
        engine.set_monitors(monitors)?;
        Ok(engine)
    }

    fn compile_rules(&mut self) -> Result<()> {
        self.targets = self
            .config
            .targets
            .iter()
            .map(|(name, target)| {
                Ok((
                    name.clone(),
                    CompiledTarget {
                        app_id: target.app_id.clone(),
                        app_name: target.app_name.as_deref().map(compile).transpose()?,
                        title: target.title.as_deref().map(compile).transpose()?,
                    },
                ))
            })
            .collect::<Result<_>>()?;
        self.include_apps = self
            .config
            .include_apps
            .iter()
            .map(|r| compile(r))
            .collect::<Result<_>>()?;
        self.exclude_apps = self
            .config
            .exclude_apps
            .iter()
            .map(|r| compile(r))
            .collect::<Result<_>>()?;
        self.rules = self
            .config
            .rules
            .iter()
            .map(|r| {
                Ok(CompiledRule {
                    app_id: r.app_id.as_deref().map(compile).transpose()?,
                    app_name: r.app_name.as_deref().map(compile).transpose()?,
                    title: r.title.as_deref().map(compile).transpose()?,
                    workspace: r.workspace.clone(),
                    floating: r.floating,
                    further: r.check_further,
                })
            })
            .collect::<Result<_>>()?;
        Ok(())
    }

    pub fn reconfigure(&mut self, config: Config) -> Result<()> {
        config.validate()?;
        let was_automatic = self.config.automatic_tiling;
        let automatic = config.automatic_tiling;
        if automatic != was_automatic {
            self.manual_restore.clear();
            self.manual_fullscreen.clear();
            for window in self.windows.values_mut() {
                if automatic {
                    // Tiling takes ownership of the current manual positions.
                    window.original_frame = window.frame;
                } else {
                    window.frame = window.original_frame;
                }
            }
        }
        self.config = config;
        self.compile_rules()?;
        self.set_monitors(self.monitors.values().cloned().collect())?;
        if automatic && !was_automatic {
            let windows = std::mem::take(&mut self.windows);
            for workspace in self.workspaces.values_mut() {
                workspace.tree =
                    Node::container(self.config.default_orientation, self.config.default_layout);
                workspace.focused = None;
                workspace.fullscreen = None;
            }
            for window in windows.into_values() {
                self.add_window(window)?;
            }
        }
        self.mode = "main".into();
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn manages_app(&self, app_id: &str) -> bool {
        (self.include_apps.is_empty() || self.include_apps.iter().any(|r| r.is_match(app_id)))
            && !self.exclude_apps.iter().any(|r| r.is_match(app_id))
    }

    pub fn set_monitors(&mut self, monitors: Vec<Monitor>) -> Result<()> {
        if monitors.is_empty()
            || monitors
                .iter()
                .any(|m| !m.frame.valid() || !m.usable.valid())
        {
            return Err(Error("no usable displays".into()));
        }
        let fallback = monitors.iter().find(|m| m.main).unwrap_or(&monitors[0]).id;
        self.monitors = monitors.into_iter().map(|m| (m.id, m)).collect();
        if !self.monitors.contains_key(&self.active_monitor) {
            self.active_monitor = fallback;
        }
        self.visible.retain(|id, _| self.monitors.contains_key(id));
        for workspace in self.workspaces.values_mut() {
            if !self.monitors.contains_key(&workspace.monitor) {
                workspace.monitor = fallback;
            }
        }
        for name in self.config.workspaces.clone() {
            self.ensure_workspace(&name)?;
        }
        let forced: Vec<_> = self.config.workspace_monitors.clone().into_iter().collect();
        for (name, selector) in forced {
            if let Ok(monitor) = self.select_monitor(&selector) {
                self.ensure_workspace(&name)?;
                if let Some(workspace) = self.workspaces.get_mut(&name) {
                    workspace.monitor = monitor;
                }
            }
        }
        self.visible.retain(|monitor, name| {
            self.workspaces
                .get(name)
                .is_some_and(|w| w.monitor == *monitor)
        });
        let ids: Vec<_> = self.monitors.keys().copied().collect();
        for id in ids {
            if self.visible.contains_key(&id) {
                continue;
            }
            let name = self
                .workspaces
                .values()
                .find(|w| w.monitor == id && !self.visible.values().any(|n| n == &w.name))
                .map(|w| w.name.clone());
            let name = match name {
                Some(n) => n,
                None => {
                    let mut n = format!("display-{id}");
                    while self.workspaces.contains_key(&n) {
                        n.push('-');
                    }
                    self.ensure_workspace(&n)?;
                    if let Some(w) = self.workspaces.get_mut(&n) {
                        w.monitor = id;
                    }
                    n
                }
            };
            self.visible.insert(id, name);
        }
        // A display may have vanished while a floating window was there.
        for window in self
            .windows
            .values_mut()
            .filter(|w| w.floating && self.config.automatic_tiling)
        {
            if let Some(m) = self
                .workspaces
                .get(&window.workspace)
                .and_then(|w| self.monitors.get(&w.monitor))
            {
                window.frame = window.frame.constrained(m.usable);
            }
        }
        Ok(())
    }

    fn ensure_workspace(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        if !self.workspaces.contains_key(name) {
            if self.workspaces.len() >= 256 {
                return Err(Error("workspace limit reached".into()));
            }
            self.workspaces.insert(
                name.into(),
                Workspace {
                    name: name.into(),
                    monitor: self.active_monitor,
                    focused: None,
                    fullscreen: None,
                    tree: Node::container(
                        self.config.default_orientation,
                        self.config.default_layout,
                    ),
                },
            );
        }
        Ok(())
    }

    fn current_name(&self) -> Result<String> {
        self.visible
            .get(&self.active_monitor)
            .cloned()
            .ok_or_else(|| Error("no active workspace".into()))
    }

    pub fn add_window(&mut self, mut window: Window) -> Result<()> {
        if self.windows.contains_key(&window.id) {
            self.update_window(window);
            return Ok(());
        }
        if !window.frame.valid() {
            return Err(Error("invalid window frame".into()));
        }
        if self.windows.len() >= 4096 {
            return Err(Error("managed window limit reached".into()));
        }
        let (x, y) = window.frame.center();
        let monitor = self
            .monitors
            .values()
            .find(|m| m.frame.contains(x, y))
            .map(|m| m.id)
            .unwrap_or(self.active_monitor);
        let mut target = self
            .visible
            .get(&monitor)
            .cloned()
            .unwrap_or(self.current_name()?);
        for rule in &self.rules {
            if !self.config.automatic_tiling {
                break;
            }
            if rule.matches(&window) {
                if let Some(w) = &rule.workspace {
                    target.clone_from(w);
                }
                if let Some(floating) = rule.floating {
                    window.floating = floating;
                }
                if !rule.further {
                    break;
                }
            }
        }
        self.ensure_workspace(&target)?;
        window.workspace = target.clone();
        let id = window.id;
        let floating = !window.is_tiled();
        self.windows.insert(id, window);
        if let Some(w) = self.workspaces.get_mut(&target) {
            if !floating {
                w.tree.insert(id, w.focused);
            }
            w.focused = Some(id);
        }
        Ok(())
    }

    /// Updates metadata without applying routing rules again. A window
    /// parked for another workspace must never be interpreted as user-moved.
    pub fn update_window(&mut self, observed: Window) {
        let Some(old) = self.windows.get_mut(&observed.id) else {
            return;
        };
        old.title = observed.title;
        let was_tiled = old.is_tiled();
        old.minimized = observed.minimized;
        old.native_fullscreen = observed.native_fullscreen;
        old.hidden = observed.hidden;
        let is_tiled = old.is_tiled();
        if (!self.config.automatic_tiling
            || (old.floating && self.visible.values().any(|n| n == &old.workspace)))
            && observed.frame.valid()
        {
            old.frame = observed.frame;
        }
        if let Some(w) = self.workspaces.get_mut(&old.workspace) {
            if was_tiled && !is_tiled {
                w.tree.remove(old.id);
            } else if !was_tiled && is_tiled {
                w.tree.insert(old.id, w.focused);
            }
        }
    }

    pub fn remove_window(&mut self, id: WindowId) {
        self.focus_order.remove(&id);
        self.manual_restore.remove(&id);
        self.manual_fullscreen.remove(&id);
        let Some(window) = self.windows.remove(&id) else {
            return;
        };
        if let Some(workspace) = self.workspaces.get_mut(&window.workspace) {
            workspace.tree.remove(id);
            if workspace.fullscreen == Some(id) {
                workspace.fullscreen = None;
            }
            if workspace.focused == Some(id) {
                workspace.focused = workspace.tree.first();
            }
        }
        if self.previous_window == Some(id) {
            self.previous_window = None;
        }
        if self.focused == Some(id) {
            self.focused = if self.config.automatic_tiling {
                self.current_name()
                    .ok()
                    .and_then(|n| self.workspaces.get(&n))
                    .and_then(|w| w.focused)
            } else {
                None
            };
        }
    }

    pub fn observed_focus(&mut self, id: WindowId) -> Result<()> {
        let previous = self.focused;
        let Some(window) = self.windows.get(&id) else {
            return Ok(());
        };
        let workspace = window.workspace.clone();
        // Activation via Dock or another launcher reveals the owning workspace.
        if self.config.automatic_tiling {
            self.switch_workspace(&workspace)?;
        } else {
            self.active_monitor = self.window_monitor(id)?;
        }
        self.set_focus(id);
        if previous != Some(id) {
            self.previous_window = previous;
        }
        Ok(())
    }

    pub fn cursor_destination(&self, target: crate::CursorTarget) -> Result<Rect> {
        if target.window() {
            let window = self
                .focused
                .and_then(|id| self.windows.get(&id))
                .filter(|w| w.available() && self.visible.values().any(|name| name == &w.workspace))
                .ok_or_else(|| Error("no visible focused window".into()))?;
            Ok(window.frame)
        } else {
            self.monitors
                .get(&self.active_monitor)
                .map(|m| m.frame)
                .ok_or_else(|| Error("no active monitor".into()))
        }
    }

    /// Commit a completed native gesture. Clicking/selecting content does not
    /// rearrange the tree because the native frame has not changed.
    pub fn commit_drag(
        &mut self,
        id: WindowId,
        before: Rect,
        after: Rect,
        x: f64,
        y: f64,
    ) -> Result<Vec<Effect>> {
        if !self.enabled
            || !self.config.automatic_tiling
            || !before.valid()
            || !after.valid()
            || !x.is_finite()
            || !y.is_finite()
        {
            return Ok(Vec::new());
        }
        let Some(window) = self.windows.get(&id) else {
            return Ok(Vec::new());
        };
        if !window.is_tiled() || before.nearly(after) {
            return Ok(Vec::new());
        }
        let name = window.workspace.clone();
        let width = after.width - before.width;
        let height = after.height - before.height;
        if width.abs() >= 1. || height.abs() >= 1. {
            self.resize(id, &name, Axis::Horizontal, width);
            self.resize(id, &name, Axis::Vertical, height);
        } else {
            let Some(monitor) = self
                .monitors
                .values()
                .find(|m| m.frame.contains(x, y))
                .map(|m| m.id)
            else {
                return Ok(Vec::new());
            };
            let destination = self
                .visible
                .get(&monitor)
                .cloned()
                .ok_or_else(|| Error("drop monitor has no workspace".into()))?;
            let target = self
                .windows
                .values()
                .filter(|w| {
                    w.id != id
                        && w.workspace == destination
                        && w.is_tiled()
                        && w.frame.contains(x, y)
                })
                .min_by(|a, b| {
                    (a.frame.width * a.frame.height).total_cmp(&(b.frame.width * b.frame.height))
                })
                .map(|w| (w.id, w.frame));
            self.move_to_workspace(id, &destination, true)?;
            if let Some((target, frame)) = target {
                let edges = [
                    ((x - frame.x) / frame.width, crate::Direction::Left),
                    ((frame.right() - x) / frame.width, crate::Direction::Right),
                    ((y - frame.y) / frame.height, crate::Direction::Up),
                    ((frame.bottom() - y) / frame.height, crate::Direction::Down),
                ];
                let direction = edges
                    .into_iter()
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .expect("edges")
                    .1;
                if let Some(workspace) = self.workspaces.get_mut(&destination) {
                    workspace.tree.remove(id);
                    if !workspace.tree.insert_relative(id, target, direction) {
                        workspace.tree.insert(id, None);
                    }
                }
            }
        }
        self.observed_focus(id)?;
        let mut effects = self.reflow();
        effects.push(Effect::Focus(id));
        Ok(effects)
    }

    fn set_focus(&mut self, id: WindowId) {
        self.focus_serial = self.focus_serial.wrapping_add(1);
        self.focus_order.insert(id, self.focus_serial);
        if self.focused != Some(id) {
            self.previous_window = self.focused;
            self.focused = Some(id);
        }
        if let Some(w) = self
            .windows
            .get(&id)
            .and_then(|w| self.workspaces.get_mut(&w.workspace))
        {
            w.focused = Some(id);
        }
    }

    fn workspace_target(&self, target: &str) -> Result<String> {
        if !matches!(target, "next" | "prev") {
            validate_name(target)?;
            return Ok(target.into());
        }
        let current = self.current_name()?;
        let mut names = self.config.workspaces.clone();
        names.extend(
            self.workspaces
                .keys()
                .filter(|n| !self.config.workspaces.contains(n))
                .cloned(),
        );
        let pos = names.iter().position(|n| n == &current).unwrap_or(0);
        let next = if target == "next" {
            (pos + 1) % names.len()
        } else {
            (pos + names.len() - 1) % names.len()
        };
        Ok(names[next].clone())
    }

    fn switch_workspace(&mut self, name: &str) -> Result<()> {
        let previous_focus = self.focused;
        self.ensure_workspace(name)?;
        let monitor = self.workspaces[name].monitor;
        if self.visible.get(&monitor).is_none_or(|n| n != name) {
            if let Some(previous) = self.visible.insert(monitor, name.into()) {
                self.previous_workspace.insert(monitor, previous);
            }
        }
        self.active_monitor = monitor;
        self.focused = self.workspaces[name].focused;
        if self.focused != previous_focus {
            self.previous_window = previous_focus;
        }
        Ok(())
    }

    fn move_to_workspace(&mut self, id: WindowId, target: &str, follow: bool) -> Result<()> {
        self.ensure_workspace(target)?;
        let window = self
            .windows
            .get_mut(&id)
            .ok_or_else(|| Error("no focused window".into()))?;
        if window.workspace == target {
            return Ok(());
        }
        let old = std::mem::replace(&mut window.workspace, target.into());
        let tiled = window.is_tiled();
        if window.floating {
            let from = self
                .workspaces
                .get(&old)
                .and_then(|w| self.monitors.get(&w.monitor));
            let to = self
                .workspaces
                .get(target)
                .and_then(|w| self.monitors.get(&w.monitor));
            if let (Some(from), Some(to)) = (from, to) {
                if from.id != to.id {
                    window.frame.x += to.usable.x - from.usable.x;
                    window.frame.y += to.usable.y - from.usable.y;
                    window.frame = window.frame.constrained(to.usable);
                }
            }
        }
        if let Some(w) = self.workspaces.get_mut(&old) {
            w.tree.remove(id);
            if w.fullscreen == Some(id) {
                w.fullscreen = None;
            }
            if w.focused == Some(id) {
                w.focused = w.tree.first();
            }
        }
        if let Some(w) = self.workspaces.get_mut(target) {
            if tiled {
                w.tree.insert(id, w.focused);
            }
            w.focused = Some(id);
        }
        if follow {
            self.switch_workspace(target)?;
            self.set_focus(id);
        } else {
            self.focused = self.workspaces.get(&old).and_then(|w| w.focused);
        }
        Ok(())
    }

    fn select_monitor(&self, selector: &str) -> Result<u32> {
        let mut monitors: Vec<_> = self.monitors.values().collect();
        monitors.sort_by(|a, b| {
            a.frame
                .x
                .total_cmp(&b.frame.x)
                .then(a.frame.y.total_cmp(&b.frame.y))
        });
        let current = monitors
            .iter()
            .position(|m| m.id == self.active_monitor)
            .unwrap_or(0);
        let selected = match selector {
            "next" => monitors.get((current + 1) % monitors.len()),
            "prev" => monitors.get((current + monitors.len() - 1) % monitors.len()),
            "main" => monitors.iter().find(|m| m.main),
            "secondary" => monitors.iter().find(|m| !m.main),
            "left" | "right" | "up" | "down" => {
                let d = match selector {
                    "left" => Direction::Left,
                    "right" => Direction::Right,
                    "up" => Direction::Up,
                    _ => Direction::Down,
                };
                let origin = self.monitors[&self.active_monitor].frame;
                monitors
                    .iter()
                    .filter(|m| m.id != self.active_monitor)
                    .filter_map(|m| directional_distance(origin, m.frame, d).map(|s| (s, m)))
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .map(|(_, m)| m)
            }
            _ => monitors.iter().find(|m| {
                m.id.to_string() == selector
                    || m.name.to_lowercase().contains(&selector.to_lowercase())
            }),
        };
        selected
            .map(|m| m.id)
            .ok_or_else(|| Error(format!("no monitor matches {selector:?}")))
    }

    fn neighbor(
        &self,
        id: WindowId,
        direction: Direction,
        floating: Option<bool>,
    ) -> Option<WindowId> {
        let w = self.windows.get(&id)?;
        self.windows
            .values()
            .filter(|other| other.id != id && other.workspace == w.workspace && other.available())
            .filter(|other| floating.is_none_or(|floating| other.floating == floating))
            .filter_map(|other| {
                directional_distance(w.frame, other.frame, direction).map(|d| (d, other.id))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
            .map(|(_, id)| id)
    }

    pub fn execute(&mut self, command: Command) -> Result<Vec<Effect>> {
        if let Command::FocusTarget(name) = &command {
            return self.execute_named(name.clone(), None);
        }
        if let Command::NamedTarget(name, inner) = &command {
            return self.execute_named(name.clone(), Some((**inner).clone()));
        }
        if !self.config.automatic_tiling {
            return self.execute_manual(command);
        }
        let previous_focus = self.focused;
        let explicit_focus = matches!(
            &command,
            Command::Focus(_)
                | Command::FocusId(_)
                | Command::FocusBack
                | Command::Workspace(_)
                | Command::WorkspaceBack
                | Command::SummonWorkspace(_)
                | Command::FocusMonitor(_)
                | Command::MoveToWorkspace(_, true)
                | Command::MoveToMonitor(_, true)
        );
        let mut effects = Vec::new();
        match command {
            Command::Target(id, command) => {
                if !self.windows.contains_key(&id) {
                    return Err(Error("window no longer exists".into()));
                }
                if command.is_query()
                    || matches!(
                        *command,
                        Command::Target(_, _)
                            | Command::Sequence(_)
                            | Command::Workspace(_)
                            | Command::SummonWorkspace(_)
                    )
                {
                    return Err(Error("this command does not accept --window-id".into()));
                }
                let previous = self.focused;
                let previous_window = self.previous_window;
                let monitor = self.active_monitor;
                self.focused = Some(id);
                let follow = matches!(
                    *command,
                    Command::Focus(_)
                        | Command::FocusId(_)
                        | Command::FocusBack
                        | Command::MoveToWorkspace(_, true)
                        | Command::MoveToMonitor(_, true)
                );
                let result = self.execute(*command);
                if !follow {
                    self.focused = previous.filter(|id| self.windows.contains_key(id));
                    self.previous_window = previous_window;
                    self.active_monitor = monitor;
                }
                return result.map(|mut effects| {
                    if !follow {
                        effects.retain(|e| !matches!(e, Effect::Focus(_)));
                    }
                    effects
                });
            }
            Command::Sequence(commands) => {
                return self.execute_sequence(commands);
            }
            Command::Enable(enabled) => {
                self.enabled = enabled;
            }
            Command::Snap(_) => {
                return Err(Error(
                    "snap requires manual mode; turn Automatic Tiling off".into(),
                ));
            }
            Command::Mode(mode) => {
                if !self.config.bindings.contains_key(&mode) {
                    return Err(Error(format!("unknown binding mode {mode:?}")));
                }
                self.mode = mode.clone();
                effects.push(Effect::Mode(mode));
            }
            Command::Exec(shell) => effects.push(Effect::Exec(shell)),
            command if command.is_query() || command == Command::Reload => return Ok(effects),
            _ if !self.enabled => {
                return Err(Error("window management is paused; use enable on".into()));
            }
            Command::MoveMouse(target) => effects.push(Effect::Cursor(
                self.cursor_destination(target)?,
                target.lazy(),
            )),
            Command::Workspace(target) => {
                let target = self.workspace_target(&target)?;
                self.switch_workspace(&target)?;
            }
            Command::SummonWorkspace(target) => {
                self.ensure_workspace(&target)?;
                if self.config.workspace_monitors.contains_key(&target) {
                    return Err(Error("workspace has a fixed monitor assignment".into()));
                }
                let old_monitor = self.workspaces[&target].monitor;
                let current = self.current_name()?;
                if let Some(workspace) = self.workspaces.get_mut(&target) {
                    workspace.monitor = self.active_monitor;
                }
                if old_monitor != self.active_monitor
                    && self.visible.get(&old_monitor) == Some(&target)
                {
                    if let Some(workspace) = self.workspaces.get_mut(&current) {
                        workspace.monitor = old_monitor;
                    }
                    self.visible.insert(old_monitor, current);
                }
                self.switch_workspace(&target)?;
            }
            Command::WorkspaceBack => {
                let target = self
                    .previous_workspace
                    .get(&self.active_monitor)
                    .cloned()
                    .ok_or_else(|| Error("no previous workspace".into()))?;
                self.switch_workspace(&target)?;
            }
            Command::FocusId(id) => {
                if !self.windows.contains_key(&id) {
                    return Err(Error("window no longer exists".into()));
                }
                self.observed_focus(id)?;
            }
            Command::FocusBack => {
                if let Some(id) = self
                    .previous_window
                    .filter(|id| self.windows.contains_key(id))
                {
                    self.observed_focus(id)?;
                }
            }
            Command::FocusMonitor(selector) => {
                self.active_monitor = self.select_monitor(&selector)?;
                let current = self.current_name()?;
                self.focused = self.workspaces[&current].focused;
            }
            Command::MoveWorkspaceToMonitor(selector) => {
                let monitor = self.select_monitor(&selector)?;
                let name = self.current_name()?;
                if self.config.workspace_monitors.contains_key(&name) {
                    return Err(Error("workspace is assigned to a monitor in config".into()));
                }
                let previous = self.visible.get(&monitor).cloned();
                let old = self.active_monitor;
                if let Some(w) = self.workspaces.get_mut(&name) {
                    w.monitor = monitor;
                }
                self.visible.insert(monitor, name);
                if let Some(previous) = previous {
                    if let Some(w) = self.workspaces.get_mut(&previous) {
                        w.monitor = old;
                    }
                    self.visible.insert(old, previous);
                }
                self.active_monitor = monitor;
            }
            Command::Balance => {
                let name = self.current_name()?;
                if let Some(w) = self.workspaces.get_mut(&name) {
                    w.tree.balance();
                }
            }
            Command::Flatten => {
                let name = self.current_name()?;
                if let Some(w) = self.workspaces.get_mut(&name) {
                    let mut ids = Vec::new();
                    w.tree.ids(&mut ids);
                    w.tree = Node::container(
                        self.config.default_orientation,
                        self.config.default_layout,
                    );
                    for id in ids {
                        w.tree.insert(id, None);
                    }
                }
            }
            Command::Layout(requests, root) => {
                let id = self.focused;
                let name = id
                    .and_then(|id| self.windows.get(&id))
                    .map(|w| w.workspace.clone())
                    .unwrap_or(self.current_name()?);
                if id.is_none()
                    && requests.iter().any(|r| {
                        matches!(
                            r,
                            LayoutRequest::Floating
                                | LayoutRequest::Tiling
                                | LayoutRequest::ToggleFloating
                        )
                    })
                {
                    return Err(Error("no focused window".into()));
                }
                self.change_layout(id.unwrap_or(0), &name, &requests, root || id.is_none());
            }
            command => {
                let id = self
                    .focused
                    .ok_or_else(|| Error("no focused window".into()))?;
                let name = self
                    .windows
                    .get(&id)
                    .ok_or_else(|| Error("focused window disappeared".into()))?
                    .workspace
                    .clone();
                match command {
                    Command::Focus(direction) => {
                        if let Some(other) = self.neighbor(id, direction, None) {
                            self.set_focus(other);
                        } else if let Ok(monitor) = self.select_monitor(match direction {
                            Direction::Left => "left",
                            Direction::Right => "right",
                            Direction::Up => "up",
                            Direction::Down => "down",
                        }) {
                            self.active_monitor = monitor;
                            let n = self.current_name()?;
                            self.focused = self.workspaces[&n].focused;
                        }
                    }
                    Command::Move(direction) => {
                        if self.windows[&id].floating {
                            if let Some(w) = self.windows.get_mut(&id) {
                                match direction.axis() {
                                    Axis::Horizontal => {
                                        w.frame.x += if direction.positive() { 50. } else { -50. }
                                    }
                                    Axis::Vertical => {
                                        w.frame.y += if direction.positive() { 50. } else { -50. }
                                    }
                                }
                            }
                        } else if let Some(w) = self.workspaces.get_mut(&name) {
                            w.tree.move_window(id, direction);
                        }
                    }
                    Command::Swap(direction) => {
                        if !self.windows[&id].available() {
                            return Err(Error("cannot swap an unavailable window".into()));
                        }
                        let floating = self.windows[&id].floating;
                        if let Some(other) = self.neighbor(id, direction, Some(floating)) {
                            if floating {
                                let a = self.windows[&id].frame;
                                let b = self.windows[&other].frame;
                                self.windows.get_mut(&id).expect("source").frame = b;
                                self.windows.get_mut(&other).expect("target").frame = a;
                            } else if let Some(w) = self.workspaces.get_mut(&name) {
                                w.tree.swap(id, other);
                            }
                        }
                    }
                    Command::Join(direction) => {
                        if let Some(w) = self.workspaces.get_mut(&name) {
                            w.tree.join_direction(id, direction);
                        }
                    }
                    Command::Split(axis) => {
                        if let Some(w) = self.workspaces.get_mut(&name) {
                            w.tree.split(id, axis);
                        }
                    }
                    Command::MoveToWorkspace(target, follow) => {
                        let target = self.workspace_target(&target)?;
                        self.move_to_workspace(id, &target, follow)?;
                    }
                    Command::MoveToMonitor(selector, follow) => {
                        let monitor = self.select_monitor(&selector)?;
                        let target = self.visible[&monitor].clone();
                        self.move_to_workspace(id, &target, follow)?;
                    }
                    Command::Layout(requests, root) => {
                        self.change_layout(id, &name, &requests, root)
                    }
                    Command::Resize(axis, delta) => self.resize(id, &name, axis, delta),
                    Command::ResizeTo(axis, size) => {
                        self.resize(id, &name, axis, size - axis.length(self.windows[&id].frame))
                    }
                    Command::ResizeSmartTo(size) => {
                        let axis = match self
                            .workspaces
                            .get_mut(&name)
                            .and_then(|w| w.tree.parent_mut(id))
                        {
                            Some(Node::Container { axis, .. }) => *axis,
                            _ => Axis::Horizontal,
                        };
                        self.resize(id, &name, axis, size - axis.length(self.windows[&id].frame));
                    }
                    Command::ResizeSmart(delta) => {
                        let axis = match self
                            .workspaces
                            .get_mut(&name)
                            .and_then(|w| w.tree.parent_mut(id))
                        {
                            Some(Node::Container { axis, .. }) => *axis,
                            _ => Axis::Horizontal,
                        };
                        self.resize(id, &name, axis, delta);
                    }
                    Command::Fullscreen(value) => {
                        if let Some(w) = self.workspaces.get_mut(&name) {
                            let on = value.unwrap_or(w.fullscreen != Some(id));
                            w.fullscreen = on.then_some(id);
                        }
                        effects.push(Effect::Raise(id));
                    }
                    Command::Close => effects.push(Effect::Close(id)),
                    Command::Minimize => effects.push(Effect::Minimize(id)),
                    Command::NativeFullscreen(value) => effects.push(Effect::NativeFullscreen(
                        id,
                        value.unwrap_or(!self.windows[&id].native_fullscreen),
                    )),
                    Command::CloseOthers => {
                        effects.extend(
                            self.windows
                                .values()
                                .filter(|w| w.workspace == name && w.id != id)
                                .map(|w| Effect::Close(w.id)),
                        );
                    }
                    _ => return Err(Error("command is not applicable here".into())),
                }
            }
        }
        effects.extend(self.reflow());
        if explicit_focus || self.focused != previous_focus {
            if let Some(id) = self.focused {
                effects.push(Effect::Focus(id));
            }
        }
        Ok(effects)
    }

    fn resize(&mut self, id: WindowId, name: &str, axis: Axis, delta: f64) {
        if self.windows.get(&id).is_some_and(|w| w.floating) {
            if let Some(w) = self.windows.get_mut(&id) {
                match axis {
                    Axis::Horizontal => w.frame.width = (w.frame.width + delta).max(40.),
                    Axis::Vertical => w.frame.height = (w.frame.height + delta).max(40.),
                }
            }
        } else if let Some(workspace) = self.workspaces.get_mut(name) {
            if let Some(monitor) = self.monitors.get(&workspace.monitor) {
                let gaps = self.config.gaps_for(monitor);
                let rect = monitor
                    .usable
                    .inset(gaps.left, gaps.top, gaps.right, gaps.bottom);
                workspace.tree.resize(
                    id,
                    axis,
                    delta,
                    rect,
                    gaps.inner,
                    self.config.accordion_padding,
                );
            }
        }
    }

    fn change_layout(&mut self, id: WindowId, name: &str, requests: &[LayoutRequest], root: bool) {
        let floating = self.windows.get(&id).is_some_and(|w| w.floating);
        let current = self.workspaces.get_mut(name).and_then(|w| {
            let node = if root {
                Some(&mut w.tree)
            } else {
                w.tree.parent_mut(id)
            };
            match node {
                Some(Node::Container { axis, layout, .. }) => Some((*axis, *layout)),
                _ => None,
            }
        });
        let matches = |request: LayoutRequest| match request {
            LayoutRequest::Floating => floating,
            LayoutRequest::Tiling => !floating,
            LayoutRequest::ToggleFloating => false,
            LayoutRequest::Tiles => current.is_some_and(|(_, l)| l == crate::Layout::Tiles),
            LayoutRequest::Accordion => current.is_some_and(|(_, l)| l == crate::Layout::Accordion),
            LayoutRequest::Horizontal => current.is_some_and(|(a, _)| a == Axis::Horizontal),
            LayoutRequest::Vertical => current.is_some_and(|(a, _)| a == Axis::Vertical),
            LayoutRequest::HorizontalTiles => {
                current == Some((Axis::Horizontal, crate::Layout::Tiles))
            }
            LayoutRequest::VerticalTiles => current == Some((Axis::Vertical, crate::Layout::Tiles)),
            LayoutRequest::HorizontalAccordion => {
                current == Some((Axis::Horizontal, crate::Layout::Accordion))
            }
            LayoutRequest::VerticalAccordion => {
                current == Some((Axis::Vertical, crate::Layout::Accordion))
            }
        };
        let Some(request) = requests
            .iter()
            .copied()
            .find(|r| !matches(*r))
            .or_else(|| requests.first().copied())
        else {
            return;
        };
        let float = match request {
            LayoutRequest::Floating => Some(true),
            LayoutRequest::Tiling => Some(false),
            LayoutRequest::ToggleFloating => Some(!floating),
            _ => None,
        };
        if let Some(float) = float {
            if let Some(window) = self.windows.get_mut(&id) {
                window.floating = float;
            }
            if let Some(w) = self.workspaces.get_mut(name) {
                if float {
                    w.tree.remove(id);
                } else if !w.tree.contains(id) {
                    w.tree.insert(id, w.focused);
                }
            }
            return;
        }
        let axis = match request {
            LayoutRequest::Horizontal
            | LayoutRequest::HorizontalTiles
            | LayoutRequest::HorizontalAccordion => Some(Axis::Horizontal),
            LayoutRequest::Vertical
            | LayoutRequest::VerticalTiles
            | LayoutRequest::VerticalAccordion => Some(Axis::Vertical),
            _ => None,
        };
        let layout = match request {
            LayoutRequest::Tiles
            | LayoutRequest::HorizontalTiles
            | LayoutRequest::VerticalTiles => Some(crate::Layout::Tiles),
            LayoutRequest::Accordion
            | LayoutRequest::HorizontalAccordion
            | LayoutRequest::VerticalAccordion => Some(crate::Layout::Accordion),
            _ => None,
        };
        if let Some(w) = self.workspaces.get_mut(name) {
            w.tree.set_layout(Some(id), axis, layout, root);
        }
    }

    /// Desired visible frames only. The platform backend parks non-visible
    /// windows and journals recovery frames before doing so.
    pub fn reflow(&mut self) -> Vec<Effect> {
        let mut frames = Vec::new();
        if !self.enabled || !self.config.automatic_tiling {
            return frames;
        }
        for name in self.visible.values() {
            let Some(workspace) = self.workspaces.get(name) else {
                continue;
            };
            let Some(monitor) = self.monitors.get(&workspace.monitor) else {
                continue;
            };
            let gaps = self.config.gaps_for(monitor);
            let area = monitor
                .usable
                .inset(gaps.left, gaps.top, gaps.right, gaps.bottom);
            let mut laid_out = Vec::new();
            workspace.tree.frames(
                area,
                gaps.inner,
                self.config.accordion_padding,
                workspace.focused,
                &mut laid_out,
            );
            for (id, frame) in laid_out {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.frame = frame;
                    frames.push(Effect::Frame(id, frame));
                }
            }
            for w in self
                .windows
                .values_mut()
                .filter(|w| w.workspace == *name && w.floating && w.available())
            {
                w.frame = w.frame.constrained(monitor.usable);
                frames.push(Effect::Frame(w.id, w.frame));
            }
            if let Some(id) = workspace.fullscreen {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.frame = area;
                    frames.push(Effect::Frame(id, area));
                }
            }
        }
        // Fullscreen overrides the regular tile frame for the same window.
        let mut seen = BTreeSet::new();
        frames.reverse();
        frames.retain(|e| match e {
            Effect::Frame(id, _) => seen.insert(*id),
            _ => true,
        });
        frames.reverse();
        frames
    }

    pub fn hidden(&self) -> Vec<WindowId> {
        if !self.enabled || !self.config.automatic_tiling {
            return Vec::new();
        }
        self.windows
            .values()
            .filter(|w| w.available() && !self.visible.values().any(|n| n == &w.workspace))
            .map(|w| w.id)
            .collect()
    }

    /// Keep the window on its owning monitor, choosing the corner with the
    /// least overlap with other displays. macOS retains a one-pixel edge.
    pub fn parking_frame(&self, id: WindowId) -> Option<Rect> {
        let window = self.windows.get(&id)?;
        let workspace = self.workspaces.get(&window.workspace)?;
        let monitor = self.monitors.get(&workspace.monitor)?;
        let right = Rect::new(
            monitor.frame.right() - 1.,
            monitor.frame.bottom() - 1.,
            window.frame.width,
            window.frame.height,
        );
        let left = Rect::new(
            monitor.frame.x - window.frame.width + 1.,
            monitor.frame.bottom() - 1.,
            window.frame.width,
            window.frame.height,
        );
        let overlap = |r: Rect| {
            self.monitors
                .values()
                .map(|m| {
                    let width = (r.right().min(m.frame.right()) - r.x.max(m.frame.x)).max(0.);
                    let height = (r.bottom().min(m.frame.bottom()) - r.y.max(m.frame.y)).max(0.);
                    width * height
                })
                .sum::<f64>()
        };
        Some(if overlap(left) < overlap(right) {
            left
        } else {
            right
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            revision: self.revision,
            enabled: self.enabled,
            automatic_tiling: self.config.automatic_tiling,
            mode: self.mode.clone(),
            focused: self.focused,
            active_monitor: self.active_monitor,
            windows: self.windows.values().cloned().collect(),
            workspaces: self.workspaces.values().cloned().collect(),
            monitors: self.monitors.values().cloned().collect(),
            visible: self.visible.clone(),
            message: None,
        }
    }

    /// Invariants used after randomized command/event sequences.
    pub fn verify(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for workspace in self.workspaces.values() {
            if !self.monitors.contains_key(&workspace.monitor) {
                return Err(Error("workspace has no monitor".into()));
            }
            let mut ids = Vec::new();
            workspace.tree.ids(&mut ids);
            for id in ids {
                let w = self
                    .windows
                    .get(&id)
                    .ok_or_else(|| Error("tree references missing window".into()))?;
                if w.workspace != workspace.name || !w.is_tiled() || !seen.insert(id) {
                    return Err(Error("inconsistent tree membership".into()));
                }
            }
        }
        for w in self.windows.values() {
            if !self.workspaces.contains_key(&w.workspace)
                || (w.is_tiled() && !seen.contains(&w.id))
            {
                return Err(Error("window has no workspace/tree node".into()));
            }
            if !w.frame.valid() {
                return Err(Error("invalid frame".into()));
            }
        }
        let mut visible = BTreeSet::new();
        for (monitor, name) in &self.visible {
            if !visible.insert(name)
                || self
                    .workspaces
                    .get(name)
                    .is_none_or(|w| w.monitor != *monitor)
            {
                return Err(Error("invalid visible workspace assignment".into()));
            }
        }
        Ok(())
    }
}

fn directional_distance(from: Rect, to: Rect, direction: Direction) -> Option<f64> {
    let (fx, fy) = from.center();
    let (tx, ty) = to.center();
    let (along, perpendicular) = match direction {
        Direction::Left => (fx - tx, (fy - ty).abs()),
        Direction::Right => (tx - fx, (fy - ty).abs()),
        Direction::Up => (fy - ty, (fx - tx).abs()),
        Direction::Down => (ty - fy, (fx - tx).abs()),
    };
    (along > 1.).then_some(along + perpendicular * 3.)
}
