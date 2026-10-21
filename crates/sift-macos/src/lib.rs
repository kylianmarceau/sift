//! Native macOS integration for the sift launcher, through `objc2` bindings
//! to AppKit:
//!
//! - [`app_icon_png`]: an application's Finder icon as PNG bytes.
//! - [`open_application`]: launch or activate an app with `NSWorkspace`.
//! - [`use_accessory_activation_policy`]: run without a Dock icon.
//! - [`Panel`]: show, hide, position and resize the launcher window.
//!
//! Everything here uses public, documented AppKit APIs and needs no special
//! permissions (no Accessibility, no Full Disk Access).

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaunchError {
    #[error("{0} does not exist")]
    NotFound(PathBuf),
    #[error("{0} is not an application bundle")]
    NotAnApp(PathBuf),
    #[error("macOS could not open {path}: {reason}")]
    Failed { path: PathBuf, reason: String },
}

/// Checks that `path` looks like a launchable bundle before handing it to
/// AppKit, so common failures produce a precise error synchronously.
pub fn validate_app_bundle(path: &Path) -> Result<(), LaunchError> {
    if !path.exists() {
        return Err(LaunchError::NotFound(path.to_path_buf()));
    }
    let is_app = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("app"));
    if !path.is_dir() || !is_app {
        return Err(LaunchError::NotAnApp(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod imp;

#[cfg(target_os = "macos")]
pub use imp::{Panel, app_icon_png, open_application, use_accessory_activation_policy};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_bundles_before_launching() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("Missing.app");
        assert_eq!(
            validate_app_bundle(&missing),
            Err(LaunchError::NotFound(missing.clone()))
        );
        let file = dir.path().join("Fake.app");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            validate_app_bundle(&file),
            Err(LaunchError::NotAnApp(file.clone()))
        );
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        assert!(matches!(
            validate_app_bundle(&folder),
            Err(LaunchError::NotAnApp(_))
        ));
        let app = dir.path().join("Real.app");
        std::fs::create_dir(&app).unwrap();
        assert_eq!(validate_app_bundle(&app), Ok(()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launching_a_missing_app_fails_without_touching_appkit() {
        let result = open_application(Path::new("/nonexistent/Nope.app"), |_| {});
        assert!(matches!(result, Err(LaunchError::NotFound(_))));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn renders_a_real_app_icon_as_png() {
        let finder = Path::new("/System/Library/CoreServices/Finder.app");
        if !finder.exists() {
            return;
        }
        let png = app_icon_png(finder, 64).expect("icon");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR width and height.
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 64);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 64);
    }
}
