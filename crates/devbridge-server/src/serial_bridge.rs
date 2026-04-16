use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{info, warn};

use devbridge_core::config::SerialBridgeServerEntry;

/// Manages virtual COM port writers for serial bridge connections.
pub struct SerialBridgeManager {
    configs: HashMap<String, SerialBridgeServerEntry>,
    ports: Arc<Mutex<HashMap<String, Box<dyn serialport::SerialPort + Send>>>>,
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
                "serial bridge manager initialized"
            );
        }
        Self {
            configs,
            ports: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn write(&self, client_id: &str, data: &[u8]) -> Result<(), String> {
        let config = self
            .configs
            .get(client_id)
            .ok_or_else(|| format!("no serial bridge config for client '{}'", client_id))?;

        let port_name = config.virtual_port.clone();
        let baud = config.baud_rate;
        let data = data.to_vec();
        let ports = Arc::clone(&self.ports);
        let cid = client_id.to_string();

        tokio::task::spawn_blocking(move || {
            let mut ports_guard = ports.lock().map_err(|e| format!("mutex poisoned: {}", e))?;

            if !ports_guard.contains_key(&cid) {
                match serialport::new(&port_name, baud)
                    .timeout(Duration::from_secs(5))
                    .open()
                {
                    Ok(port) => {
                        info!(port = %port_name, client = %cid, "virtual COM port opened");
                        ports_guard.insert(cid.clone(), port);
                    }
                    Err(e) => {
                        return Err(format!("failed to open {}: {}", port_name, e));
                    }
                }
            }

            let write_result = {
                use std::io::Write;
                let port = ports_guard.get_mut(&cid).unwrap();
                port.write_all(&data).map_err(|e| e.to_string())
            };

            if let Err(ref e) = write_result {
                warn!(client = %cid, error = %e, "serial write failed, will reopen");
                ports_guard.remove(&cid);
            }

            write_result.map_err(|e| format!("write error: {}", e))
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
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
}
