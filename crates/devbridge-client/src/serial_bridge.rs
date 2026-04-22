use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::mpsc;
use tracing::warn;

use devbridge_core::config::SerialBridgeClientConfig;
use devbridge_core::proto::SerialData;

#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(windows)]
use std::time::Duration;
#[cfg(windows)]
use tracing::info;

/// Shutdown signal passed to the reader thread. When set, the blocking
/// `read_line` loop exits at the next timeout (up to 1s later).
pub type ShutdownFlag = Arc<AtomicBool>;

/// Spawn a background task that reads from a local serial port and sends
/// barcode data through the provided mpsc channel. The returned shutdown
/// flag must be set to stop the thread when the owner (e.g., gRPC receiver)
/// reconnects, otherwise zombie threads accumulate and fight for the COM
/// port (see issue: multiple readers after gRPC reconnect held COM5
/// simultaneously, causing "Access is denied" loops).
#[cfg(windows)]
pub fn spawn_reader(
    config: SerialBridgeClientConfig,
    client_id: String,
    tx: mpsc::Sender<SerialData>,
) -> (tokio::task::JoinHandle<()>, ShutdownFlag) {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_inner = Arc::clone(&shutdown);
    let handle = tokio::task::spawn_blocking(move || {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(30);
        let bytes_sent = Arc::new(AtomicU64::new(0));
        let msg_sent = Arc::new(AtomicU64::new(0));

        info!(
            port = %config.port,
            baud = config.baud_rate,
            client_id = %client_id,
            "serial bridge reader started"
        );

        while !shutdown_inner.load(Ordering::Relaxed) {
            match open_and_read(
                &config,
                &client_id,
                &tx,
                &shutdown_inner,
                &bytes_sent,
                &msg_sent,
            ) {
                Ok(()) => {
                    info!(port = %config.port, "serial port closed cleanly");
                    backoff = Duration::from_secs(1);
                }
                Err(e) => {
                    warn!(port = %config.port, error = %e, "serial port error, retrying");
                }
            }
            // Sleep with shutdown check so we exit quickly on signal
            let deadline = std::time::Instant::now() + backoff;
            while std::time::Instant::now() < deadline {
                if shutdown_inner.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            backoff = (backoff * 2).min(max_backoff);
        }

        info!(
            port = %config.port,
            total_msg = msg_sent.load(Ordering::Relaxed),
            total_bytes = bytes_sent.load(Ordering::Relaxed),
            "serial bridge reader stopped"
        );
    });
    (handle, shutdown)
}

/// Stub for non-Windows platforms (serial bridge is Windows-only).
#[cfg(not(windows))]
pub fn spawn_reader(
    _config: SerialBridgeClientConfig,
    _client_id: String,
    _tx: mpsc::Sender<SerialData>,
) -> (tokio::task::JoinHandle<()>, ShutdownFlag) {
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async {
        warn!("serial bridge not supported on this platform");
    });
    (handle, shutdown)
}

#[cfg(windows)]
fn open_and_read(
    config: &SerialBridgeClientConfig,
    client_id: &str,
    tx: &mpsc::Sender<SerialData>,
    shutdown: &ShutdownFlag,
    bytes_sent: &Arc<AtomicU64>,
    msg_sent: &Arc<AtomicU64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut port = serialport::new(&config.port, config.baud_rate)
        .data_bits(serialport::DataBits::Eight)
        .stop_bits(serialport::StopBits::One)
        .parity(serialport::Parity::None)
        // Short timeout lets us check the shutdown flag between reads and
        // flush partial frames promptly. 100ms is small enough that a full
        // scan burst arrives as a single read() but large enough to avoid
        // spinning the CPU.
        .timeout(Duration::from_millis(100))
        .open()?;

    info!(
        port = %config.port,
        baud = config.baud_rate,
        "serial port opened (ready to read barcode data)"
    );

    // Raw byte forwarding: read whatever the OS has buffered and forward it
    // verbatim over gRPC. No delimiter assumption — historically we used
    // BufRead::read_line which silently swallowed all bytes when the scanner
    // emitted CR-only or no terminator (bytes held in BufReader's internal
    // buffer, never flushed, never logged). The server writes these bytes to
    // the paired virtual COM port where the ERP assembles its own frames.
    let mut buf = [0u8; 256];
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        match std::io::Read::read(&mut port, &mut buf) {
            Ok(0) => continue,
            Ok(n) => {
                let preview: String = buf[..n]
                    .iter()
                    .map(|b| {
                        if (32..=126).contains(b) {
                            (*b as char).to_string()
                        } else {
                            format!("\\x{:02x}", b)
                        }
                    })
                    .collect();
                let msg_count = msg_sent.fetch_add(1, Ordering::Relaxed) + 1;
                let byte_count = bytes_sent.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
                info!(
                    preview = %preview,
                    bytes = n,
                    total_msg = msg_count,
                    total_bytes = byte_count,
                    "serial bridge: chunk read from port, forwarding over gRPC"
                );
                let msg = SerialData {
                    client_id: client_id.to_string(),
                    data: buf[..n].to_vec(),
                };
                if tx.blocking_send(msg).is_err() {
                    warn!("serial bridge channel closed (gRPC receiver gone)");
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                continue;
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

    #[test]
    fn test_serial_data_no_terminator_required() {
        // Prior implementation used BufRead::read_line which silently
        // buffered bytes forever when the scanner emitted CR-only or no
        // terminator. The reader now forwards raw chunks, so the wire
        // message must accept arbitrary bytes without requiring \n.
        let msg = SerialData {
            client_id: "pjkeb-client".to_string(),
            data: b"8588008311011\r".to_vec(),
        };
        assert_eq!(msg.data.last(), Some(&b'\r'));
        assert!(!msg.data.contains(&b'\n'));
    }

    #[test]
    fn test_serial_data_binary_bytes() {
        // Some scanners wrap payloads with STX (0x02) / ETX (0x03).
        // Byte forwarding must preserve them, not split on them.
        let data = vec![0x02, b'A', b'B', b'C', 0x03];
        let msg = SerialData {
            client_id: "pjkeb-client".to_string(),
            data: data.clone(),
        };
        assert_eq!(msg.data, data);
    }

    #[test]
    fn test_shutdown_flag_default() {
        use std::sync::atomic::Ordering;
        let f: ShutdownFlag = Arc::new(AtomicBool::new(false));
        assert!(!f.load(Ordering::Relaxed));
        f.store(true, Ordering::Relaxed);
        assert!(f.load(Ordering::Relaxed));
    }
}
