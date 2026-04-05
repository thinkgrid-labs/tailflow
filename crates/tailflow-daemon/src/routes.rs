use crate::state::AppState;
use axum::{
    extract::{Query, State},
    http::{header, StatusCode, Uri},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Json, Response,
    },
    routing::get,
    Router,
};
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::sync::Arc;
use tailflow_core::processor::Filter;
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
        .route("/events", get(sse_handler))
        .route("/api/records", get(records_handler))
        .route("/health", get(health_handler))
        // Everything else → embedded web UI
        .fallback(static_handler)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ── Shared filter params ──────────────────────────────────────────────────────

/// Query parameters accepted by `/events` and `/api/records`.
///
/// Examples:
///   GET /events?grep=error
///   GET /api/records?source=nginx
///   GET /events?grep=panic&source=api
#[derive(Debug, Deserialize, Default)]
struct FilterParams {
    /// Regex matched against `record.payload`.
    grep: Option<String>,
    /// Substring matched against `record.source`.
    source: Option<String>,
}

impl FilterParams {
    fn into_filter(self) -> Filter {
        let f = match self.grep.as_deref() {
            Some(pat) => Filter::regex(pat).unwrap_or_else(|e| {
                tracing::warn!(pattern = pat, err = %e, "invalid grep regex, ignoring");
                Filter::none()
            }),
            None => Filter::none(),
        };
        match self.source {
            Some(src) => f.with_source(src),
            None => f,
        }
    }
}

// ── SSE ───────────────────────────────────────────────────────────────────────

async fn sse_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FilterParams>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, axum::Error>>> {
    let filter = params.into_filter();
    let rx = state.tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |res| match res {
        Ok(record) if filter.matches(&record) => match serde_json::to_string(&record) {
            Ok(data) => Some(Ok(Event::default().data(data))),
            Err(e) => {
                tracing::error!(err = %e, "failed to serialize log record for SSE");
                None
            }
        },
        Ok(_) => None,
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            tracing::warn!(dropped = n, "SSE client lagged");
            None
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── REST ──────────────────────────────────────────────────────────────────────

async fn records_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FilterParams>,
) -> impl IntoResponse {
    let filter = params.into_filter();
    let records: Vec<_> = state
        .ring
        .lock()
        .unwrap_or_else(|p| p.into_inner()) // recover from poisoned mutex
        .iter()
        .filter(|r| filter.matches(r))
        .cloned()
        .collect();
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
            ([(header::CONTENT_TYPE, mime)], content.data.into_owned()).into_response()
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
