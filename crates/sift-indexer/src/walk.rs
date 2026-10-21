use std::collections::HashSet;
use std::fs::{self, FileType, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Condvar, Mutex, MutexGuard};

use sift_store::{EntryId, EntryKind, NewEntry, StoreError};

use crate::{CACHEDIR_TAG, DEPENDENCY_DIR_NAMES, IndexOptions, PACKAGE_EXTENSIONS};

/// Records per batch sent from a worker to the writer.
const BATCH_ROWS: usize = 1024;
/// Batches buffered between workers and the writer. Together with
/// `BATCH_ROWS` this bounds in-flight records to roughly 64k.
const CHANNEL_BATCHES: usize = 64;
/// How often (in entries) the progress callback runs.
const PROGRESS_EVERY: u64 = 50_000;

/// A directory or entry that could not be read during a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    pub path: PathBuf,
    pub kind: std::io::ErrorKind,
    pub message: String,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

pub(crate) struct WalkConfig<'a> {
    pub root: &'a Path,
    pub root_id: EntryId,
    pub root_dev: u64,
    pub options: &'a IndexOptions,
    pub exclude_paths: &'a HashSet<PathBuf>,
}

#[derive(Debug, Default)]
pub(crate) struct WalkStats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub other: u64,
    pub errors: u64,
    pub error_samples: Vec<ScanError>,
}

/// Walks the tree, calling `insert` on the current thread for every entry
/// below the root (the root itself is written by the store).
pub(crate) fn walk(
    config: &WalkConfig<'_>,
    mut insert: impl FnMut(&NewEntry<'_>) -> Result<(), StoreError>,
    progress: &mut dyn FnMut(u64),
) -> Result<WalkStats, StoreError> {
    let queue = DirQueue::new(DirJob {
        path: config.root.to_path_buf(),
        id: config.root_id,
    });
    let next_id = AtomicI64::new(config.root_id + 1);
    // Parents of excluded paths: only entries in these directories need
    // their full path built for the exclusion check.
    let exclude_parents: HashSet<&Path> = config
        .exclude_paths
        .iter()
        .filter_map(|p| p.parent())
        .collect();
    let ctx = WorkerCtx {
        config,
        queue: &queue,
        next_id: &next_id,
        exclude_parents: &exclude_parents,
    };

    std::thread::scope(|scope| {
        let (tx, rx) = sync_channel::<Batch>(CHANNEL_BATCHES);
        for _ in 0..config.options.threads.max(1) {
            let tx = tx.clone();
            let ctx = &ctx;
            scope.spawn(move || ctx.run(tx));
        }
        drop(tx);

        let mut stats = WalkStats::default();
        let mut result = Ok(());
        let mut written = 0u64;
        // Keep draining after a failure so no worker blocks on a full channel.
        for batch in rx {
            if result.is_err() {
                continue;
            }
            result = write_batch(&batch, &mut insert, &mut stats, config.options);
            if result.is_err() {
                queue.abort();
            }
            let before = written / PROGRESS_EVERY;
            written += batch.rows.len() as u64;
            if written / PROGRESS_EVERY != before {
                progress(written);
            }
        }
        result.map(|()| stats)
    })
}

fn write_batch(
    batch: &Batch,
    insert: &mut impl FnMut(&NewEntry<'_>) -> Result<(), StoreError>,
    stats: &mut WalkStats,
    options: &IndexOptions,
) -> Result<(), StoreError> {
    let mut start = 0;
    for row in &batch.rows {
        let end = row.name_end as usize;
        insert(&NewEntry {
            id: row.id,
            parent_id: row.parent_id,
            kind: row.kind,
            name: &batch.names[start..end],
            size: row.size,
            mtime: row.mtime,
        })?;
        start = end;
        match row.kind {
            EntryKind::File => stats.files += 1,
            EntryKind::Dir => stats.dirs += 1,
            EntryKind::Symlink => stats.symlinks += 1,
            EntryKind::Other => stats.other += 1,
        }
    }
    stats.errors += batch.error_count;
    for error in &batch.errors {
        if stats.error_samples.len() >= options.max_error_samples {
            break;
        }
        tracing::debug!(%error, "scan error");
        stats.error_samples.push(error.clone());
    }
    Ok(())
}

struct WorkerCtx<'a> {
    config: &'a WalkConfig<'a>,
    queue: &'a DirQueue,
    next_id: &'a AtomicI64,
    exclude_parents: &'a HashSet<&'a Path>,
}

impl WorkerCtx<'_> {
    fn run(&self, tx: SyncSender<Batch>) {
        let mut batch = Batch::default();
        let mut subdirs = Vec::new();
        while let Some(job) = self.queue.pop() {
            let ok = self.read_dir(&job, &mut batch, &mut subdirs, &tx);
            self.queue.push(&mut subdirs);
            self.queue.finish_one();
            if !ok {
                self.queue.abort();
                return;
            }
        }
        if !batch.is_empty() {
            let _ = tx.send(batch);
        }
    }

    /// Lists one directory. Returns false if the writer has gone away.
    fn read_dir(
        &self,
        job: &DirJob,
        batch: &mut Batch,
        subdirs: &mut Vec<DirJob>,
        tx: &SyncSender<Batch>,
    ) -> bool {
        let options = self.config.options;
        let entries = match fs::read_dir(&job.path) {
            Ok(entries) => entries,
            Err(error) => {
                batch.error(&job.path, &error);
                return true;
            }
        };
        let check_excludes = self.exclude_parents.contains(job.path.as_path());

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    batch.error(&job.path, &error);
                    continue;
                }
            };
            let name = entry.file_name();
            if options.exclude_names.contains(&name) {
                continue;
            }
            // On macOS this comes from the directory listing (d_type), so it
            // costs no extra system call.
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(error) => {
                    batch.error(&job.path.join(&name), &error);
                    continue;
                }
            };
            let is_dir = file_type.is_dir();
            let path = (is_dir || check_excludes).then(|| job.path.join(&name));
            if let Some(path) = &path
                && self.config.exclude_paths.contains(path)
            {
                continue;
            }

            // `DirEntry::metadata` does not follow symlinks (fstatat with
            // AT_SYMLINK_NOFOLLOW).
            let metadata = match entry.metadata() {
                Ok(m) => Some(m),
                // Deleted between listing and inspection: nothing to index.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    batch.error(&job.path.join(&name), &error);
                    None
                }
            };

            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let kind = kind_of(file_type);
            batch.push(id, job.id, kind, name.as_bytes(), metadata.as_ref());

            if let Some(path) = path
                && is_dir
                && self.should_descend(name.as_bytes(), &path, metadata.as_ref())
            {
                subdirs.push(DirJob { path, id });
            }

            if batch.rows.len() >= BATCH_ROWS && tx.send(std::mem::take(batch)).is_err() {
                return false;
            }
        }
        true
    }

    fn should_descend(&self, name: &[u8], path: &Path, metadata: Option<&Metadata>) -> bool {
        let options = self.config.options;
        if options.one_file_system
            && let Some(m) = metadata
            && m.dev() != self.config.root_dev
        {
            return false;
        }
        if !options.descend_packages && is_package(name) {
            return false;
        }
        if options.skip_dependency_dirs
            && (is_dependency_dir(name) || path.join(CACHEDIR_TAG).exists())
        {
            return false;
        }
        true
    }
}

fn kind_of(file_type: FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn is_dependency_dir(name: &[u8]) -> bool {
    DEPENDENCY_DIR_NAMES.iter().any(|d| d.as_bytes() == name)
}

fn is_package(name: &[u8]) -> bool {
    let Some(dot) = name.iter().rposition(|&b| b == b'.') else {
        return false;
    };
    let ext = &name[dot + 1..];
    PACKAGE_EXTENSIONS
        .iter()
        .any(|p| p.as_bytes().eq_ignore_ascii_case(ext))
}

struct Row {
    id: EntryId,
    parent_id: EntryId,
    kind: EntryKind,
    /// End offset of this row's name in `Batch::names`; it starts where the
    /// previous row's name ended.
    name_end: u32,
    size: Option<u64>,
    mtime: Option<i64>,
}

/// A group of discovered entries. Names share one buffer so a batch costs
/// two allocations rather than one per entry.
#[derive(Default)]
struct Batch {
    names: Vec<u8>,
    rows: Vec<Row>,
    errors: Vec<ScanError>,
    error_count: u64,
}

impl Batch {
    fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.error_count == 0
    }

    fn push(
        &mut self,
        id: EntryId,
        parent_id: EntryId,
        kind: EntryKind,
        name: &[u8],
        metadata: Option<&Metadata>,
    ) {
        self.names.extend_from_slice(name);
        self.rows.push(Row {
            id,
            parent_id,
            kind,
            name_end: self.names.len() as u32,
            size: metadata.filter(|_| kind != EntryKind::Dir).map(|m| m.len()),
            mtime: metadata.map(|m| m.mtime()),
        });
    }

    fn error(&mut self, path: &Path, error: &std::io::Error) {
        self.error_count += 1;
        // Workers keep only a few samples; the writer trims further.
        if self.errors.len() < 32 {
            self.errors.push(ScanError {
                path: path.to_path_buf(),
                kind: error.kind(),
                message: error.to_string(),
            });
        }
    }
}

struct DirJob {
    path: PathBuf,
    id: EntryId,
}

/// Shared stack of directories waiting to be read.
///
/// A stack (rather than a FIFO) makes the traversal roughly depth-first,
/// which keeps the number of pending directories small on wide trees.
struct DirQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

struct QueueState {
    stack: Vec<DirJob>,
    /// Directories currently being read. The walk is finished when the
    /// stack is empty and nobody is reading (and could add more work).
    active: usize,
    done: bool,
}

impl DirQueue {
    fn new(root: DirJob) -> Self {
        Self {
            state: Mutex::new(QueueState {
                stack: vec![root],
                active: 0,
                done: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        // A panicking worker cannot leave the queue logically inconsistent,
        // so recover from poisoning instead of propagating the panic.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pop(&self) -> Option<DirJob> {
        let mut state = self.lock();
        loop {
            if state.done {
                return None;
            }
            if let Some(job) = state.stack.pop() {
                state.active += 1;
                return Some(job);
            }
            if state.active == 0 {
                state.done = true;
                self.ready.notify_all();
                return None;
            }
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn push(&self, jobs: &mut Vec<DirJob>) {
        if jobs.is_empty() {
            return;
        }
        let count = jobs.len();
        self.lock().stack.append(jobs);
        if count == 1 {
            self.ready.notify_one();
        } else {
            self.ready.notify_all();
        }
    }

    fn finish_one(&self) {
        let mut state = self.lock();
        state.active -= 1;
        if state.active == 0 && state.stack.is_empty() {
            state.done = true;
            self.ready.notify_all();
        }
    }

    fn abort(&self) {
        self.lock().done = true;
        self.ready.notify_all();
    }
}
