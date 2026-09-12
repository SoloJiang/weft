//! weft-codex product core: store, MCP, events. Shared inbox/brief/worktree/
//! scheduler live in `weft-*` crates; this crate is the Codex SessionPort adapter.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod api;
pub mod api_error;
pub mod brief;
pub mod bus;
pub mod cors;
pub mod curator;
pub mod events;
pub mod mcp;
pub mod orchestrator;
pub mod repo_intake;
pub mod runtime;
pub mod store;
pub mod worktree;
