//! Shared MCP server state: the registry of live stepping-debugger sessions.

use std::collections::HashMap;

use verilog_core::{IrProject, SessionId, SessionRegistry, SimSession};

/// Owns the (`SimSession`, `IrProject`) pair for every active stepping
/// debugger session, keyed by an opaque `SessionId` allocated by
/// [`SessionRegistry`].
///
/// Mirrors the pattern used in `src-tauri/src/sim_commands.rs` so behavior
/// stays consistent between the Tauri shell and the MCP server.
#[derive(Default)]
pub struct SimSessions {
    pub registry: SessionRegistry,
    pub sessions: HashMap<SessionId, SessionEntry>,
}

pub struct SessionEntry {
    pub session: SimSession,
    pub project: IrProject,
}
