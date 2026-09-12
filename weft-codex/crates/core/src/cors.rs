//! CORS for the Desktop host origin after the UI mounts in the same document.
//!
//! The React tree lives on `app://-`. Requests to weftd are cross-origin.
//! Only that measured host origin is allowed; arbitrary `Origin` values are
//! not reflected. Codex MCP clients do not send `Origin`, so `/bus` is
//! unaffected.

use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// Origins the Desktop renderer is known to send.
pub fn allowed_origin(origin: &str) -> bool {
    origin == "app://-" || origin == "app://"
}

/// CORS is for the same-document UI talking to weftd. `/bus` is MCP, not a
/// browser page, and must not grow CORS headers.
pub fn cors_enabled_path(path: &str) -> bool {
    path == "/healthz" || path == "/api" || path.starts_with("/api/") || path.starts_with("/web/")
}

pub async fn layer(req: Request, next: Next) -> Response {
    if !cors_enabled_path(req.uri().path()) {
        return next.run(req).await;
    }
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let allowed = origin
        .as_deref()
        .filter(|value| allowed_origin(value))
        .map(str::to_owned);

    if req.method() == Method::OPTIONS {
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::NO_CONTENT;
        if let Some(origin) = allowed.as_deref() {
            apply_cors_headers(response.headers_mut(), origin);
        }
        return response;
    }

    let mut response = next.run(req).await;
    if let Some(origin) = allowed.as_deref() {
        apply_cors_headers(response.headers_mut(), origin);
    }
    response
}

fn apply_cors_headers(headers: &mut axum::http::HeaderMap, origin: &str) {
    if let Ok(value) = HeaderValue::from_str(origin) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
}

#[cfg(test)]
mod tests {
    use super::allowed_origin;

    #[test]
    fn allows_desktop_host_origin() {
        assert!(allowed_origin("app://-"));
        assert!(allowed_origin("app://"));
    }

    #[test]
    fn rejects_arbitrary_origins() {
        assert!(!allowed_origin("https://example.com"));
        assert!(!allowed_origin("http://127.0.0.1:47810"));
        assert!(!allowed_origin("null"));
    }

    #[test]
    fn cors_covers_ui_paths_only() {
        assert!(super::cors_enabled_path("/healthz"));
        assert!(super::cors_enabled_path("/api/events"));
        assert!(super::cors_enabled_path("/web/weft.css"));
        assert!(!super::cors_enabled_path("/bus/thread/dir/mcp"));
        assert!(!super::cors_enabled_path("/"));
    }
}
