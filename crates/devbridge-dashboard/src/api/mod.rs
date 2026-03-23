pub mod clients;
pub mod config;
pub mod jobs;
pub mod printers;
pub mod status;
pub mod virtual_printers;
pub mod ws;

use axum::Router;

use crate::state::AppState;

/// Combine all API sub-routers into a single router.
pub fn api_router() -> Router<AppState> {
    Router::new()
        .merge(status::router())
        .merge(jobs::router())
        .merge(config::router())
        .merge(printers::router())
        .merge(virtual_printers::router())
        .merge(clients::router())
        .merge(ws::router())
}
