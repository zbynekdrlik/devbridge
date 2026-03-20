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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const VALID_TOML: &str = r#"
[general]
mode = "server"
log_level = "info"
data_dir = "/tmp/devbridge"

[server]
ipp_port = 631
grpc_port = 50051
dashboard_port = 9090
printer_name = "TestPrinter"
spool_dir = "/tmp/spool"

[server.tls]
cert_file = "server.crt"
key_file = "server.key"
ca_file = "ca.crt"

[client]
server_address = "127.0.0.1:50051"
target_printer = "LocalPrinter"
dashboard_port = 9120
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60

[client.tls]
cert_file = "client.crt"
key_file = "client.key"
ca_file = "ca.crt"

[jobs]
max_retries = 3
retry_delay_secs = 10
job_expiry_hours = 24
max_payload_size_mb = 50
"#;

    #[test]
    fn test_load_valid_config() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(VALID_TOML.as_bytes()).unwrap();

        let config = Config::load(tmp.path()).unwrap();

        assert_eq!(config.general.mode, "server");
        assert_eq!(config.general.log_level, "info");
        assert_eq!(config.general.data_dir, "/tmp/devbridge");
        assert_eq!(config.server.ipp_port, 631);
        assert_eq!(config.server.grpc_port, 50051);
        assert_eq!(config.server.dashboard_port, 9090);
        assert_eq!(config.server.printer_name, "TestPrinter");
        assert_eq!(config.server.spool_dir, "/tmp/spool");
        assert_eq!(config.server.tls.cert_file, "server.crt");
        assert_eq!(config.server.tls.key_file, "server.key");
        assert_eq!(config.server.tls.ca_file, "ca.crt");
        assert_eq!(config.client.server_address, "127.0.0.1:50051");
        assert_eq!(config.client.target_printer, "LocalPrinter");
        assert_eq!(config.client.dashboard_port, 9120);
        assert_eq!(config.client.reconnect_interval_secs, 5);
        assert_eq!(config.client.max_reconnect_interval_secs, 60);
        assert_eq!(config.client.tls.cert_file, "client.crt");
        assert_eq!(config.jobs.max_retries, 3);
        assert_eq!(config.jobs.retry_delay_secs, 10);
        assert_eq!(config.jobs.job_expiry_hours, 24);
        assert_eq!(config.jobs.max_payload_size_mb, 50);
    }

    #[test]
    fn test_load_missing_file_errors() {
        let result = Config::load(Path::new("/nonexistent/path/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_mode_override() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(VALID_TOML.as_bytes()).unwrap();

        let mut config = Config::load(tmp.path()).unwrap();
        assert_eq!(config.general.mode, "server");

        config.general.mode = "client".to_string();
        assert_eq!(config.general.mode, "client");
    }
}
