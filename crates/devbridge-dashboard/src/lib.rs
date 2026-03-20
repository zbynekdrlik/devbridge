pub mod api;
pub mod state;
pub mod static_files;

use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::api::api_router;
use crate::state::AppState;
use crate::static_files::static_handler;

/// Build the full dashboard router with API routes, static file serving, CORS, and tracing.
pub fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .nest("/api", api_router())
        .fallback(static_handler)
        .with_state(state)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
}
