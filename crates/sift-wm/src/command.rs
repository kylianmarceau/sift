use crate::{Axis, Error, Layout, Result, WindowId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}

impl Direction {
    pub fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Up | Self::Down => Axis::Vertical,
        }
    }
    pub fn positive(self) -> bool {
        matches!(self, Self::Right | Self::Down)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutRequest {
    Tiles,
    Accordion,
    Horizontal,
    Vertical,
    Floating,
    Tiling,
    ToggleFloating,
    HorizontalTiles,
    VerticalTiles,
    HorizontalAccordion,
    VerticalAccordion,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    MoveMouse(CursorTarget),
    Focus(Direction),
    FocusId(WindowId),
    FocusTarget(String),
    NamedTarget(String, Box<Command>),
    Snap(SnapPosition),
    FocusBack,
    Move(Direction),
    Swap(Direction),
    Join(Direction),
    Split(Axis),
    Workspace(String),
    WorkspaceBack,
    MoveToWorkspace(String, bool),
    FocusMonitor(String),
    MoveToMonitor(String, bool),
    MoveWorkspaceToMonitor(String),
    Layout(Vec<LayoutRequest>, bool),
    Resize(Axis, f64),
    ResizeSmart(f64),
    ResizeTo(Axis, f64),
    ResizeSmartTo(f64),
    Fullscreen(Option<bool>),
    Balance,
    Flatten,
    Close,
    Minimize,
    NativeFullscreen(Option<bool>),
    SummonWorkspace(String),
    CloseOthers,
    Target(WindowId, Box<Command>),
    Mode(String),
    Enable(bool),
    Reload,
    ListWindows,
    ListWorkspaces,
    ListMonitors,
    ListApps,
    ListModes,
    Status,
    Exec(String),
    Sequence(Vec<Command>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapPosition {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
    Maximize,
    Restore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CursorTarget {
    WindowCenter,
    WindowLazyCenter,
    MonitorCenter,
    MonitorLazyCenter,
}

impl CursorTarget {
    pub fn lazy(self) -> bool {
        matches!(self, Self::WindowLazyCenter | Self::MonitorLazyCenter)
    }
    pub fn window(self) -> bool {
        matches!(self, Self::WindowCenter | Self::WindowLazyCenter)
    }
}

impl Command {
    pub fn requires_manual_mode(&self) -> bool {
        match self {
            Self::Snap(_) => true,
            Self::Target(_, command) | Self::NamedTarget(_, command) => {
                command.requires_manual_mode()
            }
            Self::Sequence(commands) => commands.iter().any(Self::requires_manual_mode),
            _ => false,
        }
    }

    pub fn requires_automatic_tiling(&self) -> bool {
        match self {
            Self::Layout(_, _)
            | Self::Split(_)
            | Self::Join(_)
            | Self::Swap(_)
            | Self::Balance
            | Self::Flatten
            | Self::CloseOthers => true,
            Self::Target(_, command) | Self::NamedTarget(_, command) => {
                command.requires_automatic_tiling()
            }
            Self::Sequence(commands) => commands.iter().any(Self::requires_automatic_tiling),
            _ => false,
        }
    }

    pub fn is_query(&self) -> bool {
        matches!(
            self,
            Self::ListWindows
                | Self::ListWorkspaces
                | Self::ListMonitors
                | Self::ListApps
                | Self::ListModes
                | Self::Status
        )
    }
}

fn direction(s: &str) -> Result<Direction> {
    match s {
        "left" => Ok(Direction::Left),
        "down" => Ok(Direction::Down),
        "up" => Ok(Direction::Up),
        "right" => Ok(Direction::Right),
        _ => Err(Error(format!("unknown direction {s:?}"))),
    }
}

/// Tokenize quoted arguments without invoking a shell. Only the explicit
/// `exec-and-forget` command executes shell code from the user's configuration.
pub fn words(text: &str) -> Result<Vec<String>> {
    if text.len() > 16_384 {
        return Err(Error("command is too long".into()));
    }
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escape = false;
    let mut started = false;
    for c in text.chars() {
        if escape {
            word.push(c);
            escape = false;
            started = true;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escape = true;
            started = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            started = true;
        } else if c.is_whitespace() {
            if started {
                out.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(c);
            started = true;
        }
    }
    if quote.is_some() || escape {
        return Err(Error("unclosed quote or escape in command".into()));
    }
    if started {
        out.push(word);
    }
    Ok(out)
}

pub fn parse(text: &str) -> Result<Command> {
    // exec's tail is shell syntax, preserved exactly, including quotes/semicolons.
    let text = text.trim();
    if let Some(shell) = text.strip_prefix("exec-and-forget ") {
        if shell.is_empty() || shell.len() > 16_384 {
            return Err(Error("empty or oversized shell command".into()));
        }
        return Ok(Command::Exec(shell.to_owned()));
    }
    parse_args(&words(text)?)
}

pub fn parse_args(args: &[String]) -> Result<Command> {
    let mut args = args.to_vec();
    let named = if let Some(index) = args.iter().position(|s| s == "--target") {
        if index + 1 >= args.len() {
            return Err(Error("--target needs a saved target name".into()));
        }
        let name = args.remove(index + 1);
        args.remove(index);
        Some(name)
    } else {
        None
    };
    let target = if let Some(index) = args.iter().position(|s| s == "--window-id") {
        if index + 1 >= args.len() {
            return Err(Error("--window-id needs an id".into()));
        }
        let id = args
            .remove(index + 1)
            .parse::<u32>()
            .map_err(|_| Error("invalid window id".into()))?;
        args.remove(index);
        Some(id)
    } else {
        None
    };
    if let Some(name) = named {
        if target.is_some() {
            return Err(Error("use either --target or --window-id".into()));
        }
        if args.as_slice() == ["focus"] {
            return Ok(Command::FocusTarget(name));
        }
        return Ok(Command::NamedTarget(
            name,
            Box::new(parse_simple_args(&args)?),
        ));
    }
    if args.as_slice() == ["focus"] {
        if let Some(id) = target {
            return Ok(Command::FocusId(id));
        }
    }
    let command = parse_simple_args(&args)?;
    if let Some(id) = target {
        Ok(Command::Target(id, Box::new(command)))
    } else {
        Ok(command)
    }
}

fn parse_simple_args(args: &[String]) -> Result<Command> {
    let name = args.first().map(String::as_str).unwrap_or("");
    let mut values: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let take_flag = |values: &mut Vec<&str>, flag: &str| {
        if let Some(i) = values.iter().position(|s| *s == flag) {
            values.remove(i);
            true
        } else {
            false
        }
    };
    let follow = take_flag(&mut values, "--focus-follows-window");
    let root = take_flag(&mut values, "--root");
    // Output is always JSON, suitable for both Sift and shell clients.
    take_flag(&mut values, "--json");
    take_flag(&mut values, "--all");
    let arg = || {
        values
            .first()
            .copied()
            .ok_or_else(|| Error(format!("{name} needs an argument")))
    };
    let one = |command| {
        if values.is_empty() {
            Ok(command)
        } else {
            Err(Error(format!("unexpected arguments for {name}")))
        }
    };
    let single = || {
        if values.len() == 1 {
            arg()
        } else {
            Err(Error(format!("{name} expects one argument")))
        }
    };
    let numeric = |s: &str| -> Result<f64> {
        s.parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && n.abs() <= 100_000.)
            .ok_or_else(|| {
                Error("resize requires a finite point delta between -100000 and 100000".into())
            })
    };
    match name {
        "snap" => {
            Ok(Command::Snap(match single()? {
                "left" => SnapPosition::Left,
                "right" => SnapPosition::Right,
                "top" => SnapPosition::Top,
                "bottom" => SnapPosition::Bottom,
                "top-left" => SnapPosition::TopLeft,
                "top-right" => SnapPosition::TopRight,
                "bottom-left" => SnapPosition::BottomLeft,
                "bottom-right" => SnapPosition::BottomRight,
                "center" => SnapPosition::Center,
                "maximize" => SnapPosition::Maximize,
                "restore" => SnapPosition::Restore,
                _ => return Err(Error(
                    "snap expects left, right, top, bottom, a corner, center, maximize, or restore"
                        .into(),
                )),
            }))
        }
        "move-mouse" => Ok(Command::MoveMouse(match single()? {
            "window-center" => CursorTarget::WindowCenter,
            "window-lazy-center" => CursorTarget::WindowLazyCenter,
            "monitor-center" => CursorTarget::MonitorCenter,
            "monitor-lazy-center" => CursorTarget::MonitorLazyCenter,
            _ => {
                return Err(Error(
                    "move-mouse expects window[-lazy]-center or monitor[-lazy]-center".into(),
                ));
            }
        })),
        "focus" => {
            if values.len() == 2 && values[0] == "--window-id" {
                return values[1]
                    .parse()
                    .map(Command::FocusId)
                    .map_err(|_| Error("invalid window id".into()));
            }
            Ok(Command::Focus(direction(single()?)?))
        }
        "focus-back-and-forth" => one(Command::FocusBack),
        "move" => Ok(Command::Move(direction(single()?)?)),
        "swap" => Ok(Command::Swap(direction(single()?)?)),
        "join-with" => Ok(Command::Join(direction(single()?)?)),
        "split" => Ok(Command::Split(match single()? {
            "horizontal" | "h" => Axis::Horizontal,
            "vertical" | "v" => Axis::Vertical,
            _ => return Err(Error("split expects horizontal or vertical".into())),
        })),
        "workspace" => Ok(Command::Workspace(single()?.to_owned())),
        "workspace-back-and-forth" => one(Command::WorkspaceBack),
        "move-node-to-workspace" => Ok(Command::MoveToWorkspace(single()?.to_owned(), follow)),
        "focus-monitor" => Ok(Command::FocusMonitor(single()?.to_owned())),
        "move-node-to-monitor" => Ok(Command::MoveToMonitor(single()?.to_owned(), follow)),
        "move-workspace-to-monitor" => Ok(Command::MoveWorkspaceToMonitor(single()?.to_owned())),
        "layout" => {
            if values.is_empty() {
                return Err(Error("layout needs tiles, accordion, horizontal, vertical, floating, tiling, or toggle".into()));
            }
            Ok(Command::Layout(
                values
                    .iter()
                    .map(|s| match *s {
                        "tiles" => Ok(LayoutRequest::Tiles),
                        "accordion" => Ok(LayoutRequest::Accordion),
                        "horizontal" => Ok(LayoutRequest::Horizontal),
                        "vertical" => Ok(LayoutRequest::Vertical),
                        "floating" => Ok(LayoutRequest::Floating),
                        "tiling" => Ok(LayoutRequest::Tiling),
                        "toggle" => Ok(LayoutRequest::ToggleFloating),
                        "h_tiles" => Ok(LayoutRequest::HorizontalTiles),
                        "v_tiles" => Ok(LayoutRequest::VerticalTiles),
                        "h_accordion" => Ok(LayoutRequest::HorizontalAccordion),
                        "v_accordion" => Ok(LayoutRequest::VerticalAccordion),
                        _ => Err(Error(format!("unknown layout {s:?}"))),
                    })
                    .collect::<Result<_>>()?,
                root,
            ))
        }
        "resize" if values.len() == 2 => {
            let delta = numeric(values[1])?;
            let relative = values[1].starts_with(['+', '-']);
            match values[0] {
                "width" => Ok(if relative {
                    Command::Resize(Axis::Horizontal, delta)
                } else {
                    Command::ResizeTo(Axis::Horizontal, delta)
                }),
                "height" => Ok(if relative {
                    Command::Resize(Axis::Vertical, delta)
                } else {
                    Command::ResizeTo(Axis::Vertical, delta)
                }),
                "smart" => Ok(if relative {
                    Command::ResizeSmart(delta)
                } else {
                    Command::ResizeSmartTo(delta)
                }),
                _ => Err(Error("resize expects width, height or smart".into())),
            }
        }
        "fullscreen" => Ok(Command::Fullscreen(match values.as_slice() {
            [] => None,
            ["on"] => Some(true),
            ["off"] => Some(false),
            _ => return Err(Error("fullscreen expects on, off or no argument".into())),
        })),
        "balance-sizes" => one(Command::Balance),
        "flatten-workspace-tree" => one(Command::Flatten),
        "close" => one(Command::Close),
        "macos-native-minimize" => one(Command::Minimize),
        "macos-native-fullscreen" => Ok(Command::NativeFullscreen(match values.as_slice() {
            [] => None,
            ["on"] => Some(true),
            ["off"] => Some(false),
            _ => {
                return Err(Error(
                    "macos-native-fullscreen expects on, off or no argument".into(),
                ));
            }
        })),
        "summon-workspace" => Ok(Command::SummonWorkspace(single()?.to_owned())),
        "close-all-windows-but-current" => one(Command::CloseOthers),
        "mode" => Ok(Command::Mode(single()?.to_owned())),
        "enable" => Ok(Command::Enable(match single()? {
            "on" => true,
            "off" => false,
            _ => return Err(Error("enable expects on or off".into())),
        })),
        "reload-config" => one(Command::Reload),
        "list-windows" => one(Command::ListWindows),
        "list-workspaces" => one(Command::ListWorkspaces),
        "list-monitors" => one(Command::ListMonitors),
        "list-apps" => one(Command::ListApps),
        "list-modes" => one(Command::ListModes),
        "status" => one(Command::Status),
        "exec-and-forget" => Ok(Command::Exec(values.join(" "))),
        _ => Err(Error(format!(
            "unknown command or invalid arguments: {name}"
        ))),
    }
}

impl From<Layout> for LayoutRequest {
    fn from(l: Layout) -> Self {
        match l {
            Layout::Tiles => Self::Tiles,
            Layout::Accordion => Self::Accordion,
        }
    }
}
