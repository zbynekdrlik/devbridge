use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{info, warn};

use devbridge_core::config::SerialBridgeServerEntry;

/// Manages virtual COM port writers for serial bridge connections.
/// On Windows, lazily opens com0com virtual ports and writes data.
/// On other platforms, logs an error (serial bridge is Windows-only).
pub struct SerialBridgeManager {
    configs: HashMap<String, SerialBridgeServerEntry>,
    /// Per-client message counter for audit logging. Helps diagnose
    /// "scanner scanned but nothing arrived at ERP" reports without
    /// manual testing.
    counters: std::sync::Arc<std::sync::Mutex<HashMap<String, AtomicU64>>>,
    #[cfg(windows)]
    ports:
        std::sync::Arc<std::sync::Mutex<HashMap<String, Box<dyn serialport::SerialPort + Send>>>>,
}

impl SerialBridgeManager {
    pub fn new(entries: Vec<SerialBridgeServerEntry>) -> Self {
        let configs: HashMap<String, SerialBridgeServerEntry> = entries
            .into_iter()
            .map(|e| (e.client_id.clone(), e))
            .collect();
        if !configs.is_empty() {
            info!(
                count = configs.len(),
                clients = %configs.keys().cloned().collect::<Vec<_>>().join(", "),
                "serial bridge manager initialized with configured mappings"
            );
            for (cid, cfg) in &configs {
                info!(
                    client_id = %cid,
                    virtual_port = %cfg.virtual_port,
                    baud = cfg.baud_rate,
                    "serial bridge mapping"
                );
            }
        }
        Self {
            configs,
            counters: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            #[cfg(windows)]
            ports: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Returns the total number of serial messages written for this client
    /// (used by the dashboard / health endpoints for audit).
    pub fn message_count(&self, client_id: &str) -> u64 {
        self.counters
            .lock()
            .ok()
            .and_then(|g| g.get(client_id).map(|c| c.load(Ordering::Relaxed)))
            .unwrap_or(0)
    }

    fn bump_counter(&self, client_id: &str) -> u64 {
        let mut counters = match self.counters.lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        counters
            .entry(client_id.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }

    #[cfg(windows)]
    pub async fn write(&self, client_id: &str, data: &[u8]) -> Result<(), String> {
        let config = self.configs.get(client_id).ok_or_else(|| {
            warn!(
                client_id = %client_id,
                "serial bridge: received SerialData but NO config for this client_id"
            );
            format!("no serial bridge config for client '{}'", client_id)
        })?;

        let count = self.bump_counter(client_id);
        let preview: String = data
            .iter()
            .take(40)
            .map(|b| {
                if b.is_ascii_graphic() || *b == b' ' {
                    *b as char
                } else {
                    '.'
                }
            })
            .collect();
        info!(
            client_id = %client_id,
            virtual_port = %config.virtual_port,
            bytes = data.len(),
            count = count,
            preview = %preview,
            "serial bridge: received SerialData over gRPC, forwarding to virtual COM"
        );

        let port_name = config.virtual_port.clone();
        let baud = config.baud_rate;
        let data_vec = data.to_vec();
        let ports = std::sync::Arc::clone(&self.ports);
        let cid = client_id.to_string();
        let port_name_log = port_name.clone();

        tokio::task::spawn_blocking(move || {
            let mut ports_guard = ports.lock().map_err(|e| format!("mutex poisoned: {}", e))?;

            if !ports_guard.contains_key(&cid) {
                match serialport::new(&port_name, baud)
                    .timeout(std::time::Duration::from_secs(5))
                    .open()
                {
                    Ok(port) => {
                        info!(
                            port = %port_name,
                            client = %cid,
                            baud = baud,
                            "serial bridge: virtual COM port opened for first write"
                        );
                        ports_guard.insert(cid.clone(), port);
                    }
                    Err(e) => {
                        warn!(
                            port = %port_name,
                            client = %cid,
                            error = %e,
                            "serial bridge: failed to open virtual COM port"
                        );
                        return Err(format!("failed to open {}: {}", port_name, e));
                    }
                }
            }

            let write_result = {
                use std::io::Write;
                let port = ports_guard.get_mut(&cid).unwrap();
                port.write_all(&data_vec).map_err(|e| e.to_string())
            };

            if let Err(ref e) = write_result {
                warn!(client = %cid, error = %e, "serial write failed, will reopen");
                ports_guard.remove(&cid);
            }

            write_result.map_err(|e| format!("write error: {}", e))
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
        .inspect(|_| {
            info!(
                client_id = %client_id,
                virtual_port = %port_name_log,
                bytes = data.len(),
                "serial bridge: wrote {} bytes to {} successfully",
                data.len(),
                port_name_log
            );
        })
    }

    #[cfg(not(windows))]
    pub async fn write(&self, client_id: &str, data: &[u8]) -> Result<(), String> {
        let _ = self.bump_counter(client_id);
        warn!(
            client_id = %client_id,
            bytes = data.len(),
            "serial bridge: received SerialData on non-Windows platform, not supported"
        );
        Err(format!(
            "serial bridge not supported on this platform (client '{}')",
            client_id
        ))
    }

    pub fn has_config(&self, client_id: &str) -> bool {
        self.configs.contains_key(client_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_no_config() {
        let mgr = SerialBridgeManager::new(vec![]);
        assert!(!mgr.has_config("pjkeb-client"));
        assert_eq!(mgr.message_count("pjkeb-client"), 0);
    }

    #[test]
    fn test_manager_with_config() {
        let mgr = SerialBridgeManager::new(vec![SerialBridgeServerEntry {
            client_id: "pjkeb-client".to_string(),
            virtual_port: "COM20".to_string(),
            baud_rate: 9600,
        }]);
        assert!(mgr.has_config("pjkeb-client"));
        assert!(!mgr.has_config("pjsnvs"));
    }

    #[test]
    fn test_counter_bumps() {
        let mgr = SerialBridgeManager::new(vec![SerialBridgeServerEntry {
            client_id: "test".to_string(),
            virtual_port: "COM99".to_string(),
            baud_rate: 9600,
        }]);
        assert_eq!(mgr.bump_counter("test"), 1);
        assert_eq!(mgr.bump_counter("test"), 2);
        assert_eq!(mgr.message_count("test"), 2);
    }
}
