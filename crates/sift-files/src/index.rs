use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sift_search::{Query, SearchError, SearchIndex, SearchResult};

/// The currently open search index, if any.
///
/// [`SearchIndex`] holds a SQLite connection, which may move between
/// threads but not be shared, so access goes through a mutex. Searches take
/// well under a millisecond, so contention is not a concern. Swapping in a
/// rebuilt index is safe while a search runs on the old one: snapshots are
/// replaced by renaming a new file into place, so the old mapping stays
/// valid until it is dropped.
pub struct FileIndex {
    dir: PathBuf,
    current: Mutex<Option<SearchIndex>>,
}

impl FileIndex {
    /// Opens the index in `dir` if there is one. A missing index is not an
    /// error: searches return nothing until the service creates one.
    pub fn open(dir: &Path) -> Self {
        let current = match SearchIndex::open(dir) {
            Ok(index) => Some(index),
            Err(SearchError::NoIndex(_)) => None,
            Err(error) => {
                tracing::warn!(%error, "cannot open the file index");
                None
            }
        };
        Self {
            dir: dir.to_path_buf(),
            current: Mutex::new(current),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Reopens the index, picking up a new snapshot.
    pub fn reload(&self) -> Result<(), SearchError> {
        let fresh = SearchIndex::open(&self.dir)?;
        *self.lock() = Some(fresh);
        Ok(())
    }

    /// Reopens the index if another process changed it (for example
    /// `sift index` in a terminal). Returns whether it reloaded.
    pub fn reload_if_stale(&self) -> bool {
        let stale = match &*self.lock() {
            Some(index) => index.is_stale(),
            None => self.dir.join(sift_search::DB_FILE).exists(),
        };
        stale && self.reload().is_ok()
    }

    /// Number of indexed names.
    pub fn len(&self) -> usize {
        self.lock().as_ref().map_or(0, |i| i.snapshot().len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ranked results for `text`, resolved to paths.
    pub fn search(&self, text: &str, limit: usize) -> Vec<SearchResult> {
        let guard = self.lock();
        let Some(index) = guard.as_ref() else {
            return Vec::new();
        };
        let outcome = index.search(&Query::new(text).limit(limit));
        match index.resolve(&outcome.hits) {
            Ok(results) => results,
            Err(error) => {
                tracing::warn!(%error, "cannot resolve file results");
                Vec::new()
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<SearchIndex>> {
        // A panic during a search cannot leave the index inconsistent.
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }
}
