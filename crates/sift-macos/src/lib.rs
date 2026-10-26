//! Native macOS integration for the sift launcher, through `objc2` bindings
//! to AppKit:
//!
//! - [`icon_png`]: the Finder icon of any file, folder or app as PNG bytes.
//! - [`open_application`]: launch or activate an app with `NSWorkspace`.
//! - [`open_path`] / [`open_url`] / [`reveal_in_finder`]: open a file or
//!   URL with its default application, or show a file in Finder.
//! - [`copy_text`] / [`clipboard_text`] / [`clipboard_change_count`] /
//!   [`clear_clipboard`]: write, read and empty plain text on the
//!   clipboard, skipping content marked private.
//!   [`frontmost_app`] names the app that most likely copied it.
//! - [`choose_folder`]: the standard folder picker, non-modal.
//! - [`use_accessory_activation_policy`] / [`activate_app`]: run without a
//!   Dock icon, and come to the front for the settings window.
//! - [`Panel`]: show, hide, position and resize the launcher window.
//! - [`QuickLook`]: preview the selected file with the system Quick Look
//!   panel.
//! - [`paste`]: press ⌘V in the frontmost app, for paste-and-pop.
//!
//! Launcher integrations use public AppKit APIs without special permissions.
//! [`paste`] and the opt-in [`wm`] backend require Accessibility. [`wm`]
//! also dynamically resolves a private AX window-id bridge; it is never
//! started for ordinary search.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaunchError {
    #[error("{0} does not exist")]
    NotFound(PathBuf),
    #[error("{0} is not an application bundle")]
    NotAnApp(PathBuf),
    #[error("macOS could not open {path}: {reason}")]
    Failed { path: PathBuf, reason: String },
    #[error("{0} is not a valid URL")]
    BadUrl(String),
    #[error("could not write to the clipboard")]
    Clipboard,
    #[error("could not run the command: {0}")]
    Command(String),
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

/// Whether a process with this id is running (it may belong to another
/// user). Used to tell a stale "Sift is running" marker from a live one.
pub fn process_is_running(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 only checks that the process exists and
    // may be signalled; it sends nothing.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "macos")]
pub mod a11y;
#[cfg(target_os = "macos")]
mod imp;
#[cfg(target_os = "macos")]
pub mod key_capture;
#[cfg(target_os = "macos")]
pub mod login_item;
#[cfg(target_os = "macos")]
pub mod paste;
#[cfg(target_os = "macos")]
mod quicklook;
#[cfg(target_os = "macos")]
pub mod wm;

#[cfg(target_os = "macos")]
pub use quicklook::QuickLook;

#[cfg(target_os = "macos")]
pub use imp::{
    ClipboardText, ClipboardWatcher, FrontApp, Panel, activate_app, choose_folder, clear_clipboard,
    clipboard_change_count, clipboard_text, copy_text, format_local_now, frontmost_app, icon_png,
    open_application, open_path, open_text_file, open_url, reveal_in_finder,
    use_accessory_activation_policy,
};

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
    fn opening_or_revealing_a_missing_path_fails_immediately() {
        let missing = Path::new("/nonexistent/report.pdf");
        assert!(matches!(
            open_path(missing, |_| {}),
            Err(LaunchError::NotFound(_))
        ));
        assert!(matches!(
            reveal_in_finder(missing),
            Err(LaunchError::NotFound(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tells_running_processes_from_gone_ones() {
        assert!(process_is_running(std::process::id() as i32));
        // launchd always runs, as root.
        assert!(process_is_running(1));
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn");
        let pid = child.id() as i32;
        child.wait().expect("wait");
        assert!(!process_is_running(pid));
        assert!(!process_is_running(0));
    }

    #[test]
    fn invalid_urls_are_refused_before_appkit() {
        assert_eq!(
            open_url("not a url", |_| {}),
            Err(LaunchError::BadUrl("not a url".into()))
        );
        assert_eq!(
            open_url("no-scheme.example", |_| {}),
            Err(LaunchError::BadUrl("no-scheme.example".into()))
        );
    }

    #[test]
    fn formats_the_local_date() {
        let date = format_local_now("yyyy-MM-dd");
        let parts: Vec<&str> = date.split('-').collect();
        assert_eq!(parts.len(), 3, "{date}");
        assert!(parts[0].parse::<u32>().expect("year") >= 2024);
        assert_eq!(format_local_now("HH:mm").len(), 5);
    }

    #[test]
    fn renders_document_and_folder_icons() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, b"x").unwrap();
        for path in [file.as_path(), dir.path()] {
            let png = icon_png(path, 32).expect("icon");
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn renders_a_real_app_icon_as_png() {
        let finder = Path::new("/System/Library/CoreServices/Finder.app");
        if !finder.exists() {
            return;
        }
        let png = icon_png(finder, 64).expect("icon");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR width and height.
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 64);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 64);
    }
}
