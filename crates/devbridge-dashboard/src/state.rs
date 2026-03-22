use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use devbridge_server::JobQueue;
use tokio::sync::RwLock;

/// Shared application state for the dashboard.
#[derive(Clone)]
pub struct AppState {
    pub mode: String,
    pub version: String,
    pub started_at: Instant,
    pub queue: Option<Arc<JobQueue>>,
    pub target_printer: Arc<RwLock<String>>,
    pub config_path: Option<PathBuf>,
}

impl AppState {
    pub fn new(mode: String) -> Self {
        Self {
            mode,
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: Instant::now(),
            queue: None,
            target_printer: Arc::new(RwLock::new(String::new())),
            config_path: None,
        }
    }

    pub fn with_queue(mut self, queue: Arc<JobQueue>) -> Self {
        self.queue = Some(queue);
        self
    }

    pub fn with_target_printer(mut self, printer: String) -> Self {
        self.target_printer = Arc::new(RwLock::new(printer));
        self
    }

    pub fn with_shared_target_printer(mut self, printer: Arc<RwLock<String>>) -> Self {
        self.target_printer = printer;
        self
    }

    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }
}
