pub mod client_registration;
pub mod config;
pub mod error;
pub mod ipc;
pub mod job;
pub mod job_event;
pub mod proto;
pub mod virtual_printer;

pub use client_registration::ClientRegistration;
pub use config::Config;
pub use error::Error;
pub use virtual_printer::VirtualPrinter;

pub type Result<T> = std::result::Result<T, Error>;

/// Format a byte count as a human-readable size string.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Printer metadata returned by the `/api/printers` endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrinterInfo {
    pub name: String,
    pub driver: String,
    pub status: String,
    pub jobs: u64,
    pub is_target: bool,
}
