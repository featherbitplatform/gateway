//! Serves the admin web UI (React SPA node-graph editor) from assets
//! embedded into the binary at build time via `rust-embed`. Mounted as the
//! admin router's unauthenticated fallback; the SPA authenticates its own
//! calls to the admin API.

use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::Embed;

/// Compiled UI bundle (`ui/dist/`, produced by the frontend build and
/// embedded via `build.rs`).
#[derive(Embed)]
#[folder = "ui/dist/"]
struct UiAssets;

/// Fallback handler for all paths not matched by the admin API.
///
/// Serves the embedded asset matching the request path with a MIME type
/// guessed from its extension; if no asset matches, falls back to
/// `index.html` so client-side routes resolve within the SPA. Returns
/// `404 Not Found` only when the UI bundle itself is absent from the binary.
pub async fn serve_ui(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Try to serve the exact file
    if let Some(file) = UiAssets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime.as_ref())],
            file.data.to_vec(),
        )
            .into_response();
    }

    // SPA fallback: serve index.html for all non-API, non-file routes
    if let Some(index) = UiAssets::get("index.html") {
        return Html(String::from_utf8_lossy(&index.data).to_string()).into_response();
    }

    (StatusCode::NOT_FOUND, "Not found").into_response()
}
