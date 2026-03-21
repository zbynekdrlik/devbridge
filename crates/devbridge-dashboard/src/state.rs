use std::sync::Arc;
use std::time::Instant;

use devbridge_server::JobQueue;

/// Shared application state for the dashboard.
#[derive(Clone)]
pub struct AppState {
    pub mode: String,
    pub version: String,
    pub started_at: Instant,
    pub queue: Option<Arc<JobQueue>>,
}

impl AppState {
    pub fn new(mode: String) -> Self {
        Self {
            mode,
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: Instant::now(),
            queue: None,
        }
    }

    pub fn with_queue(mut self, queue: Arc<JobQueue>) -> Self {
        self.queue = Some(queue);
        self
    }
}
