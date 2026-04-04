use crate::state::AppState;
use axum::{
    extract::State,
    http::{header, Uri, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Json, Response,
    },
    routing::get,
    Router,
};
use rust_embed::RustEmbed;
use std::sync::Arc;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tower_http::cors::CorsLayer;

/// The compiled web UI, embedded at build time from `../../web/dist`.
/// Run `npm run build` in the `web/` directory before `cargo build`.
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct WebAssets;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // API routes — matched before the static fallback
        .route("/events",      get(sse_handler))
        .route("/api/records", get(records_handler))
        .route("/health",      get(health_handler))
        // Everything else → embedded web UI
        .fallback(static_handler)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ── SSE ───────────────────────────────────────────────────────────────────────

async fn sse_handler(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, axum::Error>>> {
    let rx = state.tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(record) => {
            let data = serde_json::to_string(&record).unwrap_or_default();
            Some(Ok(Event::default().data(data)))
        }
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            tracing::warn!(dropped = n, "SSE client lagged");
            None
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── REST ──────────────────────────────────────────────────────────────────────

async fn records_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let records = state.ring.lock().unwrap().clone();
    Json(records)
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
}

// ── Static file server ────────────────────────────────────────────────────────

async fn static_handler(uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');

    // Default to index.html for the root
    let path = if raw.is_empty() { "index.html" } else { raw };

    match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            (
                [(header::CONTENT_TYPE, mime)],
                content.data.into_owned(),
            )
                .into_response()
        }
        // Unknown path → serve index.html (SPA client-side routing)
        None => match WebAssets::get("index.html") {
            Some(content) => Html(content.data.into_owned()).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                "Web UI not built — run: cd web && npm install && npm run build",
            )
                .into_response(),
        },
    }
}
