//! Filesystem discovery: walks a directory tree and writes it to the store.
//!
//! A small pool of worker threads reads directories in parallel (directory
//! listing and `lstat` dominate scan time, and APFS on SSD serves them well
//! concurrently). Workers send batches of records over a bounded channel to
//! the calling thread, which is the only SQLite writer. Peak memory is
//! therefore bounded by the channel capacity plus the stack of directories
//! still waiting to be read.
//!
//! Entry ids are allocated by the workers from a shared counter at the
//! moment an entry is discovered. A directory is always discovered before
//! it is read, so every child gets a larger id than its parent; the search
//! snapshot builder relies on that ordering.
//!
//! Symbolic links are recorded but never followed, which rules out symlink
//! cycles. Directories on other devices (mount points) and macOS package
//! bundles such as `.app` are recorded without descending into them.

mod walk;

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sift_store::{Store, StoreError};

pub use walk::ScanError;

/// Directory extensions treated as opaque packages unless
/// [`IndexOptions::descend_packages`] is set. This mirrors how Finder and
/// Spotlight present bundles; the authoritative check (Launch Services'
/// package flag) needs AppKit and is deferred to the platform crate.
pub const PACKAGE_EXTENSIONS: &[&str] = &[
    "app",
    "appex",
    "bundle",
    "framework",
    "kext",
    "musiclibrary",
    "photoslibrary",
    "plugin",
    "prefpane",
    "xpc",
];

#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// Number of directory-reading threads.
    pub threads: usize,
    /// Do not descend into directories on a different device than the root.
    pub one_file_system: bool,
    /// Descend into package bundles such as `Foo.app`.
    pub descend_packages: bool,
    /// Absolute paths that are skipped entirely (not recorded, not read).
    pub exclude_paths: Vec<PathBuf>,
    /// File or directory names that are skipped wherever they occur.
    pub exclude_names: Vec<OsString>,
    /// How many individual errors to keep for reporting.
    pub max_error_samples: usize,
}

impl Default for IndexOptions {
    fn default() -> Self {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 8);
        Self {
            threads,
            one_file_system: true,
            descend_packages: false,
            exclude_paths: Vec::new(),
            exclude_names: Vec::new(),
            max_error_samples: 20,
        }
    }
}

impl IndexOptions {
    /// Locations that hold private application data and are excluded unless
    /// the user explicitly indexes them as a root: `~/Library` (mail,
    /// messages, browser profiles, keychains) and `~/.Trash`.
    pub fn default_sensitive_excludes() -> Vec<PathBuf> {
        match std::env::home_dir() {
            Some(home) => vec![home.join("Library"), home.join(".Trash")],
            None => Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("cannot access {path}: {source}")]
    Root {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0} is not a directory")]
    NotADirectory(PathBuf),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Outcome of indexing one root.
#[derive(Debug, Clone)]
pub struct IndexReport {
    pub root: PathBuf,
    /// Entries written, including the root directory itself.
    pub entries: u64,
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// Directories that could not be read and entries that could not be
    /// inspected (permission denied, vanished during the scan, ...).
    pub errors: u64,
    pub error_samples: Vec<ScanError>,
    /// Time spent walking and writing, excluding the final commit.
    pub scan_time: Duration,
    /// Total time including the commit and WAL checkpoint.
    pub total_time: Duration,
}

/// Indexes `root`, replacing any previous index of it.
///
/// `progress` is called periodically with the number of entries written so
/// far. The previous contents of the root stay visible to readers until the
/// new scan commits.
pub fn index_root(
    store: &mut Store,
    root: &Path,
    options: &IndexOptions,
    mut progress: impl FnMut(u64),
) -> Result<IndexReport, IndexError> {
    let started = Instant::now();
    let root = root.canonicalize().map_err(|source| IndexError::Root {
        path: root.to_path_buf(),
        source,
    })?;
    let meta = std::fs::metadata(&root).map_err(|source| IndexError::Root {
        path: root.clone(),
        source,
    })?;
    if !meta.is_dir() {
        return Err(IndexError::NotADirectory(root));
    }

    // A root the user names explicitly is indexed even if it is itself one
    // of the default exclusions; only locations inside it are skipped.
    let exclude_paths: HashSet<PathBuf> = options
        .exclude_paths
        .iter()
        .filter(|p| p.starts_with(&root) && **p != root)
        .cloned()
        .collect();

    let root_id = store.next_entry_id()?;
    let mut writer = store.replace_root(&root, root_id)?;
    let config = walk::WalkConfig {
        root: &root,
        root_id,
        root_dev: std::os::unix::fs::MetadataExt::dev(&meta),
        options,
        exclude_paths: &exclude_paths,
    };
    let stats = walk::walk(&config, |entry| writer.insert(entry), &mut progress)?;
    let scan_time = started.elapsed();

    let entries = stats.files + stats.dirs + stats.symlinks + stats.other + 1;
    writer.commit(sift_store::ScanSummary {
        entry_count: entries,
        error_count: stats.errors,
        scan_millis: scan_time.as_millis() as u64,
    })?;
    store.checkpoint();

    Ok(IndexReport {
        root,
        entries,
        files: stats.files,
        dirs: stats.dirs + 1,
        symlinks: stats.symlinks,
        errors: stats.errors,
        error_samples: stats.error_samples,
        scan_time,
        total_time: started.elapsed(),
    })
}
