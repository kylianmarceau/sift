//! Detects whether the previous run ended without quitting normally.
//!
//! A small marker file holds the running launcher's process id. It is
//! removed when Sift quits normally. If it is still there at the next start
//! and that process is gone, the previous run crashed or was killed: the
//! file index is then checked page by page (`quick_check`) at startup, and
//! the user is told where the log is.

use std::path::{Path, PathBuf};

const MARKER: &str = "sift.pid";

pub struct Session {
    marker: PathBuf,
}

impl Session {
    /// Writes the marker in `dir`. Returns the session and whether the
    /// previous run ended uncleanly.
    pub fn start(dir: &Path) -> (Self, bool) {
        let marker = dir.join(MARKER);
        let previous = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        let ours = std::process::id() as i32;
        let unclean =
            previous.is_some_and(|pid| pid != ours && !sift_macos::process_is_running(pid));
        if unclean {
            tracing::warn!(pid = previous, "the previous run did not quit normally");
        }
        let _ = std::fs::create_dir_all(dir);
        if let Err(error) = std::fs::write(&marker, ours.to_string()) {
            tracing::warn!(%error, "cannot write the session marker");
        }
        (Self { marker }, unclean)
    }

    /// Removes the marker; call when quitting normally.
    pub fn end(&self) {
        let ours = std::process::id().to_string();
        if std::fs::read_to_string(&self.marker).is_ok_and(|text| text.trim() == ours) {
            let _ = std::fs::remove_file(&self.marker);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_left_by_a_dead_process_means_an_unclean_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (session, unclean) = Session::start(dir.path());
        assert!(!unclean);
        session.end();
        assert!(!dir.path().join(MARKER).exists());

        // A process that is gone left its marker behind.
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn");
        let dead = child.id();
        child.wait().expect("wait");
        std::fs::write(dir.path().join(MARKER), dead.to_string()).expect("write");
        let (session, unclean) = Session::start(dir.path());
        assert!(unclean);
        session.end();

        // A marker of a live process (another running Sift) is not a crash.
        std::fs::write(dir.path().join(MARKER), "1").expect("write");
        let (_session, unclean) = Session::start(dir.path());
        assert!(!unclean);
    }
}
