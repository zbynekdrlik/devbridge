use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/config", get(get_config))
}

async fn get_config(State(state): State<AppState>) -> Json<Value> {
    // Placeholder: return basic configuration info.
    Json(json!({
        "mode": state.mode,
        "version": state.version,
    }))
}
