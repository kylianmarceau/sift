//! Result icons, rendered off the main thread and cached.
//!
//! Applications and packages get their own icon. Documents share one icon
//! per file extension and folders share one folder icon, so a result list
//! of hundreds of files costs a handful of renders (each about 1–15 ms in
//! AppKit, measured at 64 px).

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
}
