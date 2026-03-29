use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use devbridge_core::job::JobEvent;
use devbridge_server::JobQueue;
use devbridge_server::ipp_service::IppServer;
use tokio::sync::{RwLock, broadcast};

/// Shared application state for the dashboard.
#[derive(Clone)]
pub struct AppState {
    pub mode: String,
    pub version: String,
    pub started_at: Instant,
    pub queue: Option<Arc<JobQueue>>,
    pub ipp_server: Option<Arc<IppServer>>,
    pub target_printer: Arc<RwLock<String>>,
    pub config_path: Option<PathBuf>,
    pub connected_clients: Arc<AtomicU64>,
    pub job_events: broadcast::Sender<JobEvent>,
}

impl AppState {
    pub fn new(mode: String) -> Self {
        let (job_events, _) = broadcast::channel(256);
        Self {
            mode,
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: Instant::now(),
            queue: None,
            ipp_server: None,
            target_printer: Arc::new(RwLock::new(String::new())),
            config_path: None,
            connected_clients: Arc::new(AtomicU64::new(0)),
            job_events,
        }
    }

    pub fn with_queue(mut self, queue: Arc<JobQueue>) -> Self {
        self.queue = Some(queue);
        self
    }

    pub fn with_ipp_server(mut self, server: Arc<IppServer>) -> Self {
        self.ipp_server = Some(server);
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

    pub fn with_connected_clients(mut self, connected: Arc<AtomicU64>) -> Self {
        self.connected_clients = connected;
        self
    }

    pub fn with_job_events(mut self, sender: broadcast::Sender<JobEvent>) -> Self {
        self.job_events = sender;
        self
    }
}
