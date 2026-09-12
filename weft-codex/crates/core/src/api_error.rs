//! Classified failures that the HTTP layer can map without reading prose.
//!
//! [`ArtifactError`] already does this for documents. Everything else used to
//! go through `fail()`, which sniffed "unknown" / "invalid" in the message.
//! That made a store-wording change into an accidental status-code change.
//!
//! Construct one of these and wrap it in `anyhow` at the bail site. `fail()`
//! walks the chain; if it finds an [`ApiError`] it uses the variant, otherwise
//! the response is 500.

use std::fmt;

use serde_json::{json, Value};

/// A failure the API can describe without inspecting a message.
#[derive(Debug, Clone)]
pub enum ApiError {
    NotFound {
        entity: &'static str,
        id: String,
    },
    Conflict {
        code: &'static str,
        message: String,
    },
    BadRequest {
        code: &'static str,
        message: String,
    },
}

impl ApiError {
    pub fn not_found(entity: &'static str, id: impl ToString) -> Self {
        Self::NotFound {
            entity,
            id: id.to_string(),
        }
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::Conflict {
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::BadRequest {
            code,
            message: message.into(),
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::NotFound { .. } => 404,
            Self::Conflict { .. } => 409,
            Self::BadRequest { .. } => 400,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::Conflict { code, .. } | Self::BadRequest { code, .. } => code,
        }
    }

    pub fn body(&self) -> Value {
        match self {
            Self::NotFound { entity, id } => json!({
                "code": "not_found",
                "entity": entity,
                "id": id,
                "error": self.to_string(),
            }),
            Self::Conflict { code, message } | Self::BadRequest { code, message } => json!({
                "code": code,
                "error": message,
            }),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { entity, id } => write!(f, "unknown {entity} {id}"),
            Self::Conflict { message, .. } | Self::BadRequest { message, .. } => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for ApiError {}

/// Walk an anyhow chain for a classified error. Unclassified failures stay
/// internal — a message that happens to contain "unknown" is not a 404.
pub fn classified(error: &anyhow::Error) -> Option<&ApiError> {
    error.chain().find_map(|cause| cause.downcast_ref::<ApiError>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_is_404_with_entity() {
        let error = ApiError::not_found("issue", 12);
        assert_eq!(error.status(), 404);
        assert_eq!(error.code(), "not_found");
        assert_eq!(error.to_string(), "unknown issue 12");
        assert_eq!(error.body()["entity"], "issue");
        assert_eq!(error.body()["id"], "12");
    }

    #[test]
    fn conflict_and_bad_request_keep_their_codes() {
        let conflict = ApiError::conflict("cannot_complete", "cannot complete task 3 from status \"working\"; expected review");
        assert_eq!(conflict.status(), 409);
        assert_eq!(conflict.code(), "cannot_complete");
        let bad = ApiError::bad_request("empty_message", "empty message text");
        assert_eq!(bad.status(), 400);
        assert_eq!(bad.code(), "empty_message");
    }

    #[test]
    fn a_wrapped_api_error_is_still_classified() {
        let error = anyhow::Error::from(ApiError::not_found("workspace", 9)).context("set last workspace");
        let found = classified(&error).expect("still in the chain");
        assert_eq!(found.status(), 404);
        assert_eq!(found.to_string(), "unknown workspace 9");
    }

    #[test]
    fn an_untyped_message_that_says_unknown_is_not_a_404() {
        let error = anyhow::anyhow!("unknown internal invariant broken");
        assert!(classified(&error).is_none());
    }
}
