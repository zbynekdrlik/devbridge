# Serial Bridge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Forward barcode scanner serial data from store POS terminals to the server over gRPC, exposing a virtual COM port for Codex ERP.

**Architecture:** Client reads local COM port via `serialport` crate, streams `SerialData` messages over a new bidirectional gRPC RPC to the server. Server writes received data to a com0com virtual COM port pair. Codex ERP reads from the paired port. Config-driven, one serial bridge per client.

**Tech Stack:** Rust, tonic/prost (gRPC), serialport crate, com0com (Windows virtual COM driver)

**Spec:** `docs/superpowers/specs/2026-04-16-serial-bridge-design.md`

---

## File Structure

### New Files
| File | Purpose |
|------|---------|
| `crates/devbridge-client/src/serial_bridge.rs` | Client-side: read local COM port, send data over gRPC channel |
| `crates/devbridge-server/src/serial_bridge.rs` | Server-side: receive gRPC serial data, write to com0com virtual port |

### Modified Files
| File | Changes |
|------|---------|
| `proto/devbridge.proto` | Add `StreamSerialData` RPC + `SerialData`/`SerialAck` messages |
| `Cargo.toml` | Version bump 0.8.15 → 0.8.16 |
| `crates/devbridge-app/tauri.conf.json` | Version bump 0.8.15 → 0.8.16 |
| `crates/devbridge-core/src/config.rs` | Add `SerialBridgeClientConfig` and `SerialBridgeServerConfig` structs |
| `crates/devbridge-client/Cargo.toml` | Add `serialport` dependency |
| `crates/devbridge-client/src/lib.rs` | Export `serial_bridge` module |
| `crates/devbridge-client/src/receiver.rs` | Spawn serial bridge task in `run_inner()` |
| `crates/devbridge-server/Cargo.toml` | Add `serialport` dependency |
| `crates/devbridge-server/src/lib.rs` | Export `serial_bridge` module |
| `crates/devbridge-server/src/dispatch.rs` | Implement `StreamSerialData` RPC handler |
| `crates/devbridge-service/src/runtime.rs` | Pass serial bridge config to client/server |
| `CLAUDE.md` | Document serial bridge feature |

---

## Task 1: Version Bump

**Files:**
- Modify: `Cargo.toml:15`
- Modify: `crates/devbridge-app/tauri.conf.json:4`

- [ ] **Step 1: Bump workspace version**

In `Cargo.toml` line 15, change:
```toml
version = "0.8.16"
```

In `crates/devbridge-app/tauri.conf.json` line 4, change:
```json
"version": "0.8.16",
```

- [ ] **Step 2: Commit**

```bash
git add Cargo.toml crates/devbridge-app/tauri.conf.json
git commit -m "Bump to 0.8.16 for serial bridge feature"
```

---

## Task 2: Proto Extension

**Files:**
- Modify: `proto/devbridge.proto`

- [ ] **Step 1: Add SerialData messages after the existing Pong message (line 90)**

```protobuf
// Serial port bridge: client streams barcode scanner data to server
message SerialData {
  string client_id = 1;
  bytes data = 2;
}

message SerialAck {
  bool ok = 1;
}
```

- [ ] **Step 2: Add StreamSerialData RPC to the PrintBridge service (after Heartbeat, line 20)**

```protobuf
  // Client streams serial port data (barcode scans) to the server.
  // Server writes received bytes to a paired virtual COM port for ERP consumption.
  rpc StreamSerialData(stream SerialData) returns (stream SerialAck);
```

- [ ] **Step 3: Verify proto compiles**

```bash
cargo build -p devbridge-core 2>&1 | tail -5
```

Expected: build succeeds (proto codegen produces new types).

- [ ] **Step 4: Commit**

```bash
git add proto/devbridge.proto
git commit -m "Add StreamSerialData RPC for serial port bridge"
```

---

## Task 3: Config Structs

**Files:**
- Modify: `crates/devbridge-core/src/config.rs`

- [ ] **Step 1: Add SerialBridgeClientConfig struct**

After the `TlsConfig` struct (around line 86), add:

```rust
/// Client-side serial bridge configuration.
/// When enabled, the client reads from a local COM port and streams
/// barcode data to the server over gRPC.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SerialBridgeClientConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_serial_port")]
    pub port: String,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
}

fn default_serial_port() -> String {
    "COM5".to_string()
}

fn default_baud_rate() -> u32 {
    9600
}
```

- [ ] **Step 2: Add SerialBridgeServerEntry struct**

```rust
/// Server-side serial bridge mapping: which virtual COM port to write
/// for a given client's serial data.
#[derive(Debug, Clone, Deserialize)]
pub struct SerialBridgeServerEntry {
    pub client_id: String,
    pub virtual_port: String,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
}
```

- [ ] **Step 3: Add fields to ClientConfig and ServerConfig**

Add to `ClientConfig`:
```rust
    #[serde(default)]
    pub serial_bridge: SerialBridgeClientConfig,
```

Add to `ServerConfig`:
```rust
    #[serde(default)]
    pub serial_bridges: Vec<SerialBridgeServerEntry>,
```

- [ ] **Step 4: Write tests for config parsing**

Add at the bottom of `config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_serial_bridge_client_defaults() {
        let toml_str = r#"
[general]
mode = "client"
[server]
printer_name = "test"
[client]
server_address = "127.0.0.1:50051"
target_printer = "Test"
[jobs]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.client.serial_bridge.enabled);
        assert_eq!(config.client.serial_bridge.port, "COM5");
        assert_eq!(config.client.serial_bridge.baud_rate, 9600);
    }

    #[test]
    fn test_serial_bridge_client_custom() {
        let toml_str = r#"
[general]
mode = "client"
[server]
printer_name = "test"
[client]
server_address = "127.0.0.1:50051"
target_printer = "Test"
[client.serial_bridge]
enabled = true
port = "COM3"
baud_rate = 115200
[jobs]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.client.serial_bridge.enabled);
        assert_eq!(config.client.serial_bridge.port, "COM3");
        assert_eq!(config.client.serial_bridge.baud_rate, 115200);
    }

    #[test]
    fn test_serial_bridge_server_entries() {
        let toml_str = r#"
[general]
mode = "server"
[server]
printer_name = "test"
[client]
server_address = "127.0.0.1:50051"
target_printer = "Test"
[jobs]

[[server.serial_bridges]]
client_id = "pjkeb-client"
virtual_port = "COM20"
baud_rate = 9600
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.serial_bridges.len(), 1);
        assert_eq!(config.server.serial_bridges[0].client_id, "pjkeb-client");
        assert_eq!(config.server.serial_bridges[0].virtual_port, "COM20");
    }
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p devbridge-core -- config::tests
```

Expected: all 3 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/devbridge-core/src/config.rs
git commit -m "Add serial bridge config structs with tests"
```

---

## Task 4: Client Serial Bridge Module

**Files:**
- Create: `crates/devbridge-client/src/serial_bridge.rs`
- Modify: `crates/devbridge-client/Cargo.toml`
- Modify: `crates/devbridge-client/src/lib.rs`

- [ ] **Step 1: Add serialport dependency**

In `crates/devbridge-client/Cargo.toml`, add under `[dependencies]`:
```toml
serialport = "4"
```

- [ ] **Step 2: Create serial_bridge.rs**

```rust
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use devbridge_core::config::SerialBridgeClientConfig;
use devbridge_core::proto::SerialData;

/// Spawn a background task that reads from a local serial port and sends
/// barcode data through the provided mpsc channel. The channel feeds into
/// the gRPC `StreamSerialData` stream in the receiver.
pub fn spawn_reader(
    config: SerialBridgeClientConfig,
    client_id: String,
    tx: mpsc::Sender<SerialData>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(30);

        loop {
            match open_and_read(&config, &client_id, &tx) {
                Ok(()) => {
                    info!(port = %config.port, "serial port closed cleanly");
                    backoff = Duration::from_secs(1);
                }
                Err(e) => {
                    warn!(port = %config.port, error = %e, "serial port error, retrying");
                }
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(max_backoff);
        }
    })
}

fn open_and_read(
    config: &SerialBridgeClientConfig,
    client_id: &str,
    tx: &mpsc::Sender<SerialData>,
) -> Result<(), Box<dyn std::error::Error>> {
    let port = serialport::new(&config.port, config.baud_rate)
        .data_bits(serialport::DataBits::Eight)
        .stop_bits(serialport::StopBits::One)
        .parity(serialport::Parity::None)
        .timeout(Duration::from_secs(5))
        .open()?;

    info!(port = %config.port, baud = config.baud_rate, "serial port opened");

    let mut reader = std::io::BufReader::new(port);
    let mut line = String::new();

    loop {
        line.clear();
        match std::io::BufRead::read_line(&mut reader, &mut line) {
            Ok(0) => return Ok(()), // EOF
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                debug!(barcode = %trimmed, "serial data received");
                let msg = SerialData {
                    client_id: client_id.to_string(),
                    data: line.as_bytes().to_vec(),
                };
                if tx.blocking_send(msg).is_err() {
                    warn!("serial bridge channel closed");
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                continue; // Normal timeout, keep reading
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serial_data_message_construction() {
        let msg = SerialData {
            client_id: "pjkeb-client".to_string(),
            data: b"8588008311011\n".to_vec(),
        };
        assert_eq!(msg.client_id, "pjkeb-client");
        assert_eq!(msg.data, b"8588008311011\n");
    }
}
```

- [ ] **Step 3: Export module in lib.rs**

Add to `crates/devbridge-client/src/lib.rs`:
```rust
pub mod serial_bridge;
```

- [ ] **Step 4: Verify it compiles**

```bash
cargo build -p devbridge-client 2>&1 | tail -5
```

- [ ] **Step 5: Run test**

```bash
cargo test -p devbridge-client -- serial_bridge::tests
```

- [ ] **Step 6: Commit**

```bash
git add crates/devbridge-client/
git commit -m "Add client serial bridge module with COM port reader"
```

---

## Task 5: Server Serial Bridge Module

**Files:**
- Create: `crates/devbridge-server/src/serial_bridge.rs`
- Modify: `crates/devbridge-server/Cargo.toml`
- Modify: `crates/devbridge-server/src/lib.rs`

- [ ] **Step 1: Add serialport dependency**

In `crates/devbridge-server/Cargo.toml`, add under `[dependencies]`:
```toml
serialport = "4"
```

- [ ] **Step 2: Create serial_bridge.rs**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use devbridge_core::config::SerialBridgeServerEntry;

/// Manages virtual COM port writers for serial bridge connections.
/// Each client with a configured serial bridge gets a writer that
/// forwards received barcode data to a com0com virtual port.
pub struct SerialBridgeManager {
    /// Map from client_id to virtual port config
    configs: HashMap<String, SerialBridgeServerEntry>,
    /// Open port handles (lazily opened on first data)
    ports: Arc<RwLock<HashMap<String, Box<dyn serialport::SerialPort>>>>,
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
            ports: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Write serial data for a given client. Opens the virtual port lazily.
    pub async fn write(&self, client_id: &str, data: &[u8]) -> Result<(), String> {
        let config = self
            .configs
            .get(client_id)
            .ok_or_else(|| format!("no serial bridge config for client '{}'", client_id))?;

        // Use spawn_blocking for the actual serial write (blocking I/O)
        let port_name = config.virtual_port.clone();
        let baud = config.baud_rate;
        let data = data.to_vec();
        let ports = Arc::clone(&self.ports);
        let cid = client_id.to_string();

        tokio::task::spawn_blocking(move || {
            let mut ports_guard = ports.blocking_write();

            // Lazily open port
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

            // Write data
            let port = ports_guard.get_mut(&cid).unwrap();
            use std::io::Write;
            port.write_all(&data)
                .map_err(|e| {
                    // Remove broken port so it reopens next time
                    warn!(client = %cid, error = %e, "serial write failed, will reopen");
                    ports_guard.remove(&cid);
                    format!("write error: {}", e)
                })
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
    }

    /// Check if a client has serial bridge configured.
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
```

- [ ] **Step 3: Export module in lib.rs**

Add to `crates/devbridge-server/src/lib.rs`:
```rust
pub mod serial_bridge;
```

- [ ] **Step 4: Verify it compiles and tests pass**

```bash
cargo build -p devbridge-server 2>&1 | tail -5
cargo test -p devbridge-server -- serial_bridge::tests
```

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-server/
git commit -m "Add server serial bridge manager with virtual COM port writer"
```

---

## Task 6: Wire gRPC — Server Side

**Files:**
- Modify: `crates/devbridge-server/src/dispatch.rs`

- [ ] **Step 1: Add SerialBridgeManager to DispatchService**

Add field to `DispatchService` struct:
```rust
    serial_bridge: Arc<serial_bridge::SerialBridgeManager>,
```

Update `DispatchService::new()` to accept and store it. Add parameter:
```rust
    serial_bridge: Arc<serial_bridge::SerialBridgeManager>,
```

- [ ] **Step 2: Add stream type alias**

After the existing `type SubscribeJobsStream` and `type HeartbeatStream`, add:
```rust
    type StreamSerialDataStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<SerialAck, Status>> + Send>>;
```

- [ ] **Step 3: Implement stream_serial_data**

```rust
    async fn stream_serial_data(
        &self,
        request: Request<Streaming<SerialData>>,
    ) -> Result<Response<Self::StreamSerialDataStream>, Status> {
        let mut stream = request.into_inner();
        let serial_bridge = Arc::clone(&self.serial_bridge);
        let (tx, rx) = mpsc::channel(32);

        tokio::spawn(async move {
            while let Some(result) = stream.next().await {
                match result {
                    Ok(data) => {
                        let ok = match serial_bridge.write(&data.client_id, &data.data).await {
                            Ok(()) => {
                                debug!(
                                    client = %data.client_id,
                                    bytes = data.data.len(),
                                    "serial data forwarded to virtual port"
                                );
                                true
                            }
                            Err(e) => {
                                warn!(client = %data.client_id, error = %e, "serial bridge write failed");
                                false
                            }
                        };
                        let _ = tx.send(Ok(SerialAck { ok })).await;
                    }
                    Err(e) => {
                        debug!(error = %e, "serial data stream ended");
                        break;
                    }
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }
```

- [ ] **Step 4: Update all callers of DispatchService::new()**

Search for `DispatchService::new(` in the codebase and add the `serial_bridge` parameter. This includes:
- `crates/devbridge-server/src/dispatch.rs` (test helpers)
- `crates/devbridge-service/src/runtime.rs` (server startup)
- `crates/devbridge-server/tests/grpc_transfer_test.rs`

For test helpers, use `SerialBridgeManager::new(vec![])` (empty config).

- [ ] **Step 5: Verify it compiles**

```bash
cargo build --workspace 2>&1 | tail -10
```

- [ ] **Step 6: Run existing tests to verify no regression**

```bash
cargo test --workspace 2>&1 | tail -20
```

- [ ] **Step 7: Commit**

```bash
git add crates/devbridge-server/ crates/devbridge-service/
git commit -m "Wire StreamSerialData gRPC handler into DispatchService"
```

---

## Task 7: Wire gRPC — Client Side

**Files:**
- Modify: `crates/devbridge-client/src/receiver.rs`

- [ ] **Step 1: Add serial bridge channel and task spawn in run_inner()**

After the existing `ReportStatus` stream setup (around line 142), add:

```rust
        // Spawn serial bridge reader if configured
        let serial_task = if self.serial_bridge_config.enabled {
            let (serial_tx, serial_rx) = tokio::sync::mpsc::channel::<SerialData>(64);
            let serial_stream = ReceiverStream::new(serial_rx);
            let mut serial_client = client.clone();
            let serial_handle = tokio::spawn(async move {
                match serial_client.stream_serial_data(serial_stream).await {
                    Ok(resp) => {
                        let mut acks = resp.into_inner();
                        while let Some(ack) = acks.message().await.unwrap_or(None) {
                            debug!(ok = ack.ok, "serial ack received");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "StreamSerialData RPC ended");
                    }
                }
            });
            let reader_handle = serial_bridge::spawn_reader(
                self.serial_bridge_config.clone(),
                self.machine_id.clone(),
                serial_tx,
            );
            Some((serial_handle, reader_handle))
        } else {
            None
        };
```

- [ ] **Step 2: Add serial_bridge_config field to Receiver**

Add to the `Receiver` struct:
```rust
    serial_bridge_config: SerialBridgeClientConfig,
```

Initialize in `Receiver::new()` from `config.serial_bridge.clone()`.

- [ ] **Step 3: Clean up serial tasks on disconnect**

At the end of `run_inner()`, before returning, abort serial tasks:
```rust
        if let Some((grpc_handle, reader_handle)) = serial_task {
            grpc_handle.abort();
            reader_handle.abort();
        }
```

- [ ] **Step 4: Verify it compiles**

```bash
cargo build --workspace 2>&1 | tail -10
```

- [ ] **Step 5: Run all tests**

```bash
cargo test --workspace
```

- [ ] **Step 6: Commit**

```bash
git add crates/devbridge-client/
git commit -m "Wire serial bridge reader into client receiver gRPC connection"
```

---

## Task 8: Server Startup Wiring

**Files:**
- Modify: `crates/devbridge-service/src/runtime.rs`

- [ ] **Step 1: Pass serial bridge config to DispatchService in run_server()**

In `run_server()`, where `DispatchService::new()` is called, create the manager:

```rust
    let serial_bridge = Arc::new(serial_bridge::SerialBridgeManager::new(
        config.server.serial_bridges.clone(),
    ));
```

Pass it to `DispatchService::new()`.

- [ ] **Step 2: Log serial bridge status at startup**

```rust
    if !config.server.serial_bridges.is_empty() {
        info!(
            count = config.server.serial_bridges.len(),
            "serial bridge mappings configured"
        );
    }
```

- [ ] **Step 3: Verify it compiles and runs**

```bash
cargo build --workspace
cargo test --workspace
```

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-service/
git commit -m "Wire serial bridge manager into server startup"
```

---

## Task 9: Update CLAUDE.md and Commit Config Examples

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add serial bridge documentation to CLAUDE.md**

In the "Workspace Structure" section, update the devbridge-client and devbridge-server descriptions to mention serial bridge. Add a new section:

```markdown
## Serial Bridge (COM Port Forwarding)

DevBridge can forward serial port data (barcode scanners) from client machines
to the server over gRPC. The server writes to a com0com virtual COM port that
ERP applications (Codex) can read.

### Client config
```toml
[client.serial_bridge]
enabled = true
port = "COM5"
baud_rate = 9600
```

### Server config
```toml
[[server.serial_bridges]]
client_id = "pjkeb-client"
virtual_port = "COM20"
baud_rate = 9600
```

Requires com0com driver on the server to create virtual COM port pairs.
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "Document serial bridge feature in CLAUDE.md"
```

---

## Task 10: Format Check, Push, and CI

- [ ] **Step 1: Format check**

```bash
cargo fmt --all --check
```

Fix any issues.

- [ ] **Step 2: Push to dev**

```bash
git push origin dev
```

- [ ] **Step 3: Monitor CI until green**

```bash
gh run list --branch dev --limit 1
```

Wait for all jobs to pass.

---

## Task 11: Deploy and E2E Test on pjkeb

- [ ] **Step 1: Install com0com on pz-server**

Download com0com signed driver, install silently, create port pair:
```powershell
# On pz-server via MCP
setupc.exe install PortName=COM20,EmuBR=yes PortName=COM21,EmuBR=yes
```

- [ ] **Step 2: Update pz-server config.toml**

Add to `C:\ProgramData\DevBridge\config.toml`:
```toml
[[server.serial_bridges]]
client_id = "pjkeb-client"
virtual_port = "COM20"
baud_rate = 9600
```

- [ ] **Step 3: Deploy 0.8.16 to pz-server**

```powershell
irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
```

- [ ] **Step 4: Update pjkeb config.toml**

Add to `C:\ProgramData\DevBridge\config.toml`:
```toml
[client.serial_bridge]
enabled = true
port = "COM5"
baud_rate = 9600
```

- [ ] **Step 5: Deploy 0.8.16 to pjkeb**

```powershell
irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
```

- [ ] **Step 6: E2E test — scan barcode on pjkeb, verify data arrives on COM21 on pz-server**

On pz-server, open COM21 and wait:
```powershell
$sp = New-Object System.IO.Ports.SerialPort 'COM21', 9600
$sp.ReadTimeout = 120000
$sp.Open()
$data = $sp.ReadLine()
echo "RECEIVED: $data"
$sp.Close()
```

Ask employee to scan a barcode on pjkeb. Verify `$data` matches the scanned EAN-13 code.

- [ ] **Step 7: Update Codex ERP config for pjkeb**

Change pjkeb's Codex scanner port to COM21. Verify Codex receives barcodes.

---

## Verification

1. **Config**: serial bridge disabled by default (zero overhead when not configured)
2. **Client**: reads COM5 on pjkeb, reconnects on unplug/replug with exponential backoff
3. **gRPC**: SerialData messages flow from client to server
4. **Server**: writes to COM20 (com0com), data appears on COM21
5. **Codex**: reads COM21, receives barcode data
6. **Existing functionality**: print bridging, dashboard, tray app all unaffected
7. **CI**: all existing tests pass (serial bridge tests use mocks, not real COM ports)
