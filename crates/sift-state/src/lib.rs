//! The launcher's own data: the hand-edited `config.toml` (snippets,
//! custom commands, options) and `state.db` (recents, favorites,
//! clipboard history). Both live in `~/Library/Application Support/sift`
//! and never leave the machine, except that `config.toml` can live in
//! the XDG config directory instead (see [`config_file`]).

mod config;
mod store;

use std::path::{Path, PathBuf};

pub use config::{
    ClipboardConfig, CommandAction, Config, CustomCommand, Loaded, MAX_CLIP_ITEMS, Setting,
    Snippet, TEMPLATE, UpdatesConfig, apply_setting, expand_tilde,
};
pub use store::{ClipEntry, FAVORITE_BOOST, MAX_CLIP_BYTES, MAX_USAGE_ROWS, StateDb};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("state.db was written by a newer version of sift (schema {0})")]
    NewerSchema(i32),
}

/// `~/Library/Application Support/sift`.
pub fn support_dir() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join("Library/Application Support/sift"))
}

/// Where `config.toml` is, given the support directory. The first of
/// these that exists wins:
///
/// 1. `$XDG_CONFIG_HOME/sift/config.toml`
/// 2. `~/.config/sift/config.toml`, the XDG default. It is checked even
///    when `XDG_CONFIG_HOME` is unset, because apps opened from the Dock
///    or at login don't see variables set in a shell profile.
/// 3. `config.toml` in the support directory.
///
/// When none exists yet, a new file goes in `$XDG_CONFIG_HOME/sift` if
/// that is set, and in the support directory otherwise.
pub fn config_file(support_dir: &Path) -> PathBuf {
    pick_config_file(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::home_dir(),
        support_dir,
        |path| path.is_file(),
    )
}

fn pick_config_file(
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
    support_dir: &Path,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    // The XDG spec says to ignore a relative (or empty) XDG_CONFIG_HOME.
    let xdg = xdg_config_home
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("sift/config.toml"));
    let xdg_default = home.map(|home| home.join(".config/sift/config.toml"));
    let support = support_dir.join("config.toml");
    let candidates = [xdg.clone(), xdg_default, Some(support.clone())];
    candidates
        .into_iter()
        .flatten()
        .find(|path| exists(path))
        .or(xdg)
        .unwrap_or(support)
}

/// The current time in Unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(xdg: Option<&str>, existing: &[&str]) -> PathBuf {
        pick_config_file(
            xdg.map(PathBuf::from),
            Some(PathBuf::from("/Users/me")),
            Path::new("/Users/me/Library/Application Support/sift"),
            |path| existing.iter().any(|e| path == Path::new(e)),
        )
    }

    const SUPPORT: &str = "/Users/me/Library/Application Support/sift/config.toml";
    const DOT_CONFIG: &str = "/Users/me/.config/sift/config.toml";
    const XDG: &str = "/Users/me/dotfiles/sift/config.toml";

    #[test]
    fn support_dir_by_default() {
        assert_eq!(pick(None, &[]), Path::new(SUPPORT));
        assert_eq!(pick(None, &[SUPPORT]), Path::new(SUPPORT));
    }

    #[test]
    fn dot_config_is_used_when_it_exists() {
        assert_eq!(pick(None, &[DOT_CONFIG]), Path::new(DOT_CONFIG));
        assert_eq!(pick(None, &[DOT_CONFIG, SUPPORT]), Path::new(DOT_CONFIG));
    }

    #[test]
    fn xdg_config_home_comes_first() {
        let xdg = Some("/Users/me/dotfiles");
        assert_eq!(pick(xdg, &[XDG, DOT_CONFIG, SUPPORT]), Path::new(XDG));
        // A new file goes there too.
        assert_eq!(pick(xdg, &[]), Path::new(XDG));
    }

    #[test]
    fn an_existing_file_beats_an_empty_xdg_config_home() {
        // Setting the variable later must not hide a config already in use.
        let xdg = Some("/Users/me/dotfiles");
        assert_eq!(pick(xdg, &[SUPPORT]), Path::new(SUPPORT));
        assert_eq!(pick(xdg, &[DOT_CONFIG]), Path::new(DOT_CONFIG));
    }

    #[test]
    fn relative_xdg_config_home_is_ignored() {
        assert_eq!(pick(Some("dotfiles"), &[]), Path::new(SUPPORT));
        assert_eq!(pick(Some(""), &[]), Path::new(SUPPORT));
    }
}
