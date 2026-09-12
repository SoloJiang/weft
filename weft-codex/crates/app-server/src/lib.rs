//! Codex `app-server` client crate for weft-codex.
//!
//! Codex-only runtime extracted from the original prototype. Module layout:
//!
//! - [`client`] — the JSON-RPC wire layer + global/per-thread client
//! - [`proto`] — the Codex event model shared with notification mapping
//! - [`proc_registry`] — child-process registration / reaping
//! - [`detect`] — GUI-launch PATH augmentation for the Codex executable
//! - [`command`] — the single Codex executable setting

// Panic-prone code is banned in production paths (same bar as the weft repo).
// The `not(test)` guard lets test modules use unwrap/expect freely.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod client;
pub mod command;
pub mod detect;
pub mod paths;
pub mod proc_registry;
pub mod proto;
