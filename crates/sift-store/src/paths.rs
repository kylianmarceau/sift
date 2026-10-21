use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension};

use crate::{EntryId, Result, StoreError};

/// Upper bound on cached directory paths. Resolving a page of results
/// touches at most a few hundred directories; the cap only guards against a
/// resolver being kept alive across many queries.
const MAX_CACHED_DIRS: usize = 4096;

/// Rebuilds absolute paths from parent links.
///
/// Each lookup is a rowid point query, and directory paths are cached, so
/// resolving a result page costs roughly one query per distinct directory.
pub struct PathResolver<'conn> {
    conn: &'conn Connection,
    dirs: HashMap<EntryId, PathBuf>,
}

impl<'conn> PathResolver<'conn> {
    pub(crate) fn new(conn: &'conn Connection) -> Self {
        Self {
            conn,
            dirs: HashMap::new(),
        }
    }

    /// Returns the absolute path of entry `id`.
    pub fn resolve(&mut self, id: EntryId) -> Result<PathBuf> {
        let (parent, name) = self.lookup(id)?;
        match parent {
            // Root entries store their absolute path as the name.
            None => Ok(PathBuf::from(OsStr::from_bytes(&name))),
            Some(parent) => {
                let mut path = self.resolve_dir(parent)?;
                path.push(OsStr::from_bytes(&name));
                Ok(path)
            }
        }
    }

    fn resolve_dir(&mut self, id: EntryId) -> Result<PathBuf> {
        if let Some(path) = self.dirs.get(&id) {
            return Ok(path.clone());
        }

        // Walk up until a cached ancestor or the root, then build downwards.
        let mut chain = Vec::new();
        let mut current = id;
        let mut base = loop {
            if let Some(path) = self.dirs.get(&current) {
                break path.clone();
            }
            let (parent, name) = self.lookup(current)?;
            match parent {
                None => break PathBuf::from(OsStr::from_bytes(&name)),
                Some(parent) => {
                    chain.push((current, name));
                    current = parent;
                }
            }
        };
        if chain.is_empty() {
            // `current` was the root (or cached) itself.
            self.remember(current, &base);
        }
        while let Some((dir_id, name)) = chain.pop() {
            base.push(OsStr::from_bytes(&name));
            self.remember(dir_id, &base);
        }
        Ok(base)
    }

    fn remember(&mut self, id: EntryId, path: &std::path::Path) {
        if self.dirs.len() >= MAX_CACHED_DIRS {
            self.dirs.clear();
        }
        self.dirs.insert(id, path.to_path_buf());
    }

    fn lookup(&self, id: EntryId) -> Result<(Option<EntryId>, Vec<u8>)> {
        self.conn
            .prepare_cached("SELECT parent_id, name FROM entries WHERE id = ?1")?
            .query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?
            .ok_or(StoreError::MissingEntry(id))
    }
}
