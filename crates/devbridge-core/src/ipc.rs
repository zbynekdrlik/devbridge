use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcRequest {
    GetStatus,
    StartService,
    StopService,
    OpenDashboard,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcResponse {
    Status {
        running: bool,
        mode: String,
        jobs_queued: u32,
        connected_clients: u32,
    },
    Ok,
    Error {
        message: String,
    },
}
