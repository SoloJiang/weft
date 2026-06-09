//! The chat engine behind the issue console: a long-lived headless `claude -p`
//! stream-json process per timeline, with weft-owned messages in SQLite and
//! incremental pushes to the frontend. Replaces the PTY+jsonl-projection lead.
//! Spec: docs/superpowers/specs/2026-06-10-lead-chat-engine-design.md

pub mod commands;
pub mod engine;
pub mod proto;
