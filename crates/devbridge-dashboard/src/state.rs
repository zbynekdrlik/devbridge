use std::time::Instant;

/// Shared application state for the dashboard.
#[derive(Clone)]
pub struct AppState {
    pub mode: String,
    pub version: String,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(mode: String) -> Self {
        Self {
            mode,
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: Instant::now(),
        }
    }
}
