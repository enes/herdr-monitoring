//! Herdr protocol-22 integration. Pane IDs are the correlation key.

mod client;
mod control;
mod events;
pub(crate) mod paths;
pub(crate) mod transport;
mod types;

pub use client::{
    CliHerdrClient, CommandOutput, CommandRunner, HerdrClient, HerdrError, ProcessCommandRunner,
    Result,
};
pub use control::{PaneController, SocketPaneController};
pub use events::{socket_path_from_env, EventShutdown, EventStream, HerdrEvent};
pub use types::{
    correlate, AgentInfo, AgentMetadata, AgentSessionInfo, ForegroundProcess, PaneInfo,
    PaneProcessInfo, PaneTarget, SessionSnapshot,
};
