//! IPC client for communicating with the DevBridge service.
//!
//! On Windows, connects to `\\.\pipe\devbridge` using named pipes.
//! On macOS/Linux, connects to `/tmp/devbridge.sock` using Unix domain sockets.
//! Both send [`IpcRequest`] messages and receive [`IpcResponse`] messages.

use devbridge_core::ipc::{IpcRequest, IpcResponse};

/// The named pipe path used by the DevBridge service.
#[cfg(target_os = "windows")]
const PIPE_NAME: &str = r"\\.\pipe\devbridge";

/// Send an IPC request to the DevBridge service and return the response.
#[cfg(target_os = "windows")]
pub async fn send_request(
    request: &IpcRequest,
) -> Result<IpcResponse, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;

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

/// Unix domain socket path for the DevBridge service.
#[cfg(not(target_os = "windows"))]
const SOCKET_PATH: &str = "/tmp/devbridge.sock";

/// Send an IPC request to the DevBridge service via Unix domain socket.
#[cfg(not(target_os = "windows"))]
pub async fn send_request(
    request: &IpcRequest,
) -> Result<IpcResponse, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(SOCKET_PATH).await?;

    let payload = serde_json::to_vec(request)?;
    stream.write_all(&payload).await?;
    stream.flush().await?;

    // Signal end of write
    stream.shutdown().await?;

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    buf.truncate(n);

    let response: IpcResponse = serde_json::from_slice(&buf)?;
    Ok(response)
}
