use super::worker::{Task, Worker};
use super::{AppInfo, Event};
use serde::{Deserialize, Serialize};
use sift_wm::protocol::{self, Reply, Request};
use sift_wm::{Command, Config, Effect, Engine, Monitor, Rect, Snapshot, WindowId};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, mpsc};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Controller {
    pub(super) tx: mpsc::SyncSender<Event>,
    snapshot: Arc<RwLock<Arc<Snapshot>>>,
    stopping: Arc<AtomicBool>,
    config: Arc<RwLock<Arc<Config>>>,
    stopped: Arc<(Mutex<bool>, Condvar)>,
}

impl Controller {
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.snapshot, &other.snapshot)
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn execute(&self, command: Command) -> Result<(), String> {
        self.tx
            .try_send(Event::Command(command, None))
            .map_err(|_| "window manager is busy or stopped".into())
    }
    pub fn configure(&self, config: Config) -> Result<(), String> {
        self.tx
            .try_send(Event::Config(Box::new(config)))
            .map_err(|_| "window manager is busy or stopped".into())
    }
    pub(super) fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        // This runs on the main thread; never wait on an AX worker.
        let _ = self.tx.try_send(Event::Shutdown);
    }

    /// Call on a background task during application termination, allowing
    /// AX restoration to finish before the process exits.
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        let (lock, condition) = &*self.stopped;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = condition
            .wait_timeout_while(guard, timeout, |done| !*done)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecoveryFrame {
    id: WindowId,
    pid: i32,
    app_id: String,
    #[serde(default)]
    process_started: Option<(u64, u64)>,
    frame: Rect,
}

struct Service {
    engine: Engine,
    workers: BTreeMap<i32, Worker>,
    tx: mpsc::SyncSender<Event>,
    snapshot: Arc<RwLock<Arc<Snapshot>>>,
    publish: Box<dyn Fn(Arc<Snapshot>) + Send>,
    journal: PathBuf,
    recovery: Vec<RecoveryFrame>,
    parked: BTreeSet<WindowId>,
    frontmost: Option<i32>,
    message: Option<String>,
    last_published: Option<Snapshot>,
    last_journal: Vec<RecoveryFrame>,
    stopping: Arc<AtomicBool>,
    config_path: PathBuf,
    shared_config: Arc<RwLock<Arc<Config>>>,
    available_apps: Vec<AppInfo>,
    hooks: Option<super::hooks::Runner>,
    mouse_held: bool,
    pending_launches: BTreeMap<String, PendingLaunch>,
    launch_serial: u64,
    restoring: Vec<RecoveryFrame>,
    known_apps: BTreeMap<String, PathBuf>,
}

struct PendingLaunch {
    command: Command,
    workspace: Option<(String, String)>,
    awaiting_existing: bool,
    deadline: Instant,
    serial: u64,
}

fn workspace_is_current(snapshot: &Snapshot, workspace: &str) -> bool {
    snapshot
        .visible
        .get(&snapshot.active_monitor)
        .is_some_and(|name| name == workspace)
}

pub(super) fn start(
    config: Config,
    support: &Path,
    config_path: &Path,
    monitors: Vec<Monitor>,
    applications: (Vec<AppInfo>, Option<i32>),
    channels: (mpsc::SyncSender<Event>, mpsc::Receiver<Event>),
    publish: impl Fn(Arc<Snapshot>) + Send + 'static,
) -> Result<Controller, String> {
    let (tx, rx) = channels;
    let shared_config = Arc::new(RwLock::new(Arc::new(config.clone())));
    let engine = Engine::new(config, monitors).map_err(|e| e.to_string())?;
    let directory = protocol::socket_path(support)
        .parent()
        .ok_or("missing socket directory")?
        .to_path_buf();
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let metadata = std::fs::symlink_metadata(&directory).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("window manager directory is not owned by this user".into());
    }
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("instance.lock"))
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another Sift window manager is already running".into());
    }
    let socket = protocol::socket_path(support);
    if socket.exists() {
        std::fs::remove_file(&socket).map_err(|e| e.to_string())?;
    }
    let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    let snapshot = Arc::new(RwLock::new(Arc::new(engine.snapshot())));
    let stopping = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new((Mutex::new(false), Condvar::new()));
    let completion = stopped.clone();
    let journal = directory.join("recovery.json");
    let recovery: Vec<RecoveryFrame> = std::fs::read(&journal)
        .ok()
        .filter(|b| b.len() <= protocol::MAX_FRAME)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let controller = Controller {
        tx: tx.clone(),
        snapshot: snapshot.clone(),
        stopping: stopping.clone(),
        config: shared_config.clone(),
        stopped,
    };
    let mut service = Service {
        engine,
        workers: BTreeMap::new(),
        tx: tx.clone(),
        snapshot,
        publish: Box::new(publish),
        journal,
        recovery: recovery.clone(),
        parked: BTreeSet::new(),
        frontmost: applications.1,
        message: None,
        last_published: None,
        last_journal: recovery.clone(),
        stopping: stopping.clone(),
        config_path: config_path.to_path_buf(),
        shared_config,
        available_apps: Vec::new(),
        hooks: None,
        mouse_held: false,
        pending_launches: BTreeMap::new(),
        launch_serial: 0,
        restoring: Vec::new(),
        known_apps: BTreeMap::new(),
    };
    let socket_copy = socket.clone();
    let ipc_thread = std::thread::Builder::new()
        .name("sift-wm-ipc".into())
        .spawn(move || ipc(listener, socket_copy, tx, stopping))
        .map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("sift-wm".into())
        .spawn(move || {
            let _lock = lock;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                service.applications(applications.0, true);
                service.publish();
                loop {
                    // Only a user-requested app launch creates a one-shot wait.
                    // Once it completes or expires the service blocks indefinitely.
                    let event = if let Some(deadline) =
                        service.pending_launches.values().map(|p| p.deadline).min()
                    {
                        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                            Ok(event) => event,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                service.finish_launches();
                                service.publish();
                                continue;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        match rx.recv() {
                            Ok(event) => event,
                            Err(_) => break,
                        }
                    };
                    if service.stopping.load(Ordering::Acquire) || matches!(event, Event::Shutdown)
                    {
                        break;
                    }
                    let started = Instant::now();
                    match event {
                        Event::Activated(frontmost, info) => {
                            service.frontmost = frontmost;
                            if let Some(info) = info {
                                let mut apps = service.available_apps.clone();
                                if let Some(old) = apps.iter_mut().find(|app| app.pid == info.pid) {
                                    *old = info;
                                } else {
                                    apps.push(info);
                                }
                                service.applications(apps, false);
                            }
                        }
                        Event::Refresh => {
                            service.applications(service.available_apps.clone(), true)
                        }
                        Event::Applications(apps, frontmost, refresh_all) => {
                            service.frontmost = frontmost;
                            service.applications(apps, refresh_all);
                        }
                        Event::Monitors(monitors) => {
                            if let Err(e) = service.engine.set_monitors(monitors) {
                                service.message = Some(e.to_string());
                            }
                        }
                        Event::Config(config) => {
                            if let Err(e) = service.reconfigure(*config) {
                                service.message = Some(e);
                            }
                        }
                        Event::Command(command, reply) => {
                            service.reports();
                            let response = service.command(command);
                            if let Some(reply) = reply {
                                let _ = reply.send(response);
                            }
                        }
                        Event::Reports => {}
                        Event::LaunchCompleted(name, serial, result) => {
                            if service
                                .pending_launches
                                .get(&name)
                                .is_some_and(|p| p.serial == serial)
                            {
                                if let Err(error) = result {
                                    service.pending_launches.remove(&name);
                                    service.message = Some(error);
                                } else {
                                    service.applications(service.available_apps.clone(), true);
                                }
                            }
                        }
                        Event::Hover(id) => {
                            if service.engine.enabled
                                && service.engine.config.mouse.focus_follows_mouse
                                && !service.mouse_held
                                && service.engine.focused != Some(id)
                                && service
                                    .engine
                                    .windows
                                    .get(&id)
                                    .is_some_and(|w| w.available())
                            {
                                let _ = service.command(Command::FocusId(id));
                            }
                        }
                        Event::MouseHold(held) => {
                            service.mouse_held =
                                held && service.engine.config.mouse.drag_to_rearrange
                        }
                        Event::DragEnd(drag) => {
                            service.mouse_held = false;
                            if service.engine.config.mouse.drag_to_rearrange {
                                if let Some(drag) = drag {
                                    match service.engine.commit_drag(
                                        drag.id,
                                        drag.before,
                                        drag.after,
                                        drag.point.x,
                                        drag.point.y,
                                    ) {
                                        Ok(effects) => service.apply(effects),
                                        Err(error) => service.message = Some(error.to_string()),
                                    }
                                }
                            }
                        }
                        Event::Shutdown => break,
                    }
                    service.reports();
                    service.finish_launches();
                    service.apply(Vec::new());
                    service.publish();
                    tracing::debug!(
                        micros = started.elapsed().as_micros(),
                        "window manager event"
                    );
                }
            }));
            if result.is_err() {
                tracing::error!(
                    "window manager panicked; restoring windows and releasing ownership"
                );
            }
            // Restoration commands are queued before stopping each worker. They
            // run independently even if another application is unresponsive.
            service.restore();
            for worker in service.workers.values() {
                worker.send(Task::Stop);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            for worker in service.workers.values() {
                worker.wait_stopped(deadline);
            }
            service.workers.clear();
            service.stopping.store(true, Ordering::Release);
            drop(rx);
            let _ = UnixStream::connect(socket);
            let _ = ipc_thread.join();
            // A concurrent configured hook can fork while this descriptor is
            // open. Explicitly unlock the shared file description before
            // announcing completion; closing our copy alone can leave the
            // lease held until the child reaches exec and closes its copy.
            unsafe {
                libc::flock(_lock.as_raw_fd(), libc::LOCK_UN);
            }
            drop(_lock);
            let (lock, condition) = &*completion;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            condition.notify_all();
        })
        .map_err(|e| e.to_string())?;
    Ok(controller)
}

impl Service {
    fn applications(&mut self, apps: Vec<AppInfo>, refresh_all: bool) {
        let mut targets: BTreeSet<String> = self
            .engine
            .config
            .targets
            .values()
            .filter_map(|target| target.app_id.clone())
            .collect();
        for workspace in &self.engine.config.workspaces {
            targets.extend(self.engine.config.assigned_apps(workspace));
        }
        self.known_apps
            .retain(|app_id, _| targets.contains(app_id.as_str()));
        for app in &apps {
            if targets.contains(app.bundle.as_str()) {
                if let Some(path) = &app.path {
                    self.known_apps.insert(app.bundle.clone(), path.clone());
                }
            }
        }
        let considered = apps.len();
        self.available_apps = apps.clone();
        let apps: Vec<_> = apps
            .into_iter()
            .filter(|a| self.engine.manages_app(&a.bundle))
            .collect();
        tracing::debug!(
            considered,
            managed = apps.len(),
            "window-manager application discovery"
        );
        let alive: BTreeSet<_> = apps.iter().map(|a| a.pid).collect();
        for (pid, worker) in self
            .workers
            .iter()
            .filter(|(pid, _)| !alive.contains(pid) && self.engine.config.automatic_tiling)
        {
            let frames: Vec<_> = self
                .engine
                .windows
                .values()
                .filter(|w| w.pid == *pid)
                .map(|w| (w.id, w.original_frame))
                .collect();
            // Restore is asynchronous. Keep recovery data if an excluded or
            // terminating application cannot answer before its worker stops.
            for window in self.engine.windows.values().filter(|w| w.pid == *pid) {
                if !self
                    .recovery
                    .iter()
                    .any(|r| r.id == window.id && r.pid == *pid)
                {
                    self.recovery.push(RecoveryFrame {
                        id: window.id,
                        pid: *pid,
                        app_id: window.app_id.clone(),
                        process_started: Some(worker.identity),
                        frame: window.original_frame,
                    });
                }
            }
            if self.engine.config.automatic_tiling {
                worker.send(Task::Restore(frames));
            }
        }
        let gone: Vec<_> = self
            .engine
            .windows
            .values()
            .filter(|w| !alive.contains(&w.pid))
            .map(|w| w.id)
            .collect();
        for id in gone {
            self.engine.remove_window(id);
            self.parked.remove(&id);
        }
        self.workers.retain(|pid, _| alive.contains(pid));
        for app in apps {
            if let Some(worker) = self.workers.get(&app.pid) {
                if refresh_all || self.frontmost == Some(app.pid) {
                    worker.send(Task::Refresh {
                        focus: !refresh_all,
                        hidden: app.hidden,
                    });
                }
            } else {
                let pid = app.pid;
                match Worker::start(app, self.tx.clone()) {
                    Ok(worker) => {
                        self.workers.insert(pid, worker);
                    }
                    Err(e) => self.message = Some(e.to_string()),
                }
            }
        }
    }

    fn reports(&mut self) {
        let reports: Vec<_> = self
            .workers
            .values()
            .filter_map(|w| w.latest.lock().unwrap_or_else(|e| e.into_inner()).take())
            .collect();
        for report in reports {
            if let Some(failure) = report.failure {
                self.message = Some(failure);
                continue;
            }
            for id in remove_absent(&mut self.engine, &report) {
                self.parked.remove(&id);
                self.restoring.retain(|r| r.id != id || r.pid != report.pid);
                self.recovery.retain(|r| r.id != id || r.pid != report.pid);
            }
            for mut window in report.windows {
                let observed_frame = window.frame;
                let observed_offscreen = self.engine.offscreen(observed_frame);
                // A delayed AX report can still contain the parking frame
                // after the workspace has become visible. Keep the saved
                // manual frame until the restore has been acknowledged.
                let restoring = self
                    .restoring
                    .iter()
                    .any(|r| r.id == window.id && r.pid == window.pid);
                if !self.engine.config.automatic_tiling
                    && (self.parked.contains(&window.id) || (restoring && observed_offscreen))
                {
                    if let Some(existing) = self.engine.windows.get(&window.id) {
                        window.frame = existing.frame;
                    }
                }
                // Any on-screen frame acknowledges a manual restore: an app
                // may adjust the exact frame (minimum sizes, menu bar), and
                // waiting for an exact match would ignore later user moves.
                self.restoring.retain(|r| {
                    !(r.id == window.id
                        && r.pid == window.pid
                        && (observed_frame.nearly(r.frame)
                            || (!self.engine.config.automatic_tiling && !observed_offscreen)))
                });
                // Recovery matching requires both pid and native window id,
                // plus the bundle id. Never restore unrelated new windows.
                if let Some(index) = self.recovery.iter().position(|r| {
                    r.id == window.id
                        && r.pid == window.pid
                        && r.app_id == window.app_id
                        && r.process_started.is_some()
                        && r.process_started == self.workers.get(&window.pid).map(|w| w.identity)
                        && r.frame.valid()
                        && !window.minimized
                        && !window.native_fullscreen
                }) {
                    let recovery = self.recovery.remove(index);
                    window.original_frame = recovery.frame;
                    // The observed frame is already an acknowledgment. A
                    // no-op AX restore emits no notification, so waiting for
                    // one would leave a stale automatic claim in manual mode.
                    if !window.frame.nearly(recovery.frame) {
                        window.frame = recovery.frame;
                        if let Some(worker) = self.workers.get(&window.pid) {
                            worker.send(Task::Restore(vec![(window.id, recovery.frame)]));
                        }
                        self.restoring.push(recovery);
                    }
                }
                let id = window.id;
                let pid = window.pid;
                let _ = self.engine.add_window(window);
                self.rescue(id, pid);
            }
            // Inactive apps also report their last focused window. Only
            // the frontmost app can change the global focus/workspace.
            if self.frontmost == Some(report.pid) {
                if let Some(id) = report.focused {
                    let _ = self.engine.observed_focus(id);
                }
            }
        }
        if !self.engine.config.automatic_tiling {
            let mut frames = self.recovery.clone();
            frames.extend(self.restoring.iter().cloned());
            frames.extend(self.manual_parked_recovery(&self.parked));
            if let Err(error) = self.write_journal(frames) {
                self.message = Some(format!("Could not update window recovery journal: {error}"));
            }
        }
    }

    /// Manual mode: a window found at a parking spot that Sift is not
    /// holding there (lost while on another Space, left by a crash, or
    /// reopened where its app saved a parked frame) comes back on screen.
    fn rescue(&mut self, id: WindowId, pid: i32) {
        if self.engine.config.automatic_tiling
            || self.parked.contains(&id)
            || self.restoring.iter().any(|r| r.id == id && r.pid == pid)
        {
            return;
        }
        let Some(frame) = self.engine.rescue_offscreen(id) else {
            return;
        };
        let visible = self.engine.windows.get(&id).is_some_and(|w| {
            w.available() && self.engine.visible.values().any(|n| n == &w.workspace)
        });
        tracing::info!(id, visible, "moved a stranded window back on screen");
        // A hidden workspace's window keeps the rescued frame for its return.
        if visible {
            if let Some(worker) = self.workers.get(&pid) {
                worker.send(Task::Restore(vec![(id, frame)]));
            }
        }
    }

    fn command(&mut self, command: Command) -> Reply {
        let command = if command == Command::Reload {
            #[derive(Deserialize)]
            struct Settings {
                #[serde(default)]
                window_manager: Config,
            }
            let loaded = std::fs::read_to_string(&self.config_path)
                .map_err(|e| e.to_string())
                .and_then(|text| toml::from_str::<Settings>(&text).map_err(|e| e.to_string()))
                .and_then(|settings| self.reconfigure(settings.window_manager));
            match loaded {
                Ok(()) => Command::Status,
                Err(e) => return Reply::error(e),
            }
        } else {
            command
        };
        match self.engine.execute(command.clone()) {
            Ok(effects) => {
                self.message = None;
                self.apply(effects);
                self.publish();
                if command == Command::ListModes {
                    Reply {
                        ok: true,
                        error: None,
                        data: serde_json::json!(
                            self.engine.config.bindings.keys().collect::<Vec<_>>()
                        ),
                    }
                } else {
                    Reply::snapshot(&command, &self.engine.snapshot())
                }
            }
            Err(e) => {
                self.message = Some(e.to_string());
                Reply::error(e)
            }
        }
    }

    fn reconfigure(&mut self, config: Config) -> Result<(), String> {
        if config == self.engine.config {
            return Ok(());
        }
        let hooks_changed = config.hooks != self.engine.config.hooks;
        let leaving_tiling = self.engine.config.automatic_tiling && !config.automatic_tiling;
        config.validate().map_err(|e| e.to_string())?;
        if leaving_tiling {
            self.restore();
            // Native fullscreen/minimized windows are restored only after
            // macOS returns control of their frames.
            for frame in &self.last_journal {
                if self
                    .engine
                    .windows
                    .get(&frame.id)
                    .is_some_and(|w| w.pid == frame.pid && (w.minimized || w.native_fullscreen))
                    && !self
                        .recovery
                        .iter()
                        .any(|r| r.id == frame.id && r.pid == frame.pid)
                {
                    self.recovery.push(frame.clone());
                }
            }
        }
        self.engine
            .reconfigure(config.clone())
            .map_err(|e| e.to_string())?;
        if hooks_changed {
            self.hooks = None;
        }
        self.mouse_held = false;
        self.pending_launches.clear();
        self.engine.enabled = config.enabled;
        *self
            .shared_config
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(config);
        self.applications(self.available_apps.clone(), true);
        Ok(())
    }

    fn apply(&mut self, effects: Vec<Effect>) {
        if !self.engine.enabled {
            self.pending_launches.clear();
            self.restore();
            return;
        }
        let mut by_pid: BTreeMap<i32, Vec<Effect>> = BTreeMap::new();
        let mut effects = effects;
        effects.extend(self.engine.reflow());
        let hidden: BTreeSet<_> = self.engine.hidden().into_iter().collect();
        if !self.engine.config.automatic_tiling {
            for id in self.parked.difference(&hidden) {
                if let Some(w) = self.engine.windows.get(id) {
                    effects.push(Effect::Frame(*id, w.frame));
                }
            }
            for frame in
                self.manual_parked_recovery(&self.parked.difference(&hidden).copied().collect())
            {
                self.restoring
                    .retain(|r| r.id != frame.id || r.pid != frame.pid);
                self.restoring.push(frame);
            }
        }
        if let Some(target) = self
            .engine
            .config
            .mouse
            .follows_focus
            .filter(|_| self.engine.config.automatic_tiling)
        {
            if self.last_published.as_ref().is_some_and(|previous| {
                previous.focused != self.engine.focused
                    || previous.active_monitor != self.engine.active_monitor
            }) {
                if let Ok(rect) = self.engine.cursor_destination(target) {
                    effects.push(Effect::Cursor(rect, target.lazy()));
                }
            }
        }
        // Persist original positions before the first destructive operation.
        // On a write failure leave every window where it was.
        if let Err(e) = if self.engine.config.automatic_tiling {
            self.save_journal()
        } else {
            // An explicit manual move supersedes an in-flight restoration.
            // Never let an old automatic journal undo it at the next startup.
            let moved: BTreeSet<_> = effects
                .iter()
                .filter_map(|e| {
                    if let Effect::Frame(id, _) = e
                        && !self.parked.contains(id)
                    {
                        Some(*id)
                    } else {
                        None
                    }
                })
                .collect();
            self.recovery.retain(|r| !moved.contains(&r.id));
            self.restoring.retain(|r| !moved.contains(&r.id));
            let mut frames = self.recovery.clone();
            frames.extend(self.restoring.iter().cloned());
            frames.extend(self.manual_parked_recovery(&hidden));
            self.write_journal(frames)
        } {
            self.message = Some(format!(
                "Window layout was not changed: cannot save recovery positions: {e}"
            ));
            return;
        }
        for id in &hidden {
            if let Some(frame) = self.engine.parking_frame(*id) {
                // macOS keeps a small part of a window on screen. Park at
                // the lower-right edge; do not minimize or change native Spaces.
                effects.push(Effect::Frame(*id, frame));
            }
        }
        self.parked = hidden;
        // Keep the last desired frame and focus per window; a single command
        // must not perform a tile frame followed by a fullscreen frame.
        let mut seen_frames = BTreeSet::new();
        let mut seen_focus = false;
        effects.reverse();
        effects.retain(|e| match e {
            Effect::Frame(id, _) => seen_frames.insert(*id),
            Effect::Focus(_) => {
                let keep = !seen_focus;
                seen_focus = true;
                keep
            }
            _ => true,
        });
        effects.reverse();
        for effect in effects {
            let id = match effect {
                Effect::LaunchTarget {
                    name,
                    app_id,
                    command,
                } => {
                    self.launch_target(name, app_id, *command);
                    continue;
                }
                Effect::LaunchWorkspaceApp { workspace, app_id } => {
                    self.launch_workspace_app(workspace, app_id);
                    continue;
                }
                Effect::Exec(ref shell) => {
                    spawn_shell(shell);
                    continue;
                }
                Effect::Mode(_) => continue,
                Effect::Cursor(rect, lazy) => {
                    if let Err(error) = super::mouse::move_cursor(rect, lazy) {
                        self.message = Some(error);
                    }
                    continue;
                }
                Effect::Frame(_, _) if self.mouse_held => continue,
                Effect::Frame(id, _)
                | Effect::Focus(id)
                | Effect::Raise(id)
                | Effect::Close(id)
                | Effect::Minimize(id) => id,
                Effect::NativeFullscreen(id, _) => id,
            };
            if let Some(w) = self.engine.windows.get(&id) {
                by_pid.entry(w.pid).or_default().push(effect);
            }
        }
        for (pid, effects) in by_pid {
            if let Some(worker) = self.workers.get(&pid) {
                worker.send(Task::Apply(effects));
            }
        }
    }

    fn restore(&mut self) {
        if !self.engine.config.automatic_tiling {
            for (pid, worker) in &self.workers {
                let frames: Vec<_> = self
                    .parked
                    .iter()
                    .filter_map(|id| self.engine.windows.get(id))
                    .filter(|w| w.pid == *pid)
                    .map(|w| (w.id, w.frame))
                    .collect();
                worker.send(Task::Restore(frames));
            }
            self.parked.clear();
            return;
        }
        for (pid, worker) in &self.workers {
            let frames: Vec<_> = self
                .engine
                .windows
                .values()
                .filter(|w| w.pid == *pid && !w.native_fullscreen && !w.minimized)
                .map(|w| {
                    (
                        w.id,
                        w.original_frame.constrained(
                            self.engine
                                .monitors
                                .values()
                                .find(|monitor| {
                                    let (x, y) = w.original_frame.center();
                                    monitor.frame.contains(x, y)
                                })
                                .or_else(|| {
                                    self.engine
                                        .monitors
                                        .get(&self.engine.workspaces[&w.workspace].monitor)
                                })
                                .map(|m| m.usable)
                                .unwrap_or(w.original_frame),
                        ),
                    )
                })
                .collect();
            for (id, frame) in &frames {
                if let Some(window) = self.engine.windows.get(id) {
                    self.restoring.retain(|r| r.id != *id || r.pid != *pid);
                    self.restoring.push(RecoveryFrame {
                        id: *id,
                        pid: *pid,
                        app_id: window.app_id.clone(),
                        process_started: Some(worker.identity),
                        frame: *frame,
                    });
                }
            }
            worker.send(Task::Restore(frames));
        }
        self.parked.clear();
        // Keep the journal until next startup confirms that the windows are
        // back. This also covers a shutdown during a blocked AX call.
    }

    fn launch_target(&mut self, name: String, app_id: String, command: Command) {
        if let Some(pending) = self.pending_launches.get_mut(&name) {
            pending.command = command;
            return;
        }
        if self.pending_launches.len() >= 32 {
            self.message = Some("Too many pending application launches".into());
            return;
        }
        self.launch_serial = self.launch_serial.wrapping_add(1);
        let serial = self.launch_serial;
        self.pending_launches.insert(
            name.clone(),
            PendingLaunch {
                command,
                workspace: None,
                awaiting_existing: false,
                deadline: Instant::now() + Duration::from_secs(8),
                serial,
            },
        );
        self.message = Some(if name.starts_with("@workspace:") {
            format!("Opening {app_id}…")
        } else {
            format!("Opening {app_id} for target {name:?}…")
        });
        let tx = self.tx.clone();
        let target = name.clone();
        let background = name.starts_with("@workspace:");
        let path = self.known_apps.get(&app_id).cloned();
        // Launch Services via a fixed executable and separate arguments; no shell,
        // per-keystroke process, or main-thread work for an existing match.
        let result = std::thread::Builder::new()
            .name("sift-wm-open".into())
            .spawn(move || {
                let open = |path: Option<&Path>| {
                    let mut process = std::process::Command::new("/usr/bin/open");
                    if background {
                        process.arg("-g");
                    }
                    if let Some(path) = path {
                        process.arg("-a").arg(path);
                    } else {
                        process.args(["-b", &app_id]);
                    }
                    process
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .output()
                        .map_err(|e| format!("Could not open {app_id}: {e}"))
                        .and_then(|output| {
                            if output.status.success() {
                                Ok(())
                            } else {
                                let detail: String = String::from_utf8_lossy(&output.stderr)
                                    .trim()
                                    .chars()
                                    .take(512)
                                    .collect();
                                Err(format!("Could not open {app_id}: {detail}"))
                            }
                        })
                };
                // Prefer the exact copy previously observed. Bundle-ID lookup
                // remains available for first launches or moved applications.
                let result = match path {
                    Some(path) => open(Some(&path)).or_else(|_| open(None)),
                    None => open(None),
                };
                let _ = tx.send(Event::LaunchCompleted(target, serial, result));
            });
        if let Err(error) = result {
            self.pending_launches.remove(&name);
            self.message = Some(format!("Could not start application launch: {error}"));
        }
    }

    fn launch_workspace_app(&mut self, workspace: String, app_id: String) {
        let key = format!("@workspace:{app_id}");
        if let Some(pending) = self.pending_launches.get_mut(&key) {
            self.message = Some(if pending.awaiting_existing {
                format!("Finding {app_id} window…")
            } else {
                format!("Opening {app_id}…")
            });
            pending.workspace = Some((workspace, app_id));
            return;
        }
        if self.available_apps.iter().any(|app| app.bundle == app_id) {
            // The app is running but its AX worker may still be discovering
            // windows. Give it a moment before asking macOS to reopen it.
            if self.pending_launches.len() >= 32 {
                self.message = Some("Too many pending application launches".into());
                return;
            }
            self.message = Some(format!("Finding {app_id} window…"));
            self.pending_launches.insert(
                key,
                PendingLaunch {
                    command: Command::Status,
                    workspace: Some((workspace, app_id)),
                    awaiting_existing: true,
                    deadline: Instant::now() + Duration::from_millis(750),
                    serial: 0,
                },
            );
            return;
        }
        self.launch_target(key.clone(), app_id.clone(), Command::Status);
        if let Some(pending) = self.pending_launches.get_mut(&key) {
            pending.workspace = Some((workspace, app_id));
        }
    }

    fn finish_launches(&mut self) {
        let ready: Vec<_> = self
            .pending_launches
            .iter()
            .filter_map(|(name, pending)| {
                let matched = if let Some((workspace, app_id)) = &pending.workspace {
                    self.engine
                        .windows
                        .values()
                        .find(|w| &w.workspace == workspace && &w.app_id == app_id)
                        .map(|w| w.id)
                } else {
                    self.engine.resolve_target(name).ok().flatten()
                };
                (matched.is_some() || Instant::now() >= pending.deadline)
                    .then(|| (name.clone(), matched))
            })
            .collect();
        for (name, matched) in ready {
            let pending = self.pending_launches.remove(&name).expect("pending launch");
            if matched.is_none() && pending.awaiting_existing {
                if let Some((workspace, app_id)) = pending.workspace {
                    self.launch_target(name.clone(), app_id.clone(), Command::Status);
                    if let Some(next) = self.pending_launches.get_mut(&name) {
                        next.workspace = Some((workspace, app_id));
                    }
                }
                continue;
            }
            if let Some(id) = matched {
                if let Some((workspace, _)) = pending.workspace {
                    self.message = None;
                    let snapshot = self.engine.snapshot();
                    if workspace_is_current(&snapshot, &workspace) {
                        match self.engine.execute(Command::FocusId(id)) {
                            Ok(effects) => self.apply(effects),
                            Err(error) => self.message = Some(error.to_string()),
                        }
                    }
                    continue;
                }
                match self.engine.execute(pending.command) {
                    Ok(effects) => {
                        self.message = None;
                        self.apply(effects);
                    }
                    Err(error) => self.message = Some(error.to_string()),
                }
            } else {
                self.message = Some(if let Some((workspace, app_id)) = pending.workspace {
                    format!("No window for {app_id} appeared on workspace {workspace}.")
                } else {
                    format!(
                        "No window matched target {name:?} after opening its app. Sift cannot recreate a particular document or tab."
                    )
                });
            }
        }
    }

    fn save_journal(&mut self) -> std::io::Result<()> {
        let mut frames: Vec<_> = self
            .engine
            .windows
            .values()
            .map(|w| RecoveryFrame {
                id: w.id,
                pid: w.pid,
                app_id: w.app_id.clone(),
                process_started: self.workers.get(&w.pid).map(|worker| worker.identity),
                frame: w.original_frame,
            })
            .collect();
        frames.extend(
            self.recovery
                .iter()
                .filter(|r| {
                    !self.engine.windows.contains_key(&r.id)
                        && r.process_started.is_some()
                        && super::worker::process_identity(r.pid) == r.process_started
                })
                .cloned(),
        );
        self.write_journal(frames)
    }

    fn manual_parked_recovery(&self, ids: &BTreeSet<u32>) -> Vec<RecoveryFrame> {
        ids.iter()
            .filter_map(|id| self.engine.windows.get(id))
            .map(|w| RecoveryFrame {
                id: w.id,
                pid: w.pid,
                app_id: w.app_id.clone(),
                process_started: self.workers.get(&w.pid).map(|worker| worker.identity),
                frame: w.frame,
            })
            .collect()
    }

    fn write_journal(&mut self, mut frames: Vec<RecoveryFrame>) -> std::io::Result<()> {
        frames.sort_by_key(|r| (r.pid, r.id));
        if frames.len() == self.last_journal.len()
            && frames.iter().zip(&self.last_journal).all(|(a, b)| {
                a.id == b.id
                    && a.pid == b.pid
                    && a.app_id == b.app_id
                    && a.process_started == b.process_started
                    && a.frame == b.frame
            })
        {
            return Ok(());
        }
        let temp = self.journal.with_extension("tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        serde_json::to_writer(&mut file, &frames)?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(temp, &self.journal)?;
        self.last_journal = frames;
        Ok(())
    }

    fn publish(&mut self) {
        let mut snapshot = self.engine.snapshot();
        snapshot.message.clone_from(&self.message);
        if self.last_published.as_ref() == Some(&snapshot) {
            return;
        }
        let jobs = super::hooks::events(
            &self.engine.config.hooks,
            self.last_published.as_ref(),
            &snapshot,
            self.journal
                .parent()
                .and_then(Path::parent)
                .unwrap_or(Path::new(".")),
        );
        if !jobs.is_empty() {
            if self.hooks.is_none() {
                match super::hooks::Runner::start() {
                    Ok(runner) => self.hooks = Some(runner),
                    Err(error) => self.message = Some(error),
                }
            }
            if let Some(runner) = &self.hooks {
                for jobs in jobs {
                    if let Err(error) = runner.submit(jobs) {
                        self.message = Some(error);
                    }
                }
            }
            snapshot.message.clone_from(&self.message);
        }
        self.last_published = Some(snapshot.clone());
        let snapshot = Arc::new(snapshot);
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = snapshot.clone();
        (self.publish)(snapshot);
    }
}

fn remove_absent(engine: &mut Engine, report: &super::worker::Report) -> Vec<WindowId> {
    let present: BTreeSet<_> = report.present_ids.iter().copied().collect();
    let gone: Vec<_> = engine
        .windows
        .values()
        .filter(|w| w.pid == report.pid && !present.contains(&w.id))
        .map(|w| w.id)
        .collect();
    for id in &gone {
        engine.remove_window(*id);
    }
    gone
}

fn ipc(
    listener: UnixListener,
    path: PathBuf,
    tx: mpsc::SyncSender<Event>,
    stopping: Arc<AtomicBool>,
) {
    for connection in listener.incoming() {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        let Ok(mut stream) = connection else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
        // The directory is 0700; verify the peer uid as a second boundary.
        let mut uid = 0;
        let mut gid = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0
            || uid != unsafe { libc::geteuid() }
        {
            continue;
        }
        let reply = match protocol::read_frame::<Request>(&mut stream) {
            Ok(request) if request.version == protocol::VERSION && request.args.len() <= 64 => {
                match sift_wm::protocol_command(&request.args) {
                    Ok(command) => {
                        let (reply_tx, reply_rx) = mpsc::channel();
                        match tx.try_send(Event::Command(command, Some(reply_tx))) {
                            Ok(()) => reply_rx
                                .recv_timeout(Duration::from_secs(2))
                                .unwrap_or_else(|_| {
                                    Reply::error("window manager response timed out")
                                }),
                            Err(_) => Reply::error("window manager is busy or stopped"),
                        }
                    }
                    Err(e) => Reply::error(e),
                }
            }
            Ok(_) => Reply::error("unsupported protocol version or too many arguments"),
            Err(e) => Reply::error(e),
        };
        if let Err(error) = protocol::write_frame(&mut stream, &reply) {
            let _ = protocol::write_frame(&mut stream, &Reply::error(error));
        }
    }
    let _ = std::fs::remove_file(path);
}

fn spawn_shell(shell: &str) {
    // Only commands already present in the user's configuration reach here;
    // IPC cannot submit Exec. Reap children without blocking the model worker.
    match std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(shell)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let _ = std::thread::Builder::new()
                .name("sift-wm-command".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
        Err(e) => tracing::warn!(%e, "window manager hook failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_ax_metadata_keeps_the_original_recovery_frame_until_the_window_is_absent() {
        let mut engine = Engine::new(
            Config::default(),
            vec![Monitor {
                id: 1,
                name: "Test".into(),
                frame: Rect::new(0., 0., 1200., 900.),
                usable: Rect::new(0., 24., 1200., 850.),
                main: true,
            }],
        )
        .expect("engine");
        let original = Rect::new(100., 100., 400., 300.);
        engine
            .add_window(sift_wm::Window::new(7, 100, original))
            .expect("window");
        engine.reflow();
        let mut report = super::super::worker::Report {
            pid: 100,
            present_ids: vec![7],
            ..Default::default()
        };
        assert!(remove_absent(&mut engine, &report).is_empty());
        engine
            .add_window(sift_wm::Window::new(
                7,
                100,
                Rect::new(1199., 899., 400., 300.),
            ))
            .expect("recovered metadata");
        assert_eq!(engine.windows[&7].original_frame, original);
        report.present_ids.clear();
        assert_eq!(remove_absent(&mut engine, &report), [7]);
        assert!(engine.windows.is_empty());
    }

    fn service(dir: &Path) -> Controller {
        service_with_publish(dir, |_| {})
    }

    fn service_with_publish(
        dir: &Path,
        publish: impl Fn(Arc<Snapshot>) + Send + 'static,
    ) -> Controller {
        let config = Config {
            automatic_tiling: true,
            enabled: true,
            ..Config::default()
        };
        service_with_config(dir, config, publish)
    }

    fn service_with_config(
        dir: &Path,
        config: Config,
        publish: impl Fn(Arc<Snapshot>) + Send + 'static,
    ) -> Controller {
        let (tx, rx) = mpsc::sync_channel(256);
        start(
            config,
            dir,
            &dir.join("config.toml"),
            vec![Monitor {
                id: 1,
                name: "Test".into(),
                frame: Rect::new(0., 0., 1200., 900.),
                usable: Rect::new(0., 24., 1200., 850.),
                main: true,
            }],
            (Vec::new(), None),
            (tx, rx),
            publish,
        )
        .expect("headless model service")
    }

    #[test]
    fn slow_startup_hooks_do_not_block_the_control_service() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("hook-started");
        let config = Config {
            automatic_tiling: true,
            enabled: true,
            hooks: sift_wm::Hooks {
                after_startup: vec![format!(
                    "printf started > '{}'; sleep 0.5",
                    marker.display()
                )],
                ..sift_wm::Hooks::default()
            },
            ..Config::default()
        };
        let controller = service_with_config(dir.path(), config, |_| {});
        let started = Instant::now();
        while !marker.exists() {
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(10));
        }
        let request = Instant::now();
        let reply = protocol::request(dir.path(), vec!["workspace".into(), "code".into()])
            .expect("request");
        assert!(reply.ok);
        assert!(
            request.elapsed() < Duration::from_millis(250),
            "hook blocked the model"
        );
        controller.shutdown();
        assert!(controller.wait_stopped(Duration::from_secs(3)));
    }

    #[test]
    fn panic_during_publication_stops_the_service_and_releases_ownership() {
        let dir = tempfile::tempdir().expect("tempdir");
        let controller = service_with_publish(dir.path(), |_| panic!("injected publish failure"));
        assert!(controller.wait_stopped(Duration::from_secs(3)));
        assert!(!protocol::socket_path(dir.path()).exists());
        let next = service(dir.path());
        assert!(!controller.same_instance(&next));
        assert!(next.same_instance(&next.clone()));
        next.shutdown();
        assert!(next.wait_stopped(Duration::from_secs(3)));
    }

    #[test]
    fn control_socket_round_trips_commands_and_releases_ownership_on_shutdown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let controller = service(dir.path());
        let reply = protocol::request(dir.path(), vec!["workspace".into(), "code".into()])
            .expect("request");
        assert!(reply.ok);
        assert!(
            reply.data["visible"]
                .as_object()
                .expect("visible")
                .values()
                .any(|v| v == "code")
        );
        let reply = protocol::request(dir.path(), vec!["list-windows".into()]).expect("request");
        assert_eq!(reply.data, serde_json::json!([]));
        let metadata =
            std::fs::metadata(protocol::socket_path(dir.path()).parent().expect("parent"))
                .expect("metadata");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        controller.shutdown();
        assert!(controller.wait_stopped(Duration::from_secs(3)));
        assert!(!protocol::socket_path(dir.path()).exists());
        let second = service(dir.path());
        second.shutdown();
        assert!(second.wait_stopped(Duration::from_secs(3)));
    }

    #[test]
    fn workspace_launch_failure_is_reported_without_opening_launcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config {
            automatic_tiling: true,
            enabled: true,
            ..Config::default()
        };
        config.rules.push(sift_wm::Rule {
            app_id: Some(r"^org\.sift\.nonexistent\.workspace\.test$".into()),
            workspace: Some("1".into()),
            ..Default::default()
        });
        let controller = service_with_config(dir.path(), config, |_| {});
        let reply =
            protocol::request(dir.path(), vec!["workspace".into(), "1".into()]).expect("request");
        assert!(reply.ok);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = controller.snapshot();
            if snapshot
                .message
                .as_deref()
                .is_some_and(|m| m.contains("Could not open") || m.contains("No window for"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "launch failure was not reported: {:?}",
                snapshot.message
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        controller.shutdown();
        assert!(controller.wait_stopped(Duration::from_secs(3)));
    }

    #[test]
    fn running_app_gets_time_for_window_discovery_before_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config {
            automatic_tiling: true,
            enabled: true,
            ..Config::default()
        };
        config.rules.push(sift_wm::Rule {
            app_id: Some(r"^org\.sift\.nonexistent\.workspace\.test$".into()),
            workspace: Some("1".into()),
            ..Default::default()
        });
        let app = super::super::AppInfo {
            pid: i32::MAX,
            name: "Fixture".into(),
            bundle: "org.sift.nonexistent.workspace.test".into(),
            hidden: false,
            path: None,
        };
        let (tx, rx) = mpsc::sync_channel(256);
        let controller = start(
            config,
            dir.path(),
            &dir.path().join("config.toml"),
            vec![Monitor {
                id: 1,
                name: "Test".into(),
                frame: Rect::new(0., 0., 1200., 900.),
                usable: Rect::new(0., 24., 1200., 850.),
                main: true,
            }],
            (vec![app], None),
            (tx, rx),
            |_| {},
        )
        .expect("headless service");
        let args = vec!["workspace".into(), "1".into()];
        for _ in 0..2 {
            let reply = protocol::request(dir.path(), args.clone()).expect("request");
            assert!(reply.ok);
            assert!(
                controller
                    .snapshot()
                    .message
                    .as_deref()
                    .is_some_and(|m| m.starts_with("Finding "))
            );
        }
        // Repeated presses must not force an immediate reopen.
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            controller
                .snapshot()
                .message
                .as_deref()
                .is_some_and(|m| m.starts_with("Finding "))
        );
        controller.shutdown();
        assert!(controller.wait_stopped(Duration::from_secs(3)));
    }

    #[test]
    fn a_late_workspace_launch_must_not_steal_focus_after_switching_away() {
        let config = Config {
            automatic_tiling: true,
            enabled: true,
            ..Config::default()
        };
        let mut engine = Engine::new(
            config,
            vec![Monitor {
                id: 1,
                name: "Test".into(),
                frame: Rect::new(0., 0., 1200., 900.),
                usable: Rect::new(0., 24., 1200., 850.),
                main: true,
            }],
        )
        .expect("engine");
        engine
            .execute(Command::Workspace("1".into()))
            .expect("first workspace");
        assert!(workspace_is_current(&engine.snapshot(), "1"));
        engine
            .execute(Command::Workspace("2".into()))
            .expect("switch away");
        assert!(!workspace_is_current(&engine.snapshot(), "1"));
    }

    #[test]
    fn reload_changes_rules_and_bindings_but_a_malformed_file_preserves_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let controller = service(dir.path());
        std::fs::write(dir.path().join("config.toml"), "[window_manager]\nenabled = true\nworkspaces = ['code', 'web']\n[window_manager.bindings.main]\nctrl-alt-c = ['workspace code']\n").expect("config");
        let reply = protocol::request(dir.path(), vec!["reload-config".into()]).expect("request");
        assert!(reply.ok, "{:?}", reply.error);
        assert_eq!(controller.config().workspaces, ["code", "web"]);
        assert_eq!(controller.config().bindings["main"].len(), 1);
        let revision = controller.snapshot().revision;
        std::fs::write(dir.path().join("config.toml"), "[window_manager\n").expect("broken config");
        let reply = protocol::request(dir.path(), vec!["reload-config".into()]).expect("request");
        assert!(!reply.ok);
        assert_eq!(controller.snapshot().revision, revision);
        controller.shutdown();
        assert!(controller.wait_stopped(Duration::from_secs(3)));
    }

    #[test]
    fn oversized_and_unsupported_protocol_messages_do_not_stop_the_service() {
        let dir = tempfile::tempdir().expect("tempdir");
        let controller = service(dir.path());
        let mut stream = UnixStream::connect(protocol::socket_path(dir.path())).expect("connect");
        stream
            .write_all(&((protocol::MAX_FRAME + 1) as u32).to_le_bytes())
            .expect("header");
        let reply: Reply = protocol::read_frame(&mut stream).expect("reply");
        assert!(!reply.ok);
        let mut stream = UnixStream::connect(protocol::socket_path(dir.path())).expect("connect");
        protocol::write_frame(
            &mut stream,
            &Request {
                version: 99,
                args: vec!["status".into()],
            },
        )
        .expect("write");
        let reply: Reply = protocol::read_frame(&mut stream).expect("reply");
        assert!(!reply.ok);
        assert!(
            protocol::request(dir.path(), vec!["status".into()])
                .expect("request")
                .ok
        );
        controller.shutdown();
        assert!(controller.wait_stopped(Duration::from_secs(3)));
    }

    #[test]
    fn instance_lock_prevents_two_managers_from_controlling_the_same_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let controller = service(dir.path());
        let (tx, rx) = mpsc::sync_channel(256);
        let result = start(
            Config::default(),
            dir.path(),
            &dir.path().join("config.toml"),
            vec![Monitor {
                id: 1,
                name: "Test".into(),
                frame: Rect::new(0., 0., 1200., 900.),
                usable: Rect::new(0., 24., 1200., 850.),
                main: true,
            }],
            (Vec::new(), None),
            (tx, rx),
            |_| {},
        );
        assert!(result.is_err());
        controller.shutdown();
        assert!(controller.wait_stopped(Duration::from_secs(3)));
    }
}
