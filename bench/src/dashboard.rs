use std::sync::Arc;

use axum::{extract::State, response::Html, routing::get, Json, Router};
use serde_json::json;

use crate::stats::AppState;

pub async fn serve(state: Arc<AppState>, port: u16) {
    let app = Router::new()
        .route("/", get(index))
        .route("/stats", get(stats))
        .with_state(state);
    let addr = format!("127.0.0.1:{port}");
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => {
            println!("[dashboard] http://{addr}");
            if let Err(err) = axum::serve(listener, app).await {
                eprintln!("[dashboard] server error: {err}");
            }
        }
        Err(err) => eprintln!("[dashboard] failed to bind {addr}: {err}"),
    }
}

/// Serves dashboard.html from disk when available so UI edits show up on
/// refresh without a rebuild; the embedded copy is the fallback.
async fn index() -> Html<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/dashboard.html");
    match tokio::fs::read_to_string(path).await {
        Ok(page) => Html(page),
        Err(_) => Html(include_str!("../dashboard.html").to_string()),
    }
}

async fn stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let info = state.info.read().map(|i| i.clone()).unwrap_or_default();
    let shards: Vec<serde_json::Value> = state
        .shards
        .read()
        .map(|shards| {
            shards
                .iter()
                .map(|s| {
                    let samples = s.latency_samples();
                    let latency = crate::stats::latency_summary(&samples);
                    json!({
                        "submitted": s.submitted(),
                        "accepted": s.accepted(),
                        "errors": s.errors(),
                        "executed": s.executed(),
                        "latency_avg_ms": latency.map(|(avg, _)| avg),
                        "latency_p99_ms": latency.map(|(_, p99)| p99),
                        "latency_samples": samples.len(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let history = state
        .history
        .lock()
        .map(|h| {
            let skip = h.len().saturating_sub(600);
            h[skip..].to_vec()
        })
        .unwrap_or_default();
    let sweep = state.sweep.lock().map(|s| s.clone()).unwrap_or_default();
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    Json(json!({
        "info": info,
        "elapsed": state.started.elapsed().as_secs_f64(),
        "cores": cores,
        "shards": shards,
        "history": history,
        "sweep": sweep,
    }))
}
