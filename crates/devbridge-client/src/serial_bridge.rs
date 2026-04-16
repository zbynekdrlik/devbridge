use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use devbridge_core::config::SerialBridgeClientConfig;
use devbridge_core::proto::SerialData;

/// Spawn a background task that reads from a local serial port and sends
/// barcode data through the provided mpsc channel.
#[cfg(windows)]
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

/// Stub for non-Windows platforms (serial bridge is Windows-only).
#[cfg(not(windows))]
pub fn spawn_reader(
    _config: SerialBridgeClientConfig,
    _client_id: String,
    _tx: mpsc::Sender<SerialData>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        warn!("serial bridge not supported on this platform");
    })
}

#[cfg(windows)]
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
            Ok(0) => return Ok(()),
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
}
