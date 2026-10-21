//! Result icons, rendered off the main thread and cached.
//!
//! Applications and packages get their own icon. Documents share one icon
//! per file extension and folders share one folder icon, so a result list
//! of hundreds of files costs a handful of renders (each about 1–15 ms in
//! AppKit, measured at 64 px).
//!
//! The cache is bounded: application icons are kept for as long as the app
//! is in the catalog, and at most [`IconCache::LIMIT`] other icons (bundles
//! found by file search, extensions) are kept, least recently used first
//! out. Evicted icons are handed back so the caller can free their textures.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sift_core::{Action, ResultItem, ResultKind};

/// Bundle extensions that carry their own icon.
const OWN_ICON_EXTENSIONS: &[&str] = &["app", "prefpane", "bundle", "workflow", "appex"];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IconKey {
    /// This exact path (applications, packages).
    Path(PathBuf),
    /// Any file with this lowercase extension ("" for none).
    Extension(String),
    Folder,
}

/// The cache key for `item`'s icon, and a path to render it from.
pub fn icon_for(item: &ResultItem) -> Option<(IconKey, PathBuf)> {
    let path = item.action.path()?;
    let key = match (&item.action, item.kind) {
        (Action::OpenApplication(_), _) => IconKey::Path(path.to_path_buf()),
        (_, ResultKind::Folder) if has_own_icon(path) => IconKey::Path(path.to_path_buf()),
        (_, ResultKind::Folder) => IconKey::Folder,
        _ => IconKey::Extension(
            path.extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default(),
        ),
    };
    Some((key, path.to_path_buf()))
}

/// Rendered icons by key. `None` records that an icon could not be
/// rendered, so it is not tried again.
pub struct IconCache<V> {
    entries: HashMap<IconKey, Entry<V>>,
    limit: usize,
    clock: u64,
}

struct Entry<V> {
    value: Option<V>,
    used: u64,
    kept: bool,
}

impl<V> IconCache<V> {
    /// Icons kept besides the pinned ones. At 64 px one icon is 16 KB of
    /// pixels, so the bound is about 4 MB.
    pub const LIMIT: usize = 256;

    pub fn new(limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            limit,
            clock: 0,
        }
    }

    /// The icon for `key` if it was rendered (`Some(None)` if rendering
    /// failed), marking it as recently used.
    pub fn get(&mut self, key: &IconKey) -> Option<Option<&V>> {
        self.clock += 1;
        let entry = self.entries.get_mut(key)?;
        entry.used = self.clock;
        Some(entry.value.as_ref())
    }

    /// Stores an icon. `kept` icons (applications) are never evicted until
    /// [`IconCache::release_kept`]. Returns the icons that were replaced or
    /// evicted.
    pub fn insert(&mut self, key: IconKey, value: Option<V>, kept: bool) -> Vec<V> {
        self.clock += 1;
        let entry = Entry {
            value,
            used: self.clock,
            kept,
        };
        let mut dropped: Vec<V> = self
            .entries
            .insert(key, entry)
            .and_then(|old| old.value)
            .into_iter()
            .collect();
        let evictable = self.entries.values().filter(|e| !e.kept).count();
        if evictable > self.limit {
            let mut by_age: Vec<(u64, IconKey)> = self
                .entries
                .iter()
                .filter(|(_, e)| !e.kept)
                .map(|(k, e)| (e.used, k.clone()))
                .collect();
            by_age.sort_unstable_by_key(|(used, _)| *used);
            for (_, key) in by_age.into_iter().take(evictable - self.limit) {
                if let Some(value) = self.entries.remove(&key).and_then(|e| e.value) {
                    dropped.push(value);
                }
            }
        }
        dropped
    }

    /// Makes every kept icon evictable again (before the catalog is
    /// reloaded, so icons of removed apps can age out). Returns the icons
    /// evicted to get back under the limit.
    pub fn release_kept(&mut self) -> Vec<V> {
        for entry in self.entries.values_mut() {
            entry.kept = false;
        }
        let mut dropped = Vec::new();
        let mut by_age: Vec<(u64, IconKey)> = self
            .entries
            .iter()
            .map(|(k, e)| (e.used, k.clone()))
            .collect();
        by_age.sort_unstable_by_key(|(used, _)| *used);
        let excess = by_age.len().saturating_sub(self.limit);
        for (_, key) in by_age.into_iter().take(excess) {
            if let Some(value) = self.entries.remove(&key).and_then(|e| e.value) {
                dropped.push(value);
            }
        }
        dropped
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

fn has_own_icon(path: &Path) -> bool {
    path.extension().is_some_and(|e| {
        OWN_ICON_EXTENSIONS
            .iter()
            .any(|own| e.eq_ignore_ascii_case(own))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: ResultKind, action: Action) -> ResultItem {
        ResultItem {
            id: "x".into(),
            kind,
            title: "x".into(),
            subtitle: None,
            score: 0,
            action,
        }
    }

    #[test]
    fn documents_share_an_icon_per_extension() {
        let a = item(ResultKind::File, Action::OpenPath("/d/a.PDF".into()));
        let b = item(ResultKind::File, Action::OpenPath("/e/b.pdf".into()));
        assert_eq!(icon_for(&a).unwrap().0, icon_for(&b).unwrap().0);
        let none = item(ResultKind::File, Action::OpenPath("/d/Makefile".into()));
        assert_eq!(
            icon_for(&none).unwrap().0,
            IconKey::Extension(String::new())
        );
    }

    #[test]
    fn apps_and_packages_keep_their_own_icon() {
        let app = item(
            ResultKind::Application,
            Action::OpenApplication("/Applications/Safari.app".into()),
        );
        assert!(matches!(icon_for(&app).unwrap().0, IconKey::Path(_)));
        let pkg = item(ResultKind::Folder, Action::OpenPath("/x/Tool.app".into()));
        assert!(matches!(icon_for(&pkg).unwrap().0, IconKey::Path(_)));
        let dir = item(ResultKind::Folder, Action::OpenPath("/x/photos".into()));
        assert_eq!(icon_for(&dir).unwrap().0, IconKey::Folder);
        assert!(icon_for(&item(ResultKind::Command, Action::Run("quit"))).is_none());
    }

    fn ext(name: &str) -> IconKey {
        IconKey::Extension(name.into())
    }

    #[test]
    fn the_cache_evicts_the_least_recently_used_icons() {
        let mut cache = IconCache::new(2);
        assert!(cache.insert(ext("a"), Some(1), false).is_empty());
        assert!(cache.insert(ext("b"), Some(2), false).is_empty());
        // "a" is used again, so "b" is the oldest.
        assert_eq!(cache.get(&ext("a")), Some(Some(&1)));
        assert_eq!(cache.insert(ext("c"), Some(3), false), [2]);
        assert_eq!(cache.get(&ext("b")), None);
        assert_eq!(cache.len(), 2);
        // A failed render is remembered and takes no texture to free.
        assert!(cache.insert(ext("d"), None, false).contains(&1));
        assert_eq!(cache.get(&ext("d")), Some(None));
    }

    #[test]
    fn application_icons_stay_until_released() {
        let mut cache = IconCache::new(1);
        let app = IconKey::Path("/Applications/Safari.app".into());
        cache.insert(app.clone(), Some(10), true);
        cache.insert(ext("a"), Some(1), false);
        assert_eq!(cache.insert(ext("b"), Some(2), false), [1]);
        assert_eq!(cache.get(&app), Some(Some(&10)));
        // Replacing an icon hands back the old one.
        assert_eq!(cache.insert(app.clone(), Some(11), true), [10]);
        // After a release only the most recent icon fits.
        assert_eq!(cache.release_kept(), [2]);
        assert_eq!(cache.get(&app), Some(Some(&11)));
    }
}
