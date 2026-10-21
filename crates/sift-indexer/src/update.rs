//! Incremental updates: bring part of an indexed root back in line with the
//! filesystem after it changed.
//!
//! A [`Change`] only says *where* to look ("something in this directory
//! changed"), which is what macOS FSEvents reports. The update lists that
//! directory, compares it with the stored children and applies the
//! difference:
//!
//! - names gone from disk are deleted with their subtrees;
//! - new names are inserted, and new directories are walked completely
//!   (a folder moved into the tree produces no events for its contents);
//! - a name whose kind changed (a file replaced by a folder) is replaced;
//! - files whose size or modification time changed get new metadata.
//!
//! Because the filesystem, not the event, is the authority, applying the
//! same change twice, out of order, or after the fact is harmless. That is
//! what makes coalesced, replayed and dropped events safe to handle.

use std::collections::{HashMap, HashSet};
use std::fs::{self, FileType, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use sift_store::{
    EntryId, EntryKind, EventMark, NewEntry, RootInfo, Store, UpdateSummary, Updater,
};

use crate::walk::{self, WalkConfig, descends, kind_of};
use crate::{IndexError, IndexOptions, effective_excludes};

/// A place in the filesystem that may have changed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Change {
    /// Absolute path, normally a directory.
    pub path: PathBuf,
    /// Re-examine everything below `path`, not just its direct children.
    /// Used when the event source lost detail (FSEvents `MustScanSubDirs`
    /// or dropped events).
    pub recursive: bool,
}

impl Change {
    pub fn dir(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            recursive: false,
        }
    }

    pub fn tree(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            recursive: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UpdateReport {
    pub summary: UpdateSummary,
    /// Directories listed during the update.
    pub dirs_listed: u64,
    /// Changes skipped because they are outside the root or inside an
    /// excluded or skipped (dependency, package) directory.
    pub ignored: u64,
    /// Directories or entries that could not be read. Their previous index
    /// entries are kept.
    pub errors: u64,
    pub elapsed: Duration,
}

/// Applies `changes` to the index of `root` in one transaction, recording
/// `event` (if given) with it.
///
/// Returns [`IndexError::RootUnavailable`] without changing anything if the
/// root directory itself is missing, so an unplugged drive or a renamed root
/// does not wipe its index.
pub fn apply_changes(
    store: &mut Store,
    root: &RootInfo,
    options: &IndexOptions,
    changes: &[Change],
    event: Option<&EventMark>,
) -> Result<UpdateReport, IndexError> {
    let started = Instant::now();
    let root_meta = match fs::metadata(&root.path) {
        Ok(meta) if meta.is_dir() => meta,
        _ => return Err(IndexError::RootUnavailable(root.path.clone())),
    };
    let ctx = Context {
        root: &root.path,
        root_entry: root.entry_id,
        root_dev: root_meta.dev(),
        options,
        excludes: effective_excludes(&root.path, options),
    };

    let mut report = UpdateReport::default();
    let mut update = store.begin_update(root.id)?;
    let mut visited = HashSet::new();
    for change in normalize(changes) {
        if !ctx.is_watched(&change.path) {
            report.ignored += 1;
            continue;
        }
        let (dir_id, dir_path, exact) = ctx.resolve(&update, &change.path)?;
        // A path that is not indexed (yet) is handled by re-listing its
        // nearest indexed ancestor, which inserts it with its contents.
        let recursive = change.recursive && exact;
        ctx.rescan(
            &mut update,
            &dir_path,
            dir_id,
            recursive,
            &mut visited,
            &mut report,
        )?;
    }
    report.summary = update.commit(event)?;
    report.elapsed = started.elapsed();
    Ok(report)
}

/// Whether changes at `path` can affect the index of `root`: the path is
/// inside the root and not inside an excluded, dependency or package
/// directory (whose contents are not indexed).
pub fn is_watched(root: &Path, options: &IndexOptions, path: &Path) -> bool {
    let Ok(root_meta) = fs::metadata(root) else {
        return false;
    };
    Context {
        root,
        root_entry: 0,
        root_dev: root_meta.dev(),
        options,
        excludes: effective_excludes(root, options),
    }
    .is_watched(path)
}

/// Sorts changes parent-first and drops those already covered by a
/// recursive change of an ancestor, or duplicated.
fn normalize(changes: &[Change]) -> Vec<Change> {
    let mut sorted: Vec<Change> = changes.to_vec();
    sorted.sort_by(|a, b| {
        a.path
            .components()
            .count()
            .cmp(&b.path.components().count())
            .then(b.recursive.cmp(&a.recursive))
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut kept: Vec<Change> = Vec::with_capacity(sorted.len());
    for change in sorted {
        let covered = kept.iter().any(|k| {
            (k.recursive && change.path.starts_with(&k.path))
                || (k.path == change.path && (k.recursive || !change.recursive))
        });
        if !covered {
            kept.push(change);
        }
    }
    kept
}

struct Context<'a> {
    root: &'a Path,
    root_entry: EntryId,
    root_dev: u64,
    options: &'a IndexOptions,
    excludes: HashSet<PathBuf>,
}

impl Context<'_> {
    fn is_watched(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(self.root) else {
            return false;
        };
        let mut current = self.root.to_path_buf();
        for component in rel.components() {
            let Component::Normal(name) = component else {
                return false;
            };
            current.push(name);
            if self.options.exclude_names.iter().any(|n| n == name)
                || self.excludes.contains(&current)
            {
                return false;
            }
            // Directories that are recorded but not descended: nothing
            // inside them is indexed. A path that no longer exists is
            // judged by name only.
            let metadata = fs::symlink_metadata(&current).ok();
            if metadata.as_ref().is_some_and(|m| m.is_dir())
                && !descends(
                    self.options,
                    self.root_dev,
                    name.as_bytes(),
                    &current,
                    metadata.as_ref(),
                )
            {
                return false;
            }
        }
        true
    }

    /// Finds the deepest indexed directory on the way to `path`. Returns its
    /// id and path, and whether it is `path` itself.
    fn resolve(
        &self,
        update: &Updater<'_>,
        path: &Path,
    ) -> Result<(EntryId, PathBuf, bool), IndexError> {
        let mut id = self.root_entry;
        let mut current = self.root.to_path_buf();
        let rel = path.strip_prefix(self.root).unwrap_or(Path::new(""));
        for component in rel.components() {
            let Component::Normal(name) = component else {
                return Ok((id, current, false));
            };
            match update.child(id, name.as_bytes())? {
                Some(child) if child.kind == EntryKind::Dir => {
                    id = child.id;
                    current.push(name);
                }
                // Missing, or not a directory: re-list the parent.
                _ => return Ok((id, current, false)),
            }
        }
        Ok((id, current, true))
    }

    /// Re-lists directory `path` (entry `id`) and applies the differences.
    fn rescan(
        &self,
        update: &mut Updater<'_>,
        path: &Path,
        id: EntryId,
        recursive: bool,
        visited: &mut HashSet<(EntryId, bool)>,
        report: &mut UpdateReport,
    ) -> Result<(), IndexError> {
        // A recursive pass covers a shallow one, but not the reverse.
        if visited.contains(&(id, true)) || !visited.insert((id, recursive)) {
            return Ok(());
        }
        let listing = match fs::read_dir(path) {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && id != self.root_entry => {
                // The directory itself is gone.
                update.remove_subtree(id)?;
                return Ok(());
            }
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "cannot list directory");
                report.errors += 1;
                return Ok(());
            }
        };
        report.dirs_listed += 1;

        let mut on_disk: HashMap<Vec<u8>, (FileType, Option<Metadata>)> = HashMap::new();
        for entry in listing {
            let Ok(entry) = entry else {
                report.errors += 1;
                continue;
            };
            let name = entry.file_name();
            if self.options.exclude_names.contains(&name) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                report.errors += 1;
                continue;
            };
            if self.excludes.contains(&path.join(&name)) {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(m) => Some(m),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    report.errors += 1;
                    None
                }
            };
            on_disk.insert(name.as_bytes().to_vec(), (file_type, metadata));
        }

        let stored = update.children(id)?;
        let mut stored_names = HashSet::with_capacity(stored.len());
        for child in &stored {
            stored_names.insert(child.name.as_slice());
            let Some((file_type, metadata)) = on_disk.get(child.name.as_slice()) else {
                update.remove_subtree(child.id)?;
                continue;
            };
            let kind = kind_of(*file_type);
            let child_path = path.join(std::ffi::OsStr::from_bytes(&child.name));
            if kind != child.kind {
                update.remove_subtree(child.id)?;
                self.insert_new(
                    update,
                    id,
                    &child_path,
                    &child.name,
                    *file_type,
                    metadata.as_ref(),
                    report,
                )?;
                continue;
            }
            let (size, mtime) = entry_meta(kind, metadata.as_ref());
            if (size, mtime) != (child.size, child.mtime) && metadata.is_some() {
                update.update_meta(child.id, size, mtime)?;
            }
            if recursive
                && kind == EntryKind::Dir
                && descends(
                    self.options,
                    self.root_dev,
                    &child.name,
                    &child_path,
                    metadata.as_ref(),
                )
            {
                self.rescan(update, &child_path, child.id, true, visited, report)?;
            }
        }

        for (name, (file_type, metadata)) in &on_disk {
            if stored_names.contains(name.as_slice()) {
                continue;
            }
            let child_path = path.join(std::ffi::OsStr::from_bytes(name));
            self.insert_new(
                update,
                id,
                &child_path,
                name,
                *file_type,
                metadata.as_ref(),
                report,
            )?;
        }
        Ok(())
    }

    /// Inserts a newly found entry, and everything below it if it is a
    /// directory that is descended.
    #[allow(clippy::too_many_arguments)]
    fn insert_new(
        &self,
        update: &mut Updater<'_>,
        parent: EntryId,
        path: &Path,
        name: &[u8],
        file_type: FileType,
        metadata: Option<&Metadata>,
        report: &mut UpdateReport,
    ) -> Result<(), IndexError> {
        let kind = kind_of(file_type);
        let (size, mtime) = entry_meta(kind, metadata);
        let id = update.alloc_id();
        update.insert(&NewEntry {
            id,
            parent_id: parent,
            kind,
            name,
            size,
            mtime,
        })?;
        if kind == EntryKind::Dir && descends(self.options, self.root_dev, name, path, metadata) {
            let config = WalkConfig {
                start: path,
                start_id: id,
                next_id: update.next_id(),
                root_dev: self.root_dev,
                options: self.options,
                exclude_paths: &self.excludes,
            };
            let stats = walk::walk(&config, |entry| update.insert(entry), &mut |_| {})?;
            report.errors += stats.errors;
        }
        Ok(())
    }
}

/// Size and modification time as the full scan records them: directories
/// have no size.
fn entry_meta(kind: EntryKind, metadata: Option<&Metadata>) -> (Option<u64>, Option<i64>) {
    (
        metadata.filter(|_| kind != EntryKind::Dir).map(|m| m.len()),
        metadata.map(|m| m.mtime()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_drops_covered_and_duplicate_changes() {
        let changes = [
            Change::dir("/r/a/b"),
            Change::tree("/r/a"),
            Change::dir("/r/a"),
            Change::dir("/r/c"),
            Change::dir("/r/c"),
            Change::tree("/r/c"),
            Change::dir("/r/d/e"),
        ];
        assert_eq!(
            normalize(&changes),
            [
                Change::tree("/r/a"),
                Change::tree("/r/c"),
                Change::dir("/r/d/e")
            ]
        );
    }
}
