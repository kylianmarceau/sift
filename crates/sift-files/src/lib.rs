//! Files and folders in the launcher.
//!
//! - [`FileIndex`]: the open search index, shared between the search
//!   threads and the [`IndexService`], which swaps in a new snapshot after
//!   every update.
//! - [`FileProvider`]: answers launcher queries from the index.
//! - [`IndexService`]: a background thread that owns the writable store,
//!   watches every indexed root with FSEvents, applies changes, and runs
//!   the indexing commands from the settings UI.

mod index;
mod provider;
mod service;

pub use index::FileIndex;
pub use provider::{FileProvider, MIN_QUERY_CHARS, abbreviate_home};
pub use service::{
    Command, IndexService, RootState, RootStatus, ServiceConfig, Status, UpdateInfo,
};
