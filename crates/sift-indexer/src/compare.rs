//! Compares an indexed root with the filesystem.
//!
//! The root is scanned again into a scratch database with the same
//! options, and both listings (relative path → kind, size, modification
//! time) are compared. Used by `sift verify` and by tests that check live
//! updates against a fresh scan.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use sift_store::{EntryKind, RootInfo, Store, StoreError};

use crate::{IndexError, IndexOptions, index_root};

/// What is recorded about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listed {
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
}

/// Every entry of `root`, by path relative to the root (the root itself is
/// the empty path), as raw bytes.
pub fn listing(store: &Store, root: &RootInfo) -> Result<BTreeMap<Vec<u8>, Listed>, StoreError> {
    let mut folders: HashMap<i64, Vec<u8>> = HashMap::new();
    let mut out = BTreeMap::new();
    store.for_each_root_entry(root.id, |entry, size, mtime| {
        let rel = match entry.parent_id.and_then(|p| folders.get(&p)) {
            Some(parent) if parent.is_empty() => entry.name.to_vec(),
            Some(parent) => [parent.as_slice(), b"/", entry.name].concat(),
            // The root (or an orphan, which `Store::verify` reports).
            None if entry.parent_id.is_none() => Vec::new(),
            None => [b"?orphan/".as_slice(), entry.name].concat(),
        };
        if entry.kind == EntryKind::Dir {
            folders.insert(entry.id, rel.clone());
        }
        out.insert(
            rel,
            Listed {
                kind: entry.kind,
                size,
                mtime,
            },
        );
    })?;
    Ok(out)
}

/// How an index differs from the filesystem.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Difference {
    /// On disk but not in the index.
    pub missing: Vec<Vec<u8>>,
    /// In the index but no longer on disk.
    pub extra: Vec<Vec<u8>>,
    /// In both, with a different kind, size or modification time.
    pub changed: Vec<Vec<u8>>,
    /// Paths compared on disk.
    pub on_disk: usize,
}

impl Difference {
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty() && self.changed.is_empty()
    }
}

/// Compares two listings. Folder sizes and modification times are ignored
/// (they change with every edit inside and say nothing about the index).
pub fn difference(
    indexed: &BTreeMap<Vec<u8>, Listed>,
    disk: &BTreeMap<Vec<u8>, Listed>,
) -> Difference {
    let mut diff = Difference {
        on_disk: disk.len(),
        ..Difference::default()
    };
    for (path, on_disk) in disk {
        match indexed.get(path) {
            None => diff.missing.push(path.clone()),
            Some(entry) if entry.kind != on_disk.kind => diff.changed.push(path.clone()),
            Some(entry)
                if on_disk.kind != EntryKind::Dir
                    && (entry.size != on_disk.size || entry.mtime != on_disk.mtime) =>
            {
                diff.changed.push(path.clone());
            }
            Some(_) => {}
        }
    }
    diff.extra = indexed
        .keys()
        .filter(|path| !disk.contains_key(*path))
        .cloned()
        .collect();
    diff
}

/// Scans `root` again into a scratch database at `scratch` (created, then
/// deleted) and compares it with the index in `store`.
pub fn compare_with_disk(
    store: &Store,
    root: &RootInfo,
    options: &IndexOptions,
    scratch: &Path,
) -> Result<Difference, IndexError> {
    let indexed = listing(store, root)?;
    let result = (|| {
        let mut fresh = Store::open(scratch)?;
        index_root(&mut fresh, &root.path, options, |_| {})?;
        let fresh_root = fresh
            .root(&root.path)?
            .ok_or_else(|| IndexError::RootUnavailable(root.path.clone()))?;
        Ok::<_, IndexError>(listing(&fresh, &fresh_root)?)
    })();
    for suffix in ["", "-wal", "-shm"] {
        let mut path = scratch.as_os_str().to_owned();
        path.push(suffix);
        let _ = std::fs::remove_file(path);
    }
    Ok(difference(&indexed, &result?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(kind: EntryKind, size: u64, mtime: i64) -> Listed {
        Listed {
            kind,
            size: Some(size),
            mtime: Some(mtime),
        }
    }

    #[test]
    fn reports_missing_extra_and_changed_paths() {
        let file = |size| listed(EntryKind::File, size, 1);
        let indexed = BTreeMap::from([
            (b"same".to_vec(), file(1)),
            (b"gone".to_vec(), file(1)),
            (b"grown".to_vec(), file(1)),
            (b"dir".to_vec(), listed(EntryKind::Dir, 64, 1)),
        ]);
        let disk = BTreeMap::from([
            (b"same".to_vec(), file(1)),
            (b"new".to_vec(), file(1)),
            (b"grown".to_vec(), file(2)),
            // A folder's own size and date are not compared.
            (b"dir".to_vec(), listed(EntryKind::Dir, 96, 2)),
        ]);
        let diff = difference(&indexed, &disk);
        assert_eq!(diff.missing, [b"new".to_vec()]);
        assert_eq!(diff.extra, [b"gone".to_vec()]);
        assert_eq!(diff.changed, [b"grown".to_vec()]);
        assert_eq!(diff.on_disk, 4);
        assert!(!diff.is_empty());
    }
}
