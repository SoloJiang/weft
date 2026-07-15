//! The chat engine behind the issue console: lead and worker timelines run
//! through their selected tool, with weft-owned messages in SQLite and
//! incremental pushes to the frontend.
//! Spec: docs/superpowers/specs/2026-06-10-lead-chat-engine-design.md

pub mod commands;
pub mod delta_hub;
pub mod engine;
pub mod out_hub;
pub mod proto;
pub mod repo_state;
pub mod revive;
pub mod sentinels;
pub mod test_plan;
pub mod window;
