//! Checking for a newer release.
//!
//! At most once a day, and only while `[updates] check` is on (the
//! default), Sift asks GitHub for the latest release of this repository.
//! It sends one HTTPS GET request with no data about the user beyond what
//! any HTTPS request carries (address, user agent "Sift/VERSION"). If the
//! release is newer, the launcher says so and the *Check for Updates*
//! command opens the release page. Nothing is downloaded or installed.
//!
//! The reply is untrusted input: only its version string is used. The page
//! that opens is always the fixed [`RELEASES_PAGE`], never a link from
//! the reply.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Where users download releases.
pub const RELEASES_PAGE: &str = "https://github.com/kylianmarceau/sift/releases/latest";
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/kylianmarceau/sift/releases/latest";
/// This build's version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Automatic checks happen at most this often.
pub const INTERVAL_SECS: i64 = 24 * 60 * 60;
const TIMEOUT: Duration = Duration::from_secs(15);
/// Where the time and result of the last check are kept.
const STATE_FILE: &str = "update-check";

/// What is known about updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    /// Not checked in this run, and nothing remembered.
    Unknown,
    Checking,
    UpToDate,
    /// A newer release, by version ("0.2.0").
    Available(String),
    Failed(String),
}

/// A release version: `major.minor.patch`, with an optional pre-release
/// suffix (`-beta.1`), which counts as older than the plain version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    /// `true` for a plain release, so it sorts above its pre-releases.
    release: bool,
}

impl Version {
    /// Parses "1.2.3", "v1.2.3" or "1.2.3-rc.1". Returns `None` for
    /// anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let core = core.split_once('+').map_or(core, |(c, _)| c);
        let mut parts = core.split('.').map(|p| {
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse::<u64>().ok())
                .flatten()
        });
        let version = Self {
            major: parts.next()??,
            minor: parts.next()??,
            patch: parts.next()??,
            release: pre.is_none_or(str::is_empty),
        };
        parts.next().is_none().then_some(version)
    }
}

/// Whether `latest` (a release tag) is newer than this build. Pre-releases
/// are never offered.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (Version::parse(latest), Version::parse(current)) {
        (Some(latest), Some(current)) => latest.release && latest > current,
        _ => false,
    }
}

/// The URL to ask: the GitHub API, or `SIFT_UPDATE_URL` (for testing,
/// which may be a `file://` URL).
fn source() -> (String, bool) {
    match std::env::var("SIFT_UPDATE_URL") {
        Ok(url) if !url.is_empty() => (url, true),
        _ => (LATEST_RELEASE_API.to_string(), false),
    }
}

/// Asks for the latest release. Blocking (runs `curl`); call it on a
/// background thread. Returns its version, or `None` when the repository
/// has no published release (or is not public).
pub fn fetch_latest() -> Result<Option<String>, String> {
    let (url, testing) = source();
    fetch_from(&url, testing)
}

/// [`fetch_latest`] from `url`; `allow_file` also accepts `file://`.
fn fetch_from(url: &str, allow_file: bool) -> Result<Option<String>, String> {
    let testing = allow_file;
    let output = Command::new("/usr/bin/curl")
        .args(["--silent", "--show-error", "--location"])
        // HTTPS only, also across redirects; file:// only for testing.
        .args(["--proto", if testing { "=https,file" } else { "=https" }])
        .args(["--proto-redir", "=https"])
        .args(["--max-time", &TIMEOUT.as_secs().to_string()])
        .args(["--max-filesize", "1000000"])
        .args(["--header", "Accept: application/vnd.github+json"])
        .args(["--user-agent", &format!("Sift/{CURRENT}")])
        .args(["--write-out", "\n%{http_code}"])
        .arg(url)
        .output()
        .map_err(|error| format!("cannot run curl: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().trim_start_matches("curl: ").to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, ""));
    match code.trim() {
        // file:// reports 000.
        "200" | "000" => parse_release(body),
        "404" => Ok(None),
        code => Err(format!("GitHub answered with status {code}")),
    }
}

/// The version of a GitHub "latest release" reply, if it is a published,
/// final release.
fn parse_release(body: &str) -> Result<Option<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "the reply is not valid JSON".to_string())?;
    let flag = |name: &str| value.get(name).and_then(|v| v.as_bool()).unwrap_or(false);
    if flag("draft") || flag("prerelease") {
        return Ok(None);
    }
    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("the reply has no tag_name")?;
    Version::parse(tag).ok_or_else(|| format!("unrecognised version tag {tag:?}"))?;
    Ok(Some(tag.trim().trim_start_matches('v').to_string()))
}

/// The time of the last successful check and the latest version it saw.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LastCheck {
    pub at: i64,
    pub latest: Option<String>,
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

/// Reads the last check. A missing or unreadable file means never.
pub fn load(dir: &Path) -> LastCheck {
    let Ok(text) = std::fs::read_to_string(state_path(dir)) else {
        return LastCheck::default();
    };
    let mut lines = text.lines();
    let at = lines
        .next()
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0);
    let latest = lines
        .next()
        .map(str::trim)
        .filter(|v| Version::parse(v).is_some())
        .map(str::to_string);
    LastCheck { at, latest }
}

/// Records a check (best effort: failing to write only means checking
/// again sooner).
pub fn save(dir: &Path, check: &LastCheck) {
    let text = format!("{}\n{}\n", check.at, check.latest.as_deref().unwrap_or(""));
    let _ = std::fs::create_dir_all(dir);
    if let Err(error) = std::fs::write(state_path(dir), text) {
        tracing::debug!(%error, "cannot record the update check");
    }
}

/// Whether an automatic check is due at `now`.
pub fn due(last: &LastCheck, now: i64) -> bool {
    now - last.at >= INTERVAL_SECS || now < last.at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        // Pre-releases are never offered, even when higher.
        assert!(!is_newer("0.2.0-beta.1", "0.1.0"));
        // A final release is newer than its own pre-release.
        assert!(is_newer("0.2.0", "0.2.0-rc.1"));
        // Nonsense is never newer.
        for bad in ["", "latest", "1.2", "1.2.3.4", "v1..3", "1.2.x", "１.2.3"] {
            assert!(!is_newer(bad, "0.1.0"), "{bad:?}");
        }
    }

    #[test]
    fn reads_only_the_version_from_a_release_reply() {
        let reply = r#"{"tag_name": "v0.3.1", "html_url": "https://evil.example", "draft": false, "prerelease": false}"#;
        assert_eq!(parse_release(reply), Ok(Some("0.3.1".to_string())));
        let pre = r#"{"tag_name": "v0.4.0-rc.1", "prerelease": true}"#;
        assert_eq!(parse_release(pre), Ok(None));
        assert!(parse_release(r#"{"tag_name": "nightly"}"#).is_err());
        assert!(parse_release("<html>rate limited</html>").is_err());
        assert!(parse_release(r#"{"name": "x"}"#).is_err());
    }

    #[test]
    fn remembers_the_last_check_and_spaces_checks_a_day_apart() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(load(dir.path()), LastCheck::default());
        let check = LastCheck {
            at: 1_000_000,
            latest: Some("0.2.0".into()),
        };
        save(dir.path(), &check);
        assert_eq!(load(dir.path()), check);
        assert!(!due(&check, 1_000_000 + INTERVAL_SECS - 1));
        assert!(due(&check, 1_000_000 + INTERVAL_SECS));
        // A clock that went backwards does not block checks forever.
        assert!(due(&check, 999_000));
        std::fs::write(dir.path().join(STATE_FILE), "garbage\n<script>\n").expect("write");
        assert_eq!(load(dir.path()), LastCheck::default());
    }

    #[test]
    fn fetches_from_a_file_url_in_tests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("latest.json");
        std::fs::write(&path, r#"{"tag_name": "v9.9.9"}"#).expect("write");
        let url = format!("file://{}", path.display());
        assert_eq!(fetch_from(&url, true), Ok(Some("9.9.9".to_string())));
        // Outside tests only HTTPS is accepted.
        assert!(fetch_from(&url, false).is_err());
    }
}
