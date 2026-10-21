//! The launcher's own data: the hand-edited `config.toml` (snippets,
//! custom commands, options) and `state.db` (recents, favorites,
//! clipboard history). Both live in `~/Library/Application Support/sift`
//! and never leave the machine.

mod config;
mod store;

use std::path::PathBuf;

pub use config::{
    ClipboardConfig, CommandAction, Config, CustomCommand, Loaded, Snippet, TEMPLATE,
    UpdatesConfig, expand_tilde,
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

/// The current time in Unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
