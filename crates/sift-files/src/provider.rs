use std::path::{Path, PathBuf};
use std::sync::Arc;

use sift_core::{Action, Provider, ResultItem, ResultKind};
use sift_search::SearchResult;
use sift_store::EntryKind;

use crate::FileIndex;

/// Queries shorter than this return no files. One character matches a
/// large part of any index and would bury the applications.
pub const MIN_QUERY_CHARS: usize = 2;

/// Files and folders from the index.
pub struct FileProvider {
    index: Arc<FileIndex>,
    home: Option<PathBuf>,
}

impl FileProvider {
    pub fn new(index: Arc<FileIndex>) -> Self {
        Self {
            index,
            home: std::env::home_dir(),
        }
    }

    fn item(&self, result: SearchResult) -> ResultItem {
        let title = result
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| result.path.to_string_lossy().into_owned());
        let subtitle = result
            .path
            .parent()
            .map(|parent| abbreviate_home(parent, self.home.as_deref()));
        let kind = match result.kind {
            EntryKind::Dir => ResultKind::Folder,
            EntryKind::File | EntryKind::Symlink | EntryKind::Other => ResultKind::File,
        };
        ResultItem {
            id: result.path.to_string_lossy().as_ref().into(),
            kind,
            title: title.into(),
            subtitle: subtitle.map(Into::into),
            score: result.score,
            action: Action::OpenPath(result.path),
        }
    }
}

impl Provider for FileProvider {
    fn name(&self) -> &'static str {
        "files"
    }

    fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
        if query.trim().chars().count() < MIN_QUERY_CHARS {
            return Vec::new();
        }
        self.index
            .search(query, limit)
            .into_iter()
            // The index can trail the filesystem by a moment (the watcher
            // batches events). Never offer something that is already gone:
            // one lstat per result, a few microseconds each.
            .filter(|r| std::fs::symlink_metadata(&r.path).is_ok())
            .map(|r| self.item(r))
            .collect()
    }
}

/// `/Users/me/Documents/x` → `~/Documents/x`.
fn abbreviate_home(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviates_the_home_folder() {
        let home = Path::new("/Users/me");
        assert_eq!(abbreviate_home(Path::new("/Users/me"), Some(home)), "~");
        assert_eq!(
            abbreviate_home(Path::new("/Users/me/Documents/a b"), Some(home)),
            "~/Documents/a b"
        );
        assert_eq!(
            abbreviate_home(Path::new("/Users/meow/x"), Some(home)),
            "/Users/meow/x"
        );
        assert_eq!(abbreviate_home(Path::new("/Volumes/X"), None), "/Volumes/X");
    }
}
