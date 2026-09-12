pub mod builtin_allow;
pub mod computer_srv;
pub mod global;
pub mod inject;
pub mod server;
pub mod state;

pub use state::{Ask, AskKind, BusRegistry, Msg, Wake, HUMAN, LEAD};

/// Shared inbox crate. The Tauri bus keeps Ask / IM / close fences here and
/// does not share `~/.weft` with weft-codex.
pub use weft_bus as shared_inbox;
