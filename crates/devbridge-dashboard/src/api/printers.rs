use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/printers", get(get_printers))
}

async fn get_printers() -> Json<Value> {
    // Placeholder: return an empty list of printers.
    Json(json!([]))
}
