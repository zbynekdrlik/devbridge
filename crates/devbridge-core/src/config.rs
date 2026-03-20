use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub general: GeneralConfig,
    pub server: ServerConfig,
    pub client: ClientConfig,
    pub jobs: JobsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    pub mode: String,
    pub log_level: String,
    pub data_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub ipp_port: u16,
    pub grpc_port: u16,
    pub dashboard_port: u16,
    pub printer_name: String,
    pub spool_dir: String,
    pub tls: TlsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub server_address: String,
    pub target_printer: String,
    pub dashboard_port: u16,
    pub reconnect_interval_secs: u64,
    pub max_reconnect_interval_secs: u64,
    pub tls: TlsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub cert_file: String,
    pub key_file: String,
    pub ca_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobsConfig {
    pub max_retries: u32,
    pub retry_delay_secs: u64,
    pub job_expiry_hours: u64,
    pub max_payload_size_mb: u64,
}

impl Config {
    pub fn load(path: &Path) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(Error::Io)?;
        let config: Config = toml::from_str(&content).map_err(|e| Error::Config(e.to_string()))?;
        Ok(config)
    }
}
