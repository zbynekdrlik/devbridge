//! `[client.pokladna]` — the Odoo device-job source of the store's pokladna
//! (eKasa receipts + card payments, #99).

use serde::{Deserialize, Serialize};

use super::lan_api::LanApiConfig;
use super::redact_secret;

/// Longest `wait_s` the device-job contract allows a `/pokladna/bridge/next`
/// call to ask for (#99).
pub const POKLADNA_MAX_WAIT_S: u64 = 55;

/// `[client.pokladna]` — the client pulls device jobs (eKasa receipts, card
/// payments) for its pokladňa from Odoo (`POST /pokladna/bridge/next|ack|
/// pending|heartbeat`, JSON-RPC 2.0, Bearer key of scope `pokladna_bridge`)
/// and runs each one AT MOST ONCE on the store's device (#99).
///
/// `Debug` is hand-written so `api_key` can never reach a log line. Unknown
/// keys are tolerated (the device adapters add their own later).
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct PokladnaClientConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Odoo base URL, e.g. `https://erp.slovnormal.sk` (no trailing path).
    #[serde(default)]
    pub url: String,
    /// `pokladna_bridge` API key of the store's dedicated Odoo bridge user
    /// (NOT the label printer's key). Secret — never logged.
    #[serde(default)]
    pub api_key: String,
    /// Long-poll wait of `/pokladna/bridge/next` in seconds (0 = pure short
    /// polling, at most [`POKLADNA_MAX_WAIT_S`]). The HTTP timeout of that
    /// call is `wait_s + 10` s (contract).
    #[serde(default = "default_pokladna_wait_s")]
    pub wait_s: u64,
    /// Seconds between `/pokladna/bridge/heartbeat` calls (also sent at once
    /// when a device status changes).
    #[serde(default = "default_pokladna_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
    /// Tills (`pos.config`) this bridge serves and their devices — the
    /// mapping the Portos / TIM executors use (`[[client.pokladna.kasy]]`).
    #[serde(default)]
    pub kasy: Vec<PokladnaKasaConfig>,
    /// Register the `test` executor (E2E ONLY, never on a store): it drives
    /// no real device, runs only jobs carrying a `devbridge_test` script and
    /// fails every other job as "nothing ran".
    #[serde(default)]
    pub test_executor: bool,
    /// The local HTTPS API of the tills (`[client.pokladna.lan_api]`, #101).
    #[serde(default)]
    pub lan_api: LanApiConfig,
}

/// One `[[client.pokladna.kasy]]` entry — a till and its devices.
///
/// `Debug` is hand-written so `lan_key` can never reach a log line.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct PokladnaKasaConfig {
    /// Odoo `pos.config` id of the till.
    pub id: i64,
    /// Portos eKasa API base URL of this till.
    #[serde(default)]
    pub portos_url: Option<String>,
    /// Card terminal (TIM API) address of this till.
    #[serde(default)]
    pub terminal_ip: Option<String>,
    /// Card terminal id.
    #[serde(default)]
    pub terminal_tid: Option<String>,
    /// Bearer key of this till on the LAN API (#101) — valid for THIS till
    /// only, never the Odoo bridge key. Secret — never logged.
    #[serde(default)]
    pub lan_key: Option<String>,
}

impl std::fmt::Debug for PokladnaKasaConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PokladnaKasaConfig")
            .field("id", &self.id)
            .field("portos_url", &self.portos_url)
            .field("terminal_ip", &self.terminal_ip)
            .field("terminal_tid", &self.terminal_tid)
            .field("lan_key", &self.lan_key.as_deref().map(redact_secret))
            .finish()
    }
}

impl Default for PokladnaClientConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            api_key: String::new(),
            wait_s: default_pokladna_wait_s(),
            heartbeat_interval_secs: default_pokladna_heartbeat_interval_secs(),
            kasy: Vec::new(),
            test_executor: false,
            lan_api: LanApiConfig::default(),
        }
    }
}

impl std::fmt::Debug for PokladnaClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PokladnaClientConfig")
            .field("enabled", &self.enabled)
            .field("url", &self.url)
            .field("api_key", &redact_secret(&self.api_key))
            .field("wait_s", &self.wait_s)
            .field("heartbeat_interval_secs", &self.heartbeat_interval_secs)
            .field("kasy", &self.kasy)
            .field("test_executor", &self.test_executor)
            .field("lan_api", &self.lan_api)
            .finish()
    }
}

/// The contract's default and the Odoo-side cap (`device_job_bridge_wait_max_s`).
fn default_pokladna_wait_s() -> u64 {
    25
}

/// The contract's suggested cadence.
fn default_pokladna_heartbeat_interval_secs() -> u64 {
    30
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::io::Write;

    const POKLADNA_TOML: &str = r#"
[general]
mode = "client"
log_level = "info"
data_dir = "C:/ProgramData/DevBridge"

[server]
ipp_port = 631
grpc_port = 50051
dashboard_port = 9121
printer_name = "unused"
spool_dir = "C:/ProgramData/DevBridge/spool"

[client]
server_address = "10.88.1.100:50051"
target_printer = "P"
dashboard_port = 9120
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60

[client.pokladna]
enabled = true
url = "https://erp.example.test"
api_key = "pokladna-s3cr3t"

[[client.pokladna.kasy]]
id = 3
portos_url = "http://127.0.0.1:3010"
terminal_ip = "192.168.1.50"
terminal_tid = "T0001"
lan_key = "lan-key-of-till-3-0123456789abcdef"
future_key = "tolerated"

[[client.pokladna.kasy]]
id = 4

[jobs]
max_retries = 3
retry_delay_secs = 10
job_expiry_hours = 24
max_payload_size_mb = 50
"#;

    #[test]
    fn test_pokladna_config_parses_with_defaults_and_kasy() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(POKLADNA_TOML.as_bytes()).unwrap();
        let p = Config::load(tmp.path()).unwrap().client.pokladna;
        assert!(p.enabled);
        assert_eq!(p.url, "https://erp.example.test");
        assert_eq!(p.api_key, "pokladna-s3cr3t");
        assert_eq!(p.wait_s, 25);
        assert_eq!(p.heartbeat_interval_secs, 30);
        assert!(!p.test_executor);
        assert_eq!(p.lan_api, LanApiConfig::default(), "absent = disabled");
        assert_eq!(
            p.kasy,
            vec![
                PokladnaKasaConfig {
                    id: 3,
                    portos_url: Some("http://127.0.0.1:3010".into()),
                    terminal_ip: Some("192.168.1.50".into()),
                    terminal_tid: Some("T0001".into()),
                    lan_key: Some("lan-key-of-till-3-0123456789abcdef".into()),
                },
                PokladnaKasaConfig {
                    id: 4,
                    portos_url: None,
                    terminal_ip: None,
                    terminal_tid: None,
                    lan_key: None,
                },
            ]
        );
        assert_eq!(POKLADNA_MAX_WAIT_S, 55);
    }

    #[test]
    fn test_pokladna_explicit_values_and_absent_block() {
        let toml = POKLADNA_TOML.replace(
            "api_key = \"pokladna-s3cr3t\"",
            "api_key = \"pokladna-s3cr3t\"\nwait_s = 0\nheartbeat_interval_secs = 10\ntest_executor = true",
        );
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(toml.as_bytes()).unwrap();
        let p = Config::load(tmp.path()).unwrap().client.pokladna;
        assert_eq!((p.wait_s, p.heartbeat_interval_secs), (0, 10));
        assert!(p.test_executor);

        // the same config without the [client.pokladna] tables
        let start = POKLADNA_TOML.find("[client.pokladna]").unwrap();
        let end = POKLADNA_TOML.find("[jobs]").unwrap();
        let without = format!("{}{}", &POKLADNA_TOML[..start], &POKLADNA_TOML[end..]);
        assert!(!without.contains("pokladna"), "{without}");
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(without.as_bytes()).unwrap();
        let absent = Config::load(tmp.path()).unwrap().client.pokladna;
        assert_eq!(absent, PokladnaClientConfig::default());
        assert!(!absent.enabled && absent.kasy.is_empty() && !absent.test_executor);
    }

    #[test]
    fn test_pokladna_debug_never_prints_the_api_key() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(POKLADNA_TOML.as_bytes()).unwrap();
        let config = Config::load(tmp.path()).unwrap();
        let dbg = format!("{config:?}");
        assert!(!dbg.contains("pokladna-s3cr3t"), "{dbg}");
        assert!(!dbg.contains("lan-key-of-till-3"), "{dbg}");
        assert!(
            dbg.contains("lan_key: Some(\"<redacted>\") }, PokladnaKasaConfig { id: 4"),
            "{dbg}"
        );
        assert!(dbg.contains("terminal_tid: None, lan_key: None }"), "{dbg}");
        assert!(
            dbg.contains(
                "PokladnaKasaConfig { id: 3, portos_url: Some(\"http://127.0.0.1:3010\"), \
                 terminal_ip: Some(\"192.168.1.50\")"
            ),
            "{dbg}"
        );
        assert!(
            dbg.contains("lan_api: LanApiConfig { enabled: false, port: 9443"),
            "{dbg}"
        );
        assert!(dbg.contains("PokladnaClientConfig { enabled: true, url: \"https://erp.example.test\", api_key: \"<redacted>\", wait_s: 25, heartbeat_interval_secs: 30"), "{dbg}");
        assert!(dbg.contains("terminal_tid: Some(\"T0001\")"), "{dbg}");
        let empty = format!("{:?}", PokladnaClientConfig::default());
        assert!(empty.contains("api_key: \"<empty>\""), "{empty}");
        assert!(empty.contains("test_executor: false"), "{empty}");
    }
}
