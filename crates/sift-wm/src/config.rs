use crate::{Axis, Command, Error, Layout, Result, parse};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub default_layout: Layout,
    pub default_orientation: Axis,
    pub accordion_padding: f64,
    pub gaps: Gaps,
    /// Display selector -> partial gap overrides; numeric IDs are most specific.
    pub monitor_gaps: BTreeMap<String, GapsOverride>,
    pub hooks: Hooks,
    pub mouse: MouseConfig,
    pub workspaces: Vec<String>,
    /// Workspace name -> display name substring, numeric display id, main or secondary.
    pub workspace_monitors: BTreeMap<String, String>,
    pub rules: Vec<Rule>,
    /// Optional bundle-id regex allowlist and denylist. Empty allowlist manages all regular apps.
    pub include_apps: Vec<String>,
    pub exclude_apps: Vec<String>,
    /// Binding mode -> physical shortcut (e.g. ctrl-alt-h) -> command(s).
    pub bindings: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MouseConfig {
    pub focus_follows_mouse: bool,
    pub drag_to_rearrange: bool,
    pub follows_focus: Option<crate::CursorTarget>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hooks {
    pub after_startup: Vec<String>,
    pub on_window_detected: Vec<String>,
    pub on_focus_changed: Vec<String>,
    pub on_workspace_changed: Vec<String>,
    pub on_focused_monitor_changed: Vec<String>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.groups()
            .into_iter()
            .all(|commands| commands.is_empty())
    }
    fn groups(&self) -> [&[String]; 5] {
        [
            &self.after_startup,
            &self.on_window_detected,
            &self.on_focus_changed,
            &self.on_workspace_changed,
            &self.on_focused_monitor_changed,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gaps {
    pub inner: f64,
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GapsOverride {
    pub inner: Option<f64>,
    pub left: Option<f64>,
    pub top: Option<f64>,
    pub right: Option<f64>,
    pub bottom: Option<f64>,
}

impl GapsOverride {
    fn values(self) -> [Option<f64>; 5] {
        [self.inner, self.left, self.top, self.right, self.bottom]
    }
    fn apply(self, gaps: &mut Gaps) {
        for (value, target) in self.values().into_iter().zip([
            &mut gaps.inner,
            &mut gaps.left,
            &mut gaps.top,
            &mut gaps.right,
            &mut gaps.bottom,
        ]) {
            if let Some(value) = value {
                *target = value;
            }
        }
    }
}

impl Default for Gaps {
    fn default() -> Self {
        Self {
            inner: 8.,
            left: 8.,
            top: 8.,
            right: 8.,
            bottom: 8.,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rule {
    /// Regexes; all supplied conditions must match. Compiled once at configuration load.
    pub app_id: Option<String>,
    pub app_name: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<String>,
    pub floating: Option<bool>,
    pub check_further: bool,
}

impl Default for Config {
    fn default() -> Self {
        let mut main = BTreeMap::new();
        for (key, command) in [
            ("ctrl-alt-h", "focus left"),
            ("ctrl-alt-j", "focus down"),
            ("ctrl-alt-k", "focus up"),
            ("ctrl-alt-l", "focus right"),
            ("ctrl-alt-shift-h", "move left"),
            ("ctrl-alt-shift-j", "move down"),
            ("ctrl-alt-shift-k", "move up"),
            ("ctrl-alt-shift-l", "move right"),
            ("ctrl-alt-slash", "layout tiles horizontal vertical"),
            ("ctrl-alt-comma", "layout accordion horizontal vertical"),
            ("ctrl-alt-f", "fullscreen"),
            ("ctrl-alt-minus", "resize smart -50"),
            ("ctrl-alt-equal", "resize smart +50"),
            ("ctrl-alt-tab", "workspace-back-and-forth"),
        ] {
            main.insert(key.into(), vec![command.into()]);
        }
        for n in 1..=9 {
            main.insert(format!("ctrl-alt-{n}"), vec![format!("workspace {n}")]);
            main.insert(
                format!("ctrl-alt-shift-{n}"),
                vec![format!("move-node-to-workspace {n}")],
            );
        }
        Self {
            enabled: false,
            default_layout: Layout::Tiles,
            default_orientation: Axis::Horizontal,
            accordion_padding: 30.,
            gaps: Gaps::default(),
            monitor_gaps: BTreeMap::new(),
            hooks: Hooks::default(),
            mouse: MouseConfig::default(),
            workspaces: (1..=9).map(|n| n.to_string()).collect(),
            workspace_monitors: BTreeMap::new(),
            rules: Vec::new(),
            include_apps: Vec::new(),
            exclude_apps: Vec::new(),
            bindings: BTreeMap::from([("main".into(), main)]),
        }
    }
}

impl Config {
    pub(crate) fn gaps_for(&self, monitor: &crate::Monitor) -> Gaps {
        let mut gaps = self.gaps;
        if self.monitor_gaps.is_empty() {
            return gaps;
        }
        let mut matches = self
            .monitor_gaps
            .iter()
            .filter_map(|(selector, values)| {
                let priority = if selector.parse::<u32>().ok() == Some(monitor.id) {
                    3
                } else if selector == &monitor.name {
                    2
                } else if (selector == "main" && monitor.main)
                    || (selector == "secondary" && !monitor.main)
                {
                    0
                } else if monitor
                    .name
                    .to_lowercase()
                    .contains(&selector.to_lowercase())
                {
                    1
                } else {
                    return None;
                };
                Some((priority, selector, values))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        for (_, _, values) in matches {
            values.apply(&mut gaps);
        }
        gaps
    }

    pub fn validate(&self) -> Result<()> {
        for commands in self.hooks.groups() {
            if commands.len() > 32
                || commands.iter().any(|command| {
                    command.trim().is_empty() || command.len() > 16_384 || command.contains('\0')
                })
            {
                return Err(Error("each hook accepts at most 32 non-empty shell commands, each up to 16 KiB without NUL".into()));
            }
        }
        if self.workspaces.len() > 256 || self.workspaces.is_empty() {
            return Err(Error("configure between 1 and 256 workspaces".into()));
        }
        let mut seen = std::collections::HashSet::new();
        for name in &self.workspaces {
            validate_name(name)?;
            if !seen.insert(name) {
                return Err(Error(format!("duplicate workspace {name:?}")));
            }
        }
        if self.rules.len() > 256 || self.bindings.len() > 32 {
            return Err(Error("too many window rules or binding modes".into()));
        }
        for value in [
            self.accordion_padding,
            self.gaps.inner,
            self.gaps.left,
            self.gaps.right,
            self.gaps.top,
            self.gaps.bottom,
        ] {
            if !value.is_finite() || !(0. ..=1000.).contains(&value) {
                return Err(Error(
                    "gaps and accordion padding must be finite values from 0 to 1000".into(),
                ));
            }
        }
        if self.monitor_gaps.len() > 64 {
            return Err(Error("too many monitor gap overrides".into()));
        }
        for (selector, gaps) in &self.monitor_gaps {
            validate_name(selector)?;
            if gaps
                .values()
                .into_iter()
                .flatten()
                .any(|v| !v.is_finite() || !(0. ..=1000.).contains(&v))
            {
                return Err(Error(
                    "monitor gaps must be finite values from 0 to 1000".into(),
                ));
            }
        }
        if !self.bindings.contains_key("main") {
            return Err(Error("a main binding mode is required".into()));
        }
        for (mode, bindings) in &self.bindings {
            validate_name(mode)?;
            if bindings.len() > 256 {
                return Err(Error("too many hotkeys in a mode".into()));
            }
            for commands in bindings.values() {
                if commands.is_empty() || commands.len() > 32 {
                    return Err(Error("a binding needs 1 to 32 commands".into()));
                }
                for text in commands {
                    if let Command::Mode(target) = parse(text)? {
                        if !self.bindings.contains_key(&target) {
                            return Err(Error(format!("unknown binding mode {target:?}")));
                        }
                    }
                }
            }
        }
        for rule in &self.rules {
            for pattern in [&rule.app_id, &rule.app_name, &rule.title]
                .into_iter()
                .flatten()
            {
                compile(pattern)?;
            }
            if let Some(w) = &rule.workspace {
                validate_name(w)?;
            }
        }
        if self.include_apps.len() + self.exclude_apps.len() > 256 {
            return Err(Error("too many application filters".into()));
        }
        for pattern in self.include_apps.iter().chain(&self.exclude_apps) {
            compile(pattern)?;
        }
        for (workspace, selector) in &self.workspace_monitors {
            validate_name(workspace)?;
            validate_name(selector)?;
        }
        Ok(())
    }
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        Err(Error(
            "names must contain 1 to 128 bytes and no control characters".into(),
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn compile(pattern: &str) -> Result<regex::Regex> {
    regex::RegexBuilder::new(pattern)
        .size_limit(256 * 1024)
        .build()
        .map_err(|e| Error(format!("invalid window-rule regex: {e}")))
}
