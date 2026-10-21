//! Keeps the file index current while the launcher runs.
//!
//! One background thread owns the writable [`Store`]. It:
//!
//! 1. at startup, checks each root: missing roots are marked unavailable
//!    (their index is kept, for unplugged drives); roots without a valid
//!    saved FSEvents position are rescanned once;
//! 2. watches all available roots with one FSEvents stream, replaying the
//!    history since the oldest saved position, so changes made while sift
//!    was not running are applied too;
//! 3. batches events (FSEvents latency, then a short quiet period), applies
//!    them with `sift_indexer::apply_changes`, rebuilds the name snapshot
//!    and swaps it into the shared [`FileIndex`];
//! 4. runs [`Command`]s from the settings UI.
//!
//! Lost history (dropped events, wrapped ids, a reset event store) falls
//! back to rescanning the affected subtree or root, so the index recovers
//! without user action.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use sift_indexer::{Change, IndexError, IndexOptions, apply_changes, index_root_at};
use sift_store::{EventMark, RootInfo, Store, UpdateSummary};
use sift_watch::{Event, Watcher, current_event_id, event_store_uuid};

use crate::FileIndex;

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub index_dir: PathBuf,
    /// FSEvents latency: how long FSEvents collects changes before
    /// delivering them.
    pub latency: Duration,
    /// After a delivery, wait this long for more events before applying.
    pub quiet: Duration,
    /// Apply at the latest this long after the first event of a batch,
    /// even if events keep arriving.
    pub max_delay: Duration,
}

impl ServiceConfig {
    pub fn new(index_dir: impl Into<PathBuf>) -> Self {
        Self {
            index_dir: index_dir.into(),
            latency: Duration::from_millis(300),
            quiet: Duration::from_millis(150),
            max_delay: Duration::from_secs(2),
        }
    }
}

/// Requests from the UI.
#[derive(Debug, Clone)]
pub enum Command {
    /// Index a new folder and start watching it.
    AddRoot {
        path: PathBuf,
        options: IndexOptions,
    },
    /// Stop indexing a folder and delete its entries.
    RemoveRoot(PathBuf),
    /// Rescan a folder from scratch with its saved options.
    Reindex(PathBuf),
    /// Change a folder's options (rescans it).
    SetOptions {
        path: PathBuf,
        options: IndexOptions,
    },
    /// Re-check root availability and pick up changes made by other
    /// processes (such as `sift index`). Cheap; the launcher sends it each
    /// time the panel opens.
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootState {
    /// Indexed and watched for changes.
    Watching,
    /// A full scan is running.
    Indexing { entries: u64 },
    /// The folder is missing (deleted, renamed, or on an unmounted drive).
    /// Its last index is kept and still searchable.
    Unavailable,
    /// The last scan failed.
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct RootStatus {
    pub path: PathBuf,
    pub entries: u64,
    /// Items that could not be read during the last full scan.
    pub errors: u64,
    /// Unix time of the last full scan.
    pub indexed_at: i64,
    pub options: IndexOptions,
    pub state: RootState,
}

/// The most recent incremental update.
#[derive(Debug, Clone, Copy)]
pub struct UpdateInfo {
    pub at: SystemTime,
    pub changes: UpdateSummary,
    /// Time to apply the changes to the store.
    pub apply: Duration,
    /// Time to rebuild and swap in the name snapshot.
    pub snapshot: Duration,
    /// From the first event of the batch arriving to the new snapshot
    /// being searchable (includes the batching delay, not FSEvents'
    /// own latency).
    pub event_to_searchable: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub roots: Vec<RootStatus>,
    /// Database plus snapshot size on disk.
    pub index_bytes: u64,
    pub last_update: Option<UpdateInfo>,
    /// The last error a command produced, for display.
    pub message: Option<String>,
}

enum Msg {
    Command(Command),
    Events(Vec<Event>, Instant),
    Shutdown,
}

/// Handle to the background index thread. Dropping it stops the thread.
pub struct IndexService {
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl IndexService {
    /// Starts the service. `on_status` runs on the service thread whenever
    /// something visible changes.
    pub fn start(
        config: ServiceConfig,
        index: Arc<FileIndex>,
        on_status: impl Fn(&Status) + Send + 'static,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.index_dir)?;
        let (tx, rx) = channel();
        let events_tx = tx.clone();
        let thread = std::thread::Builder::new()
            .name("sift-index".into())
            .spawn(move || {
                let store = match Store::open(&config.index_dir.join(sift_search::DB_FILE)) {
                    Ok(store) => store,
                    Err(error) => {
                        tracing::error!(%error, "cannot open the index database");
                        on_status(&Status {
                            message: Some(error.to_string()),
                            ..Status::default()
                        });
                        return;
                    }
                };
                let mut worker = Worker {
                    config,
                    store,
                    index,
                    events_tx,
                    watcher: None,
                    states: HashMap::new(),
                    on_status: Box::new(on_status),
                    last_update: None,
                    message: None,
                };
                worker.run(rx);
            })?;
        Ok(Self {
            tx,
            thread: Some(thread),
        })
    }

    pub fn send(&self, command: Command) {
        let _ = self.tx.send(Msg::Command(command));
    }
}

impl Drop for IndexService {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Worker {
    config: ServiceConfig,
    store: Store,
    index: Arc<FileIndex>,
    events_tx: Sender<Msg>,
    watcher: Option<Watcher>,
    states: HashMap<PathBuf, RootState>,
    on_status: Box<dyn Fn(&Status) + Send>,
    last_update: Option<UpdateInfo>,
    message: Option<String>,
}

impl Worker {
    fn run(&mut self, rx: Receiver<Msg>) {
        self.startup();
        let mut pending: Option<Msg> = None;
        loop {
            let msg = match pending.take() {
                Some(msg) => msg,
                None => match rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => break,
                },
            };
            match msg {
                Msg::Shutdown => break,
                Msg::Command(command) => self.command(command),
                Msg::Events(mut events, first_at) => {
                    // Batch: wait for a quiet period, bounded by max_delay.
                    let deadline = first_at + self.config.max_delay;
                    loop {
                        let wait = self
                            .config
                            .quiet
                            .min(deadline.saturating_duration_since(Instant::now()));
                        match rx.recv_timeout(wait) {
                            Ok(Msg::Events(more, _)) => events.extend(more),
                            Ok(other) => {
                                pending = Some(other);
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                        if Instant::now() >= deadline {
                            break;
                        }
                    }
                    self.apply_events(events, first_at);
                }
            }
        }
        // Stop FSEvents before the store goes away.
        self.watcher = None;
    }

    fn startup(&mut self) {
        let roots = self.roots();
        let mut scanned = false;
        for root in &roots {
            if !root.path.is_dir() {
                self.states
                    .insert(root.path.clone(), RootState::Unavailable);
                continue;
            }
            let valid = root
                .event
                .as_ref()
                .is_some_and(|mark| event_store_uuid(&root.path).as_deref() == Some(&mark.uuid));
            if valid {
                self.states.insert(root.path.clone(), RootState::Watching);
            } else {
                // Never watched, or the volume's event history was reset:
                // the saved position is meaningless.
                tracing::info!(root = %root.path.display(), "no valid event position; rescanning");
                self.full_scan(&root.path, options_of(root));
                scanned = true;
            }
        }
        if scanned {
            self.publish_snapshot();
        } else {
            self.index.reload_if_stale();
        }
        self.restart_watcher();
        self.emit();
    }

    fn command(&mut self, command: Command) {
        self.message = None;
        match command {
            Command::AddRoot { path, options } => {
                let path = match path.canonicalize() {
                    Ok(path) => path,
                    Err(error) => {
                        self.message = Some(format!("Cannot open {}: {error}", path.display()));
                        self.emit();
                        return;
                    }
                };
                self.full_scan(&path, options);
                self.publish_snapshot();
                self.restart_watcher();
            }
            Command::RemoveRoot(path) => {
                match self.store.remove_root(&path) {
                    Ok(_) => {
                        self.states.remove(&path);
                    }
                    Err(error) => self.message = Some(error.to_string()),
                }
                self.publish_snapshot();
                self.restart_watcher();
            }
            Command::Reindex(path) => {
                let options = self
                    .roots()
                    .iter()
                    .find(|r| r.path == path)
                    .map(options_of)
                    .unwrap_or_default();
                self.full_scan(&path, options);
                self.publish_snapshot();
                self.restart_watcher();
            }
            Command::SetOptions { path, options } => {
                self.full_scan(&path, options);
                self.publish_snapshot();
                self.restart_watcher();
            }
            Command::Refresh => {
                if self.check_roots() {
                    self.publish_snapshot();
                    self.restart_watcher();
                } else if self.index.reload_if_stale() {
                    tracing::info!("picked up an index change made by another process");
                }
            }
        }
        self.emit();
    }

    /// Re-checks which roots exist. Rescans roots that came back (their
    /// contents may have changed arbitrarily while away), and adopts roots
    /// another process added. Returns whether anything changed.
    fn check_roots(&mut self) -> bool {
        let mut changed = false;
        let roots = self.roots();
        for root in &roots {
            let available = root.path.is_dir();
            let state = self.states.get(&root.path).cloned();
            match (state, available) {
                (Some(RootState::Unavailable), true) => {
                    tracing::info!(root = %root.path.display(), "root is back; rescanning");
                    self.full_scan(&root.path, options_of(root));
                    changed = true;
                }
                (None, true) => {
                    // Added by another process (`sift index`).
                    self.states.insert(root.path.clone(), RootState::Watching);
                    changed = true;
                }
                (Some(RootState::Unavailable), false) | (Some(RootState::Indexing { .. }), _) => {}
                (_, false) => {
                    tracing::info!(root = %root.path.display(), "root unavailable");
                    self.states
                        .insert(root.path.clone(), RootState::Unavailable);
                    changed = true;
                }
                (Some(_), true) => {}
            }
        }
        self.states
            .retain(|path, _| roots.iter().any(|r| &r.path == path));
        changed
    }

    fn apply_events(&mut self, events: Vec<Event>, first_at: Instant) {
        let roots = self.roots();
        let mut changes: HashMap<PathBuf, Vec<Change>> = HashMap::new();
        let mut rescan_all = false;
        let mut check_roots = false;
        let mut max_id = 0;
        for event in events {
            max_id = max_id.max(event.id);
            let flags = event.flags;
            if flags.history_done() {
                continue;
            }
            if flags.ids_wrapped() {
                rescan_all = true;
                continue;
            }
            if flags.root_changed() || flags.mount() || flags.unmount() {
                check_roots = true;
                continue;
            }
            let Some(root) = roots
                .iter()
                .filter(|r| event.path.starts_with(&r.path))
                .max_by_key(|r| r.path.as_os_str().len())
            else {
                continue;
            };
            changes.entry(root.path.clone()).or_default().push(Change {
                path: event.path,
                recursive: flags.must_scan_subdirs() || flags.dropped(),
            });
        }

        if rescan_all {
            tracing::warn!("FSEvents ids wrapped; rescanning every root");
            for root in roots.iter().filter(|r| r.path.is_dir()) {
                self.full_scan(&root.path, options_of(root));
            }
            self.publish_snapshot();
            self.restart_watcher();
            self.emit();
            return;
        }

        let started = Instant::now();
        let mut total = UpdateSummary::default();
        for root in &roots {
            if self.states.get(&root.path) != Some(&RootState::Watching) {
                continue;
            }
            // Every event up to `max_id` for any watched path has been
            // delivered (one stream, in id order), so every root's position
            // can advance to it, changed or not.
            let mark = (max_id > 0)
                .then(|| event_store_uuid(&root.path))
                .flatten()
                .map(|uuid| EventMark { id: max_id, uuid });
            match changes.remove(&root.path) {
                Some(root_changes) => {
                    let options = options_of(root);
                    match apply_changes(
                        &mut self.store,
                        root,
                        &options,
                        &root_changes,
                        mark.as_ref(),
                    ) {
                        Ok(report) => {
                            total.inserted += report.summary.inserted;
                            total.removed += report.summary.removed;
                            total.updated += report.summary.updated;
                            tracing::debug!(
                                root = %root.path.display(),
                                changes = root_changes.len(),
                                ?report,
                                "applied filesystem changes"
                            );
                        }
                        Err(IndexError::RootUnavailable(_)) => check_roots = true,
                        Err(error) => {
                            tracing::warn!(%error, root = %root.path.display(), "update failed");
                        }
                    }
                }
                None => {
                    if let Some(mark) = &mark
                        && let Err(error) = self.store.set_root_event(root.id, mark)
                    {
                        tracing::warn!(%error, "cannot save the event position");
                    }
                }
            }
        }
        let apply = started.elapsed();

        if check_roots && self.check_roots() {
            self.publish_snapshot();
            self.restart_watcher();
        } else if total.changed() {
            let snapshot_started = Instant::now();
            self.publish_snapshot();
            self.last_update = Some(UpdateInfo {
                at: SystemTime::now(),
                changes: total,
                apply,
                snapshot: snapshot_started.elapsed(),
                event_to_searchable: first_at.elapsed(),
            });
            tracing::info!(
                inserted = total.inserted,
                removed = total.removed,
                updated = total.updated,
                apply_ms = apply.as_secs_f64() * 1000.,
                event_to_searchable_ms = first_at.elapsed().as_secs_f64() * 1000.,
                "index updated"
            );
        } else {
            return;
        }
        self.emit();
    }

    /// Replaces a root's index with a fresh scan, recording the FSEvents
    /// position from before the scan so nothing that changes during it is
    /// missed.
    fn full_scan(&mut self, path: &Path, options: IndexOptions) {
        let mark = event_store_uuid(path).map(|uuid| EventMark {
            id: current_event_id(),
            uuid,
        });
        self.states
            .insert(path.to_path_buf(), RootState::Indexing { entries: 0 });
        // Progress reports reuse the full status, with this root's count
        // updated, so the settings list does not flicker during a scan.
        let mut base = self.status();
        if !base.roots.iter().any(|r| r.path == path) {
            base.roots.push(RootStatus {
                path: path.to_path_buf(),
                entries: 0,
                errors: 0,
                indexed_at: 0,
                options: options.clone(),
                state: RootState::Indexing { entries: 0 },
            });
        }
        (self.on_status)(&base);
        let mut last_emit = Instant::now();
        let result = {
            let on_status = &self.on_status;
            let store = &mut self.store;
            index_root_at(store, path, &options, mark.as_ref(), |entries| {
                if last_emit.elapsed() > Duration::from_millis(200) {
                    last_emit = Instant::now();
                    for root in base.roots.iter_mut().filter(|r| r.path == path) {
                        root.state = RootState::Indexing { entries };
                        root.entries = entries;
                    }
                    on_status(&base);
                }
            })
        };
        match result {
            Ok(report) => {
                tracing::info!(
                    root = %report.root.display(),
                    entries = report.entries,
                    ms = report.total_time.as_secs_f64() * 1000.,
                    "indexed"
                );
                self.states.insert(report.root, RootState::Watching);
            }
            Err(error) => {
                tracing::warn!(%error, root = %path.display(), "indexing failed");
                self.message = Some(error.to_string());
                let state = match error {
                    IndexError::Root { .. } | IndexError::RootUnavailable(_) => {
                        RootState::Unavailable
                    }
                    other => RootState::Failed(other.to_string()),
                };
                // A failed first scan leaves no root in the store; only
                // keep the state for roots that exist there.
                if self.roots().iter().any(|r| r.path == path) {
                    self.states.insert(path.to_path_buf(), state);
                } else {
                    self.states.remove(path);
                }
            }
        }
    }

    fn publish_snapshot(&mut self) {
        if let Err(error) = sift_search::rebuild_snapshot(&self.store, &self.config.index_dir) {
            tracing::error!(%error, "cannot rebuild the name snapshot");
            return;
        }
        if let Err(error) = self.index.reload() {
            tracing::error!(%error, "cannot reopen the file index");
        }
    }

    fn restart_watcher(&mut self) {
        if let Some(old) = self.watcher.take() {
            // Deliver what the old stream still holds before replacing it.
            old.flush();
        }
        let roots: Vec<RootInfo> = self
            .roots()
            .into_iter()
            .filter(|r| self.states.get(&r.path) == Some(&RootState::Watching))
            .collect();
        if roots.is_empty() {
            return;
        }
        let since = roots
            .iter()
            .filter_map(|r| r.event.as_ref().map(|m| m.id))
            .min();
        let paths: Vec<PathBuf> = roots.iter().map(|r| r.path.clone()).collect();
        let tx = self.events_tx.clone();
        match Watcher::start(&paths, since, self.config.latency, move |events| {
            let _ = tx.send(Msg::Events(events, Instant::now()));
        }) {
            Ok(watcher) => {
                tracing::info!(roots = paths.len(), ?since, "watching for changes");
                self.watcher = Some(watcher);
            }
            Err(error) => {
                tracing::error!(%error, "cannot watch for filesystem changes");
                self.message = Some(format!("Live updates are off: {error}"));
            }
        }
    }

    fn roots(&self) -> Vec<RootInfo> {
        self.store.roots().unwrap_or_else(|error| {
            tracing::error!(%error, "cannot read roots");
            Vec::new()
        })
    }

    fn emit(&self) {
        (self.on_status)(&self.status());
    }

    fn status(&self) -> Status {
        let roots = self
            .roots()
            .into_iter()
            .map(|r| RootStatus {
                state: self
                    .states
                    .get(&r.path)
                    .cloned()
                    .unwrap_or(RootState::Watching),
                options: options_of(&r),
                path: r.path,
                entries: r.entry_count,
                errors: r.error_count,
                indexed_at: r.indexed_at,
            })
            .collect();
        let snapshot = std::fs::metadata(self.config.index_dir.join(sift_search::SNAPSHOT_FILE))
            .map(|m| m.len())
            .unwrap_or(0);
        Status {
            roots,
            index_bytes: self.store.disk_size() + snapshot,
            last_update: self.last_update,
            message: self.message.clone(),
        }
    }
}

fn options_of(root: &RootInfo) -> IndexOptions {
    IndexOptions::decode_saved(&root.options).unwrap_or_default()
}
