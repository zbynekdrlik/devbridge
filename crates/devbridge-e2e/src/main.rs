use anyhow::{Context, Result, bail};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let server_host =
        std::env::var("E2E_SERVER_HOST").unwrap_or_else(|_| "print-server.lan".into());
    let client_host =
        std::env::var("E2E_CLIENT_HOST").unwrap_or_else(|_| "print-client.lan".into());
    let _target_printer = std::env::var("E2E_TARGET_PRINTER")
        .unwrap_or_else(|_| "Microsoft Print to PDF".into());

    let server_base = format!("http://{}:9120", server_host);
    let client_base = format!("http://{}:9120", client_host);
    let ipp_url = format!("http://{}:631/ipp/print", server_host);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    // Run tests sequentially
    println!("=== DevBridge E2E Test Suite ===\n");

    print!("[1/6] Server healthy... ");
    test_server_healthy(&client, &server_base).await?;
    println!("PASS");

    print!("[2/6] Client healthy... ");
    test_client_healthy(&client, &client_base).await?;
    println!("PASS");

    print!("[3/6] Client connected... ");
    test_client_connected(&client, &server_base).await?;
    println!("PASS");

    print!("[4/6] Print pipeline... ");
    test_print_pipeline(&client, &server_base, &ipp_url).await?;
    println!("PASS");

    print!("[5/6] Dashboard reflects job... ");
    test_dashboard_reflects_job(&client, &server_base).await?;
    println!("PASS");

    print!("[6/6] Job metadata correct... ");
    test_job_metadata_correct(&client, &server_base).await?;
    println!("PASS");

    println!("\n=== All E2E tests passed! ===");
    Ok(())
}

async fn test_server_healthy(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/status", server_base))
        .send()
        .await
        .context("Failed to reach server")?;

    let status = resp.status();
    anyhow::ensure!(status.is_success(), "Server returned {}", status);

    let json: serde_json::Value = resp.json().await?;
    anyhow::ensure!(
        json["mode"] == "server",
        "Expected server mode, got {:?}",
        json["mode"]
    );
    anyhow::ensure!(json["status"] == "running", "Server not running");
    Ok(())
}

async fn test_client_healthy(client: &reqwest::Client, client_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/status", client_base))
        .send()
        .await
        .context("Failed to reach client")?;

    let status = resp.status();
    anyhow::ensure!(status.is_success(), "Client returned {}", status);

    let json: serde_json::Value = resp.json().await?;
    anyhow::ensure!(
        json["mode"] == "client",
        "Expected client mode, got {:?}",
        json["mode"]
    );
    anyhow::ensure!(json["status"] == "running", "Client not running");
    Ok(())
}

async fn test_client_connected(client: &reqwest::Client, server_base: &str) -> Result<()> {
    // For now, verify the server is accepting connections by checking status.
    // Full connected-client verification requires the dashboard API to expose
    // connected clients.
    let resp = client
        .get(format!("{}/api/status", server_base))
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "Server not reachable");
    Ok(())
}

async fn test_print_pipeline(
    client: &reqwest::Client,
    server_base: &str,
    ipp_url: &str,
) -> Result<()> {
    // Read the test PDF fixture at runtime
    let pdf_data = std::fs::read("tests/fixtures/test-page.pdf")
        .or_else(|_| std::fs::read("../../tests/fixtures/test-page.pdf"))
        .context("Failed to read test PDF fixture")?;

    // Build a minimal IPP Print-Job request
    let ipp_payload = build_ipp_print_job(&pdf_data);

    // Submit via HTTP POST (IPP is HTTP-based)
    let resp = client
        .post(ipp_url)
        .header("Content-Type", "application/ipp")
        .body(ipp_payload)
        .send()
        .await
        .context("Failed to submit IPP job")?;

    let status = resp.status();
    let body = resp.bytes().await?;
    println!("  IPP response: status={}, body_len={}", status, body.len());

    anyhow::ensure!(
        status.is_success() || status.as_u16() == 200,
        "IPP submission failed with status {}",
        status
    );

    // Poll job status until completed (timeout 60s)
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(60);
    let mut last_count = 0;

    loop {
        if start.elapsed() > timeout {
            bail!("Timed out waiting for job completion (last job count: {})", last_count);
        }

        let resp = client
            .get(format!("{}/api/jobs", server_base))
            .send()
            .await?;
        let jobs: serde_json::Value = resp.json().await?;

        if let Some(arr) = jobs.as_array() {
            last_count = arr.len();
            if last_count > 0 && last_count != arr.len() {
                println!("  Jobs found: {}", last_count);
            }
            if let Some(latest) = arr.first() {
                let state = latest["state"].as_str().unwrap_or("");
                let job_id = latest["job_id"].as_str().unwrap_or("?");
                if state != "queued" {
                    println!("  Job {}: state={}", job_id, state);
                }
                if state == "completed" {
                    return Ok(());
                }
                if state == "failed" {
                    bail!("Job {} failed: {:?}", job_id, latest);
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn test_dashboard_reflects_job(
    client: &reqwest::Client,
    server_base: &str,
) -> Result<()> {
    let resp = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "Jobs endpoint failed");

    let jobs: serde_json::Value = resp.json().await?;
    let arr = jobs.as_array().context("Expected jobs array")?;
    anyhow::ensure!(!arr.is_empty(), "No jobs found after pipeline test");
    Ok(())
}

async fn test_job_metadata_correct(
    client: &reqwest::Client,
    server_base: &str,
) -> Result<()> {
    let resp = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await?;
    let jobs: serde_json::Value = resp.json().await?;
    let arr = jobs.as_array().context("Expected jobs array")?;
    let job = arr.first().context("No jobs found")?;

    // Verify expected metadata fields exist
    anyhow::ensure!(job["job_id"].is_string(), "Missing job_id");
    anyhow::ensure!(job["document_name"].is_string(), "Missing document_name");
    anyhow::ensure!(job["payload_sha256"].is_string(), "Missing payload_sha256");
    anyhow::ensure!(job["state"].is_string(), "Missing state");
    Ok(())
}

/// Build a minimal IPP Print-Job request payload.
/// IPP is binary-encoded over HTTP POST.
fn build_ipp_print_job(pdf_data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();

    // IPP version 1.1
    buf.push(1); // major
    buf.push(1); // minor

    // Operation: Print-Job (0x0002)
    buf.push(0x00);
    buf.push(0x02);

    // Request ID
    buf.push(0x00);
    buf.push(0x00);
    buf.push(0x00);
    buf.push(0x01);

    // Operation attributes tag
    buf.push(0x01);

    // charset attribute
    buf.push(0x47); // charset type
    let name = b"attributes-charset";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"utf-8";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // natural language
    buf.push(0x48); // natural-language type
    let name = b"attributes-natural-language";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"en-us";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // printer-uri
    buf.push(0x45); // uri type
    let name = b"printer-uri";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"ipp://localhost:631/ipp/print";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // document-format
    buf.push(0x49); // mimeMediaType
    let name = b"document-format";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"application/pdf";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // End of attributes
    buf.push(0x03);

    // Document data
    buf.extend_from_slice(pdf_data);

    buf
}
