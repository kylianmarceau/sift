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

mod compare;
mod update;
mod walk;

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sift_store::{EventMark, Store, StoreError};

pub use compare::{Difference, Listed, compare_with_disk, difference, listing};
pub use update::{Change, UpdateReport, apply_changes, is_watched};
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

/// Directory names whose contents are dependency, tool or cache data rather
/// than the user's own files. With [`IndexOptions::skip_dependency_dirs`]
/// (the default) they are recorded, so `node_modules` itself is still
/// findable, but not descended into. Measured on a real `~/Documents`, these
/// trees were about 60% of all entries and dominated search results.
pub const DEPENDENCY_DIR_NAMES: &[&str] = &[
    // Version control
    ".git",
    ".hg",
    ".svn",
    // JavaScript
    "node_modules",
    "bower_components",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".parcel-cache",
    ".turbo",
    // Python
    ".venv",
    "venv",
    "site-packages",
    "dist-packages",
    "__pycache__",
    ".tox",
    ".nox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    // Apple, JVM and infrastructure tooling
    "Pods",
    "DerivedData",
    ".build",
    ".gradle",
    ".terraform",
];

/// Directories containing this file declare themselves caches under the
/// Cache Directory Tagging Specification (used by Cargo's `target/`, uv,
/// pip, Bazel and others), so they are skipped like dependency folders.
pub const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexOptions {
    /// Number of directory-reading threads.
    pub threads: usize,
    /// Do not descend into directories on a different device than the root.
    pub one_file_system: bool,
    /// Descend into package bundles such as `Foo.app`.
    pub descend_packages: bool,
    /// Record but do not descend into [`DEPENDENCY_DIR_NAMES`] and tagged
    /// cache directories.
    pub skip_dependency_dirs: bool,
    /// Skip [`IndexOptions::default_sensitive_excludes`] when they fall
    /// inside a root.
    pub skip_sensitive: bool,
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
            skip_dependency_dirs: true,
            skip_sensitive: true,
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

    /// Serialises the options that define *what* gets indexed, so a later
    /// re-index of the same root can reuse them. Thread count and error
    /// sampling are run-time choices and are not saved.
    ///
    /// Format: NUL-separated `key=value` records. Names and paths cannot
    /// contain NUL, so no escaping is needed, and raw bytes round-trip.
    pub fn encode_saved(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut record = |key: &str, value: &[u8]| {
            out.extend_from_slice(key.as_bytes());
            out.push(b'=');
            out.extend_from_slice(value);
            out.push(0);
        };
        record("v", b"1");
        let flag = |b: bool| if b { b"1" as &[u8] } else { b"0" };
        record("one_file_system", flag(self.one_file_system));
        record("descend_packages", flag(self.descend_packages));
        record("skip_dependency_dirs", flag(self.skip_dependency_dirs));
        record("skip_sensitive", flag(self.skip_sensitive));
        for path in &self.exclude_paths {
            record("exclude_path", path.as_os_str().as_bytes());
        }
        for name in &self.exclude_names {
            record("exclude_name", name.as_bytes());
        }
        out
    }

    /// Restores options saved by [`IndexOptions::encode_saved`] on top of
    /// the defaults. Unknown keys are ignored so newer saved options remain
    /// readable; malformed data yields `None`.
    pub fn decode_saved(bytes: &[u8]) -> Option<Self> {
        let mut options = Self::default();
        for record in bytes.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            let eq = record.iter().position(|&b| b == b'=')?;
            let (key, value) = (&record[..eq], &record[eq + 1..]);
            let flag = value == b"1";
            match key {
                b"one_file_system" => options.one_file_system = flag,
                b"descend_packages" => options.descend_packages = flag,
                b"skip_dependency_dirs" => options.skip_dependency_dirs = flag,
                b"skip_sensitive" => options.skip_sensitive = flag,
                b"exclude_path" => options
                    .exclude_paths
                    .push(PathBuf::from(OsStr::from_bytes(value))),
                b"exclude_name" => options.exclude_names.push(OsStr::from_bytes(value).into()),
                _ => {}
            }
        }
        Some(options)
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
    /// The root is missing (deleted, renamed or on an unmounted drive).
    /// Its index is left untouched.
    #[error("{0} is not available")]
    RootUnavailable(PathBuf),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Whether `path` (canonical) is too broad to index without explicit
/// confirmation: the filesystem root or the home folder. Indexing either
/// would pull in system files or private application data.
pub fn is_broad_root(path: &Path) -> bool {
    path == Path::new("/") || std::env::home_dir().as_deref() == Some(path)
}

/// The excluded paths that apply inside `root`: the user's exclusions plus,
/// with [`IndexOptions::skip_sensitive`], the default sensitive locations.
/// A root the user names explicitly is indexed even if it is itself one of
/// the default exclusions; only locations inside it are skipped.
pub(crate) fn effective_excludes(root: &Path, options: &IndexOptions) -> HashSet<PathBuf> {
    let sensitive = if options.skip_sensitive {
        IndexOptions::default_sensitive_excludes()
    } else {
        Vec::new()
    };
    options
        .exclude_paths
        .iter()
        .chain(&sensitive)
        .filter(|p| p.starts_with(root) && *p != root)
        .cloned()
        .collect()
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
    progress: impl FnMut(u64),
) -> Result<IndexReport, IndexError> {
    index_root_at(store, root, options, None, progress)
}

/// Like [`index_root`], and records `event` with the new scan.
///
/// A watcher passes the FSEvents id it read *before* the scan started.
/// Changes made during the scan are then replayed from that id and
/// re-applied, which is harmless because updates compare against the
/// filesystem rather than trusting the event.
pub fn index_root_at(
    store: &mut Store,
    root: &Path,
    options: &IndexOptions,
    event: Option<&EventMark>,
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

    let exclude_paths = effective_excludes(&root, options);

    let root_id = store.next_entry_id()?;
    let mut writer = store.replace_root(&root, root_id, &options.encode_saved())?;
    if let Some(event) = event {
        writer.set_event(event)?;
    }
    let config = walk::WalkConfig {
        start: &root,
        start_id: root_id,
        next_id: root_id + 1,
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
    store.compact_if_fragmented()?;

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
