use crate::state::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
    routing::get,
    Router,
};
use std::sync::Arc;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tower_http::cors::CorsLayer;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/events", get(sse_handler))
        .route("/api/records", get(records_handler))
        .route("/health", get(health_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// GET /events — Server-Sent Events stream of LogRecord JSON
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

/// GET /api/records — last N buffered records as JSON array
async fn records_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let records = state.ring.lock().unwrap().clone();
    Json(records)
}

/// GET /health
async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"ok": true})))
}
