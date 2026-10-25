//! UI-independent window management. No AppKit calls, timers, or background
//! threads: events and commands change a model and produce desired frames.
//! Inspired by AeroSpace's MIT-licensed tree/workspace model; see
//! docs/window-manager.md for scope, attribution, and platform constraints.

mod command;
mod config;
mod geometry;
mod model;
pub mod protocol;
mod tree;

pub use command::{
    Command, CursorTarget, Direction, LayoutRequest, SnapPosition, parse, parse_args,
};
pub use config::{Config, Gaps, GapsOverride, Hooks, MouseConfig, Rule, WindowTarget};
pub use geometry::{Axis, Rect};
pub use model::{Effect, Engine, Monitor, Snapshot, Window, WindowId, Workspace};
pub use tree::Layout;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

/// The local control socket accepts structured window commands. Arbitrary
/// shell execution belongs to explicitly configured key bindings only.
pub fn protocol_command(args: &[String]) -> Result<Command> {
    let command = parse_args(args)?;
    fn safe(command: &Command) -> bool {
        match command {
            Command::Exec(_) | Command::Sequence(_) => false,
            Command::Target(_, command) | Command::NamedTarget(_, command) => safe(command),
            _ => true,
        }
    }
    if !safe(&command) {
        return Err(Error(
            "shell commands cannot be submitted through the control socket".into(),
        ));
    }
    Ok(command)
}
