use super::*;
use crate::SnapPosition;

impl Engine {
    pub(super) fn execute_sequence(&mut self, commands: Vec<Command>) -> Result<Vec<Effect>> {
        if commands.len() > 32 {
            return Err(Error("too many commands".into()));
        }
        let mut pending = commands.into_iter();
        let mut effects = Vec::new();
        while let Some(command) = pending.next() {
            let mut next = self.execute(command)?;
            if let Some(Effect::LaunchTarget { command, .. }) = next
                .iter_mut()
                .find(|e| matches!(e, Effect::LaunchTarget { .. }))
            {
                let mut continuation = vec![(**command).clone()];
                continuation.extend(pending);
                **command = Command::Sequence(continuation);
                effects.extend(next);
                return Ok(effects);
            }
            effects.extend(next);
        }
        Ok(effects)
    }

    /// Resolve against current metadata, preferring the focused match, then
    /// most recently focused, then the lowest native ID for a stable tie break.
    pub fn resolve_target(&self, name: &str) -> Result<Option<WindowId>> {
        let target = self
            .targets
            .get(name)
            .ok_or_else(|| Error(format!("unknown window target {name:?}")))?;
        Ok(self
            .windows
            .values()
            .filter(|w| {
                target.app_id.as_ref().is_none_or(|id| id == &w.app_id)
                    && target
                        .app_name
                        .as_ref()
                        .is_none_or(|r| r.is_match(&w.app_name))
                    && target.title.as_ref().is_none_or(|r| r.is_match(&w.title))
            })
            .max_by_key(|w| {
                (
                    self.focused == Some(w.id),
                    self.focus_order.get(&w.id).copied().unwrap_or(0),
                    std::cmp::Reverse(w.id),
                )
            })
            .map(|w| w.id))
    }

    pub(super) fn execute_named(
        &mut self,
        name: String,
        inner: Option<Command>,
    ) -> Result<Vec<Effect>> {
        if !self.enabled {
            return Err(Error("window controls are paused; use enable on".into()));
        }
        if self.config.automatic_tiling && inner.as_ref().is_some_and(Command::requires_manual_mode)
        {
            return Err(Error(
                "snap requires manual mode; turn Automatic Tiling off".into(),
            ));
        }
        if !self.config.automatic_tiling
            && inner
                .as_ref()
                .is_some_and(Command::requires_automatic_tiling)
        {
            return Err(Error("this command requires Automatic Tiling".into()));
        }
        if inner.as_ref().is_some_and(|c| {
            !matches!(
                c,
                Command::Snap(_)
                    | Command::Move(_)
                    | Command::Resize(_, _)
                    | Command::ResizeTo(_, _)
                    | Command::ResizeSmart(_)
                    | Command::ResizeSmartTo(_)
                    | Command::Fullscreen(_)
                    | Command::NativeFullscreen(_)
                    | Command::Close
                    | Command::Minimize
                    | Command::MoveToMonitor(_, _)
                    | Command::MoveToWorkspace(_, _)
            )
        }) {
            return Err(Error("this command does not accept --target".into()));
        }
        if let Some(id) = self.resolve_target(&name)? {
            return self.execute(match inner {
                None => Command::FocusId(id),
                Some(command) => Command::Target(id, Box::new(command)),
            });
        }
        let target = &self.config.targets[&name];
        let can_launch = !matches!(inner, Some(Command::Close | Command::Minimize));
        if target.launch && can_launch {
            if let Some(app_id) = &target.app_id {
                let command = match inner {
                    None => Command::FocusTarget(name.clone()),
                    Some(command) => Command::NamedTarget(name.clone(), Box::new(command)),
                };
                return Ok(vec![Effect::LaunchTarget {
                    name,
                    app_id: app_id.clone(),
                    command: Box::new(command),
                }]);
            }
        }
        Err(Error(format!("no window matches target {name:?}")))
    }

    pub(super) fn window_monitor(&self, id: WindowId) -> Result<u32> {
        let window = self
            .windows
            .get(&id)
            .ok_or_else(|| Error("window no longer exists".into()))?;
        let (x, y) = window.frame.center();
        Ok(self
            .monitors
            .values()
            .find(|m| m.frame.contains(x, y))
            .map(|m| m.id)
            .unwrap_or(self.active_monitor))
    }

    /// Manual actions emit only the explicitly requested effects. There is no
    /// reflow, parking, workspace routing, or frame write after a notification.
    pub(super) fn execute_manual(&mut self, command: Command) -> Result<Vec<Effect>> {
        match command {
            Command::Enable(on) => {
                self.enabled = on;
                return Ok(Vec::new());
            }
            Command::Mode(mode) => {
                if !self.config.bindings.contains_key(&mode) {
                    return Err(Error(format!("unknown binding mode {mode:?}")));
                }
                self.mode = mode.clone();
                return Ok(vec![Effect::Mode(mode)]);
            }
            Command::Reload => return Ok(Vec::new()),
            ref command if command.is_query() => return Ok(Vec::new()),
            _ if !self.enabled => {
                return Err(Error("window controls are paused; use enable on".into()));
            }
            ref command if command.requires_automatic_tiling() => return Err(Error(
                "this command requires Automatic Tiling; manual mode leaves other windows alone"
                    .into(),
            )),
            Command::Sequence(commands) => {
                return self.execute_sequence(commands);
            }
            Command::Target(id, command) => {
                if !self.windows.contains_key(&id) {
                    return Err(Error("window no longer exists".into()));
                }
                if !matches!(
                    *command,
                    Command::Snap(_)
                        | Command::Move(_)
                        | Command::Resize(_, _)
                        | Command::ResizeTo(_, _)
                        | Command::ResizeSmart(_)
                        | Command::ResizeSmartTo(_)
                        | Command::Fullscreen(_)
                        | Command::Close
                        | Command::Minimize
                        | Command::NativeFullscreen(_)
                        | Command::MoveToMonitor(_, _)
                ) {
                    return Err(Error(
                        "this command does not accept --window-id in manual mode".into(),
                    ));
                }
                let previous = self.focused;
                let previous_window = self.previous_window;
                let previous_monitor = self.active_monitor;
                let follow = matches!(*command, Command::MoveToMonitor(_, true));
                self.focused = Some(id);
                self.active_monitor = self.window_monitor(id)?;
                let result = self.execute(*command);
                if !follow {
                    self.focused = previous;
                    self.previous_window = previous_window;
                    self.active_monitor = previous_monitor;
                }
                return result;
            }
            Command::Exec(shell) => return Ok(vec![Effect::Exec(shell)]),
            Command::MoveMouse(target) => {
                return Ok(vec![Effect::Cursor(
                    self.cursor_destination(target)?,
                    target.lazy(),
                )]);
            }
            Command::FocusId(id) => {
                self.observed_focus(id)?;
                if !self.windows.contains_key(&id) {
                    return Err(Error("window no longer exists".into()));
                }
                return Ok(vec![Effect::Focus(id)]);
            }
            Command::FocusBack => {
                let id = self
                    .previous_window
                    .filter(|id| self.windows.contains_key(id))
                    .ok_or_else(|| Error("no previous window".into()))?;
                return self.execute(Command::FocusId(id));
            }
            Command::FocusMonitor(selector) => {
                let monitor = self.select_monitor(&selector)?;
                let id = self
                    .windows
                    .values()
                    .filter(|w| w.available() && self.window_monitor(w.id).ok() == Some(monitor))
                    .max_by_key(|w| self.focus_order.get(&w.id).copied().unwrap_or(0))
                    .map(|w| w.id)
                    .ok_or_else(|| Error("no available window on that monitor".into()))?;
                return self.execute(Command::FocusId(id));
            }
            Command::Focus(direction) => {
                let id = self
                    .focused
                    .ok_or_else(|| Error("no focused window".into()))?;
                let origin = self.windows[&id].frame;
                let other = self
                    .windows
                    .values()
                    .filter(|w| w.id != id && w.available())
                    .filter_map(|w| {
                        directional_distance(origin, w.frame, direction)
                            .map(|distance| (distance, w.id))
                    })
                    .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
                    .map(|(_, id)| id);
                return other.map_or(Ok(Vec::new()), |id| self.execute(Command::FocusId(id)));
            }
            _ => {}
        }
        let id = self
            .focused
            .ok_or_else(|| Error("no focused window".into()))?;
        let window = &self.windows[&id];
        if matches!(command, Command::Close) {
            return Ok(vec![Effect::Close(id)]);
        }
        if matches!(command, Command::Minimize) {
            return Ok(vec![Effect::Minimize(id)]);
        }
        if let Command::NativeFullscreen(value) = command {
            return Ok(vec![Effect::NativeFullscreen(
                id,
                value.unwrap_or(!window.native_fullscreen),
            )]);
        }
        if !window.available() {
            return Err(Error(
                "focus or restore this window before changing its frame".into(),
            ));
        }
        let before = window.frame;
        let monitor = self.window_monitor(id)?;
        let area = self.monitors[&monitor].usable;
        let mut frame = before;
        let mut focus = false;
        let moving_monitor = matches!(command, Command::MoveToMonitor(_, _));
        if !matches!(command, Command::Fullscreen(_)) {
            self.manual_fullscreen.remove(&id);
        }
        match command {
            Command::Snap(position) => {
                if position == SnapPosition::Restore {
                    frame = self
                        .manual_restore
                        .remove(&id)
                        .ok_or_else(|| Error("no saved position for this window".into()))?;
                } else {
                    self.manual_restore.entry(id).or_insert(before);
                    frame = match position {
                        SnapPosition::Left => {
                            Rect::new(area.x, area.y, area.width / 2., area.height)
                        }
                        SnapPosition::Right => Rect::new(
                            area.x + area.width / 2.,
                            area.y,
                            area.width / 2.,
                            area.height,
                        ),
                        SnapPosition::Top => {
                            Rect::new(area.x, area.y, area.width, area.height / 2.)
                        }
                        SnapPosition::Bottom => Rect::new(
                            area.x,
                            area.y + area.height / 2.,
                            area.width,
                            area.height / 2.,
                        ),
                        SnapPosition::TopLeft
                        | SnapPosition::TopRight
                        | SnapPosition::BottomLeft
                        | SnapPosition::BottomRight => Rect::new(
                            area.x
                                + if matches!(
                                    position,
                                    SnapPosition::TopRight | SnapPosition::BottomRight
                                ) {
                                    area.width / 2.
                                } else {
                                    0.
                                },
                            area.y
                                + if matches!(
                                    position,
                                    SnapPosition::BottomLeft | SnapPosition::BottomRight
                                ) {
                                    area.height / 2.
                                } else {
                                    0.
                                },
                            area.width / 2.,
                            area.height / 2.,
                        ),
                        SnapPosition::Center => Rect::new(
                            area.x + (area.width - before.width) / 2.,
                            area.y + (area.height - before.height) / 2.,
                            before.width,
                            before.height,
                        ),
                        SnapPosition::Maximize => area,
                        SnapPosition::Restore => unreachable!(),
                    };
                }
            }
            Command::Fullscreen(value) => {
                if value.unwrap_or(!self.manual_fullscreen.contains_key(&id)) {
                    self.manual_fullscreen.entry(id).or_insert(before);
                    frame = area;
                } else if let Some(previous) = self.manual_fullscreen.remove(&id) {
                    frame = previous;
                } else {
                    return Ok(Vec::new());
                }
            }
            Command::Move(direction) => match direction {
                Direction::Left => frame.x -= 50.,
                Direction::Right => frame.x += 50.,
                Direction::Up => frame.y -= 50.,
                Direction::Down => frame.y += 50.,
            },
            Command::Resize(axis, delta) => match axis {
                Axis::Horizontal => frame.width = (frame.width + delta).max(40.),
                Axis::Vertical => frame.height = (frame.height + delta).max(40.),
            },
            Command::ResizeTo(axis, size) => match axis {
                Axis::Horizontal => frame.width = size.max(40.),
                Axis::Vertical => frame.height = size.max(40.),
            },
            Command::ResizeSmart(delta) => frame.width = (frame.width + delta).max(40.),
            Command::ResizeSmartTo(size) => frame.width = size.max(40.),
            Command::MoveToMonitor(selector, follow) => {
                let destination = self.select_monitor(&selector)?;
                let to = self.monitors[&destination].usable;
                frame.x += to.x - area.x;
                frame.y += to.y - area.y;
                frame = frame.constrained(to);
                focus = follow;
            }
            _ => return Err(Error(
                "this command requires Automatic Tiling; manual mode leaves other windows alone"
                    .into(),
            )),
        }
        if !moving_monitor {
            frame = frame.constrained(area);
        }
        self.windows.get_mut(&id).expect("window").frame = frame;
        let mut effects = vec![Effect::Frame(id, frame)];
        if focus {
            self.observed_focus(id)?;
            effects.push(Effect::Focus(id));
        }
        Ok(effects)
    }
}
