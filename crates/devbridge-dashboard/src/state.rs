use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use devbridge_core::job::JobEvent;
use devbridge_core::job_event::PrintJobEvent;
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
    pub print_events: broadcast::Sender<PrintJobEvent>,
    // Client identity fields (only set in client mode)
    pub client_id: Option<String>,
    pub printer_display_name: Option<String>,
    pub printer_address: Option<String>,
    pub print_backend: Option<String>,
    pub server_address: Option<String>,
    /// Effective per-job print timeout (seconds) loaded from
    /// `[jobs].print_timeout_secs`. Surfaced on `/api/status` so operators
    /// tuning this knob can verify the value the running service actually
    /// loaded without grepping `config.toml`. Only set when the service
    /// constructs the state from a real config (see runtime.rs). See issue #53.
    pub print_timeout_secs: Option<u64>,
}

impl AppState {
    pub fn new(mode: String) -> Self {
        let (job_events, _) = broadcast::channel(256);
        let (print_events, _) = broadcast::channel(256);
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
            print_events,
            client_id: None,
            printer_display_name: None,
            printer_address: None,
            print_backend: None,
            server_address: None,
            print_timeout_secs: None,
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

    pub fn with_print_events(mut self, sender: broadcast::Sender<PrintJobEvent>) -> Self {
        self.print_events = sender;
        self
    }

    pub fn with_client_config(mut self, config: &devbridge_core::config::ClientConfig) -> Self {
        self.client_id = config.client_id.clone();
        self.printer_display_name = config.printer_display_name.clone();
        self.printer_address = config.printer_address.clone();
        self.print_backend = Some(config.print_backend.clone());
        self.server_address = Some(config.server_address.clone());
        self
    }

    /// Thread the effective `[jobs]` tuning into the dashboard state.
    ///
    /// Currently surfaces `print_timeout_secs` on `/api/status` so operators
    /// can verify which timeout the running service loaded. See issue #53.
    pub fn with_jobs_config(mut self, config: &devbridge_core::config::JobsConfig) -> Self {
        self.print_timeout_secs = Some(config.print_timeout_secs);
        self
    }
}
