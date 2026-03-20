use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/jobs", get(get_jobs))
}

async fn get_jobs() -> Json<Value> {
    // Placeholder: return an empty list of jobs.
    Json(json!([]))
}
