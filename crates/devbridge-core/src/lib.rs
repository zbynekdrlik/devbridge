pub mod config;
pub mod error;
pub mod ipc;
pub mod job;
pub mod proto;

pub use config::Config;
pub use error::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Printer metadata returned by the `/api/printers` endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrinterInfo {
    pub name: String,
    pub driver: String,
    pub status: String,
    pub jobs: u64,
    pub is_target: bool,
}
