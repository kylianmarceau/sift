//! Installed applications: discovery and the application search provider.
//!
//! Discovery lists `.app` bundles in the standard locations once and keeps
//! them in memory; a few hundred names fit in tens of kilobytes, and
//! matching them takes microseconds, so there is no index or snapshot. The
//! launcher refreshes the catalog in the background, never per keystroke.
//!
//! Matching reuses the file index's ranking (`sift_search::NameMatcher`):
//! the same folding, tiers and fuzzy scorer, so `term` ranks Terminal above
//! iTerm for the same reasons a file search would.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sift_core::{Action, Provider, ResultItem, ResultKind};
use sift_search::{NameMatcher, fold};

/// An installed application bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// Display name: the bundle's file name without `.app`, as Finder
    /// shows it.
    pub name: Arc<str>,
    /// `name` folded for matching (see `sift_search::fold`).
    folded: Box<[u8]>,
    /// Path as found (which may be a symlink, like `/Applications/Safari.app`).
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    /// Folder the app was found in, for the result's secondary line.
    pub location: Arc<str>,
}

/// A place to look for applications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A folder whose `.app` bundles are listed, along with bundles one
    /// level down in ordinary subfolders (`/Applications/Utilities`,
    /// vendor folders such as `/Applications/Microsoft Office`).
    Folder(PathBuf),
    /// A single bundle outside the usual folders, such as Finder.
    Bundle(PathBuf),
}

impl Location {
    /// Standard locations in priority order: when two copies of the same
    /// app exist, the one found first wins.
    pub fn defaults() -> Vec<Location> {
        let mut locations = vec![Location::Folder("/Applications".into())];
        if let Some(home) = std::env::home_dir() {
            locations.push(Location::Folder(home.join("Applications")));
        }
        locations.extend([
            Location::Folder("/System/Applications".into()),
            Location::Folder("/System/Library/CoreServices/Applications".into()),
            Location::Bundle("/System/Library/CoreServices/Finder.app".into()),
        ]);
        locations
    }
}

/// The set of discovered applications.
#[derive(Debug, Clone, Default)]
pub struct AppCatalog {
    apps: Vec<App>,
}

impl AppCatalog {
    /// Scans `locations`. Missing or unreadable folders and malformed
    /// bundles are skipped (and logged); they never fail discovery.
    pub fn discover(locations: &[Location]) -> Self {
        let mut found = Discovery::default();
        for location in locations {
            match location {
                Location::Folder(dir) => found.scan_folder(dir, 1),
                Location::Bundle(path) => found.add(path),
            }
        }
        found.apps.sort_by(|a, b| a.name.cmp(&b.name));
        Self { apps: found.apps }
    }

    pub fn apps(&self) -> &[App] {
        &self.apps
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }
}

#[derive(Default)]
struct Discovery {
    apps: Vec<App>,
    /// Resolved bundle paths seen so far: a symlinked and a real copy of
    /// the same bundle are one app.
    real_paths: HashSet<PathBuf>,
    /// Bundle identifiers seen so far: two copies of one app (for example
    /// in /Applications and ~/Applications) are listed once.
    bundle_ids: HashSet<String>,
}

impl Discovery {
    fn scan_folder(&mut self, dir: &Path, descend: u32) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(dir = %dir.display(), %error, "cannot list applications");
                }
                return;
            }
        };
        // Sort for deterministic duplicate resolution.
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            // `is_dir` follows symlinks, which is intended here: system apps
            // such as Safari are symlinks into a read-only volume. Only one
            // level is followed, so link cycles cannot recurse.
            if !path.is_dir() {
                continue;
            }
            if is_app_bundle(&path) {
                self.add(&path);
            } else if descend > 0 && !is_hidden(&path) {
                self.scan_folder(&path, descend - 1);
            }
        }
    }

    fn add(&mut self, path: &Path) {
        let Some(name) = path.file_stem().map(OsStr::to_string_lossy) else {
            return;
        };
        let Ok(real) = path.canonicalize() else {
            return;
        };
        if !self.real_paths.insert(real.clone()) {
            return;
        }
        let bundle_id = read_bundle_id(&real);
        if let Some(id) = &bundle_id
            && !self.bundle_ids.insert(id.clone())
        {
            return;
        }
        let location = path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.apps.push(App {
            folded: fold(name.as_bytes()).into_boxed_slice(),
            name: name.as_ref().into(),
            path: path.to_path_buf(),
            bundle_id,
            location: location.into(),
        });
    }
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.as_encoded_bytes().starts_with(b"."))
}

/// Reads `CFBundleIdentifier` from the bundle's Info.plist (XML or binary).
/// A missing or malformed plist is not an error: the app is still listed.
fn read_bundle_id(bundle: &Path) -> Option<String> {
    let plist_path = bundle.join("Contents/Info.plist");
    let value = match plist::Value::from_file(&plist_path) {
        Ok(value) => value,
        Err(error) => {
            tracing::debug!(bundle = %bundle.display(), %error, "unreadable Info.plist");
            return None;
        }
    };
    value
        .as_dictionary()?
        .get("CFBundleIdentifier")?
        .as_string()
        .map(str::to_owned)
}

/// Searches installed applications by name.
pub struct AppProvider {
    catalog: AppCatalog,
}

impl AppProvider {
    pub fn new(catalog: AppCatalog) -> Self {
        Self { catalog }
    }

    pub fn catalog(&self) -> &AppCatalog {
        &self.catalog
    }
}

impl Provider for AppProvider {
    fn name(&self) -> &'static str {
        "apps"
    }

    fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
        let Some(matcher) = NameMatcher::new(query) else {
            return Vec::new();
        };
        let mut scored: Vec<(u64, &App)> = self
            .catalog
            .apps
            .iter()
            .filter_map(|app| matcher.score(&app.folded).map(|(_, key)| (key, app)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        scored
            .into_iter()
            .take(limit)
            .map(|(score, app)| ResultItem {
                id: app.path.to_string_lossy().as_ref().into(),
                kind: ResultKind::Application,
                title: app.name.clone(),
                subtitle: Some(app.location.clone()),
                score,
                action: Action::OpenApplication(app.path.clone()),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
