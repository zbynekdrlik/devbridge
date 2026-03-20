//! Integration tests for gRPC job transfer pipeline.
//! These tests start a real gRPC server and client in-process.

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_job_subscribe_and_download() {
        // TODO: Start dispatch server, connect client, subscribe, verify job receipt
    }

    #[tokio::test]
    async fn test_resumable_download() {
        // TODO: Start download, interrupt, resume from offset, verify SHA256
    }
}
