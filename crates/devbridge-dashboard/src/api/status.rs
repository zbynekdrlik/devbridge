use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/status", get(get_status))
}

async fn get_status(State(state): State<AppState>) -> Json<Value> {
    let uptime = state.started_at.elapsed();
    Json(json!({
        "mode": state.mode,
        "version": state.version,
        "uptime_secs": uptime.as_secs(),
        "status": "running",
    }))
}
