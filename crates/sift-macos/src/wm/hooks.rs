//! Optional user-authored shell hooks. No threads exist when hooks are unused;
//! two blocking command workers and a bounded queue isolate slow scripts.
use sift_wm::{Hooks, Snapshot, Window};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

pub(super) struct Job {
    shell: String,
    event: &'static str,
    window: Option<Window>,
    workspace: String,
    previous_workspace: String,
    monitor: u32,
    previous_monitor: u32,
    support: PathBuf,
}

pub(super) struct Runner {
    tx: mpsc::SyncSender<Vec<Job>>,
    active: Arc<AtomicBool>,
}
impl Drop for Runner {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}
impl Runner {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel::<Vec<Job>>(32);
        let rx = Arc::new(Mutex::new(rx));
        let active = Arc::new(AtomicBool::new(true));
        for index in 0..2 {
            let rx = rx.clone();
            let active = active.clone();
            std::thread::Builder::new().name(format!("sift-wm-hook-{index}")).spawn(move || {
                loop {
                    let jobs = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok(jobs) = jobs else { break; };
                    // Commands in one callback remain ordered. Separate events
                    // can progress on the other worker if one script is slow.
                    for job in jobs {
                        if !active.load(Ordering::Acquire) { return; }
                        let mut command = std::process::Command::new("/bin/sh");
                        command.arg("-c").arg(&job.shell)
                            .env_remove("SIFT_WINDOW_ID").env_remove("SIFT_APP_ID").env_remove("SIFT_WINDOW_TITLE")
                            .env("SIFT_EVENT", job.event).env("SIFT_WORKSPACE", &job.workspace)
                            .env("SIFT_PREV_WORKSPACE", &job.previous_workspace)
                            .env("SIFT_MONITOR_ID", job.monitor.to_string())
                            .env("SIFT_PREV_MONITOR_ID", job.previous_monitor.to_string())
                            .env("SIFT_CONFIG_DIR", &job.support)
                            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                        if let Some(window) = &job.window {
                            command.env("SIFT_WINDOW_ID", window.id.to_string())
                                .env("SIFT_APP_ID", &window.app_id).env("SIFT_WINDOW_TITLE", &window.title);
                        }
                        match command.status() {
                            Ok(status) if status.success() => {},
                            Ok(status) => tracing::warn!(event = job.event, ?status, "window-manager hook failed"),
                            Err(error) => tracing::warn!(event = job.event, %error, "cannot run window-manager hook"),
                        }
                    }
                }
            }).map_err(|e| e.to_string())?;
        }
        Ok(Self { tx, active })
    }
    pub fn submit(&self, jobs: Vec<Job>) -> Result<(), String> {
        self.tx
            .try_send(jobs)
            .map_err(|_| "window-manager hook queue is full or stopped".into())
    }
}

pub(super) fn events(
    hooks: &Hooks,
    previous: Option<&Snapshot>,
    next: &Snapshot,
    support: &Path,
) -> Vec<Vec<Job>> {
    if hooks.is_empty() || !next.enabled {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut add =
        |commands: &[String], event: &'static str, window: Option<&Window>, monitor: u32| {
            if commands.is_empty() {
                return;
            }
            let workspace = window
                .map(|w| w.workspace.clone())
                .or_else(|| next.visible.get(&monitor).cloned())
                .unwrap_or_default();
            result.push(
                commands
                    .iter()
                    .map(|shell| Job {
                        shell: shell.clone(),
                        event,
                        window: window.cloned(),
                        workspace: workspace.clone(),
                        previous_workspace: previous
                            .and_then(|p| p.visible.get(&monitor))
                            .cloned()
                            .unwrap_or_default(),
                        monitor,
                        previous_monitor: previous.map(|p| p.active_monitor).unwrap_or(monitor),
                        support: support.to_path_buf(),
                    })
                    .collect(),
            );
        };
    let focused = next
        .focused
        .and_then(|id| next.windows.iter().find(|w| w.id == id));
    if previous.is_none() {
        add(
            &hooks.after_startup,
            "after_startup",
            focused,
            next.active_monitor,
        );
    }
    if !hooks.on_window_detected.is_empty() {
        let previous_ids = previous
            .into_iter()
            .flat_map(|snapshot| snapshot.windows.iter())
            .map(|window| (window.pid, window.id))
            .collect::<std::collections::BTreeSet<_>>();
        for window in &next.windows {
            if previous_ids.contains(&(window.pid, window.id)) {
                continue;
            }
            let monitor = next
                .workspaces
                .iter()
                .find(|w| w.name == window.workspace)
                .map(|w| w.monitor)
                .unwrap_or(next.active_monitor);
            add(
                &hooks.on_window_detected,
                "window_detected",
                Some(window),
                monitor,
            );
        }
    }
    if previous.is_some_and(|p| p.focused != next.focused) {
        add(
            &hooks.on_focus_changed,
            "focus_changed",
            focused,
            next.active_monitor,
        );
    }
    if previous.is_some_and(|p| p.active_monitor != next.active_monitor) {
        add(
            &hooks.on_focused_monitor_changed,
            "focused_monitor_changed",
            focused,
            next.active_monitor,
        );
    }
    for (&monitor, name) in &next.visible {
        if previous.is_some_and(|p| p.visible.get(&monitor) != Some(name)) {
            let window = focused.filter(|w| &w.workspace == name);
            add(
                &hooks.on_workspace_changed,
                "workspace_changed",
                window,
                monitor,
            );
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use sift_wm::{Config, Engine, Monitor, Rect, parse};
    #[test]
    fn frame_and_title_updates_do_not_repeat_window_or_focus_callbacks() {
        let hooks = Hooks {
            on_window_detected: vec!["true".into()],
            on_focus_changed: vec!["true".into()],
            ..Hooks::default()
        };
        let mut before = Snapshot {
            enabled: true,
            ..Snapshot::default()
        };
        let mut next = before.clone();
        next.windows
            .push(Window::new(42, 7, Rect::new(0., 0., 400., 300.)));
        next.focused = Some(42);
        let jobs = events(&hooks, Some(&before), &next, Path::new("/tmp"));
        assert_eq!(
            jobs.iter()
                .flatten()
                .map(|job| job.event)
                .collect::<Vec<_>>(),
            ["window_detected", "focus_changed"]
        );
        before = next.clone();
        next.windows[0].title = "new title".into();
        next.windows[0].frame.x = 100.;
        assert!(events(&hooks, Some(&before), &next, Path::new("/tmp")).is_empty());
    }
    #[test]
    fn callbacks_fire_once_per_transition_and_preserve_shell_environment_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("events");
        let shell = format!(
            "printf '%s|%s|%s\\n' \"$SIFT_EVENT\" \"$SIFT_WORKSPACE\" \"$SIFT_PREV_WORKSPACE\" >> '{}'",
            marker.display()
        );
        let hooks = Hooks {
            after_startup: vec![shell.clone()],
            on_workspace_changed: vec![shell],
            ..Hooks::default()
        };
        let mut engine = Engine::new(
            Config::default(),
            vec![Monitor {
                id: 1,
                name: "Test".into(),
                main: true,
                frame: Rect::new(0., 0., 1200., 900.),
                usable: Rect::new(0., 24., 1200., 850.),
            }],
        )
        .expect("engine");
        let before = engine.snapshot();
        let startup = events(&hooks, None, &before, dir.path());
        assert_eq!(startup.len(), 1);
        let name = "code; $(touch forbidden)";
        engine
            .execute(parse(&format!("workspace '{name}'")).expect("command"))
            .expect("execute");
        let after = engine.snapshot();
        let changed = events(&hooks, Some(&before), &after, dir.path());
        assert_eq!(changed.len(), 1);
        assert!(events(&hooks, Some(&after), &after, dir.path()).is_empty());
        let runner = Runner::start().expect("runner");
        // Submit as one ordered batch for a deterministic assertion. Event
        // batches can otherwise run concurrently on the two isolated workers.
        runner
            .submit(startup.into_iter().chain(changed).flatten().collect())
            .expect("submit");
        let started = std::time::Instant::now();
        loop {
            let text = std::fs::read_to_string(&marker).unwrap_or_default();
            if text.lines().count() == 2 {
                assert_eq!(
                    text,
                    format!("after_startup|1|\nworkspace_changed|{name}|1\n")
                );
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(3),
                "hook did not finish: {text}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
