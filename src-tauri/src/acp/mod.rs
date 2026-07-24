//! Generic Agent Client Protocol (ACP) runtime.
//!
//! Protocol concerns (JSON-RPC framing, session/update mapping, permission
//! option selection, process demux) live here. Per-CLI identity belongs under
//! [`backends`]. Codex app-server is a **different** wire dialect and must not
//! use this module.

#![allow(dead_code)] // Runtime/engine wire-in lands in later tasks; pure layers ship first.

pub mod jsonrpc;
pub mod map;
pub mod permission;

// backends + runtime land in subsequent tasks.
// pub mod backends;
// pub mod runtime;

#[allow(unused_imports)]
pub use jsonrpc::{
    classify, encode_error_response, encode_notification, encode_request, encode_response, Incoming,
};
#[allow(unused_imports)]
pub use map::{stop_reason_is_cancelled, stop_reason_is_error, update_to_out, UpdateOut};
#[allow(unused_imports)]
pub use permission::{
    intent_key, intent_key_from_params, pick_option_id, selected_outcome, summary_from_params,
    AlwaysCache, Want,
};
