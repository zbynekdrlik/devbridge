//! Named pipe IPC client for communicating with the DevBridge Windows service.
//!
//! On Windows, connects to `\\.\pipe\devbridge` to send [`IpcRequest`] messages
//! and receive [`IpcResponse`] messages. On non-Windows platforms, operations
//! are logged but no actual connection is made.

use devbridge_core::ipc::{IpcRequest, IpcResponse};

/// The named pipe path used by the DevBridge service.
#[cfg(target_os = "windows")]
const PIPE_NAME: &str = r"\\.\pipe\devbridge";

/// Send an IPC request to the DevBridge service and return the response.
#[cfg(target_os = "windows")]
pub async fn send_request(request: &IpcRequest) -> Result<IpcResponse, Box<dyn std::error::Error>> {
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut client = ClientOptions::new().open(PIPE_NAME)?;

    let payload = serde_json::to_vec(request)?;
    client.write_all(&payload).await?;
    client.flush().await?;

    let mut buf = vec![0u8; 4096];
    let n = client.read(&mut buf).await?;
    buf.truncate(n);

    let response: IpcResponse = serde_json::from_slice(&buf)?;
    Ok(response)
}

/// Placeholder for non-Windows platforms. Logs the request and returns an error response.
#[cfg(not(target_os = "windows"))]
pub async fn send_request(request: &IpcRequest) -> Result<IpcResponse, Box<dyn std::error::Error>> {
    tracing::warn!(
        "IPC not available on this platform. Request: {:?}",
        request
    );
    Ok(IpcResponse::Error {
        message: "Named pipe IPC is only available on Windows".to_string(),
    })
}
