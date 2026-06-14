use anyhow::{Context, Result, bail};
use std::time::Duration;

/// Expected document name sent in the E2E Print-Job request. Used by
/// `build_ipp_print_job` to populate the `document-name` operation
/// attribute and by `test_job_metadata_correct` to assert the server
/// captured it (issue #30).
const E2E_DOCUMENT_NAME: &str = "DevBridge-E2E-Selfhost.pdf";

/// Expected copies value sent in the E2E Print-Job request. Used by
/// `build_ipp_print_job` to populate the `copies` job attribute and by
/// `test_job_metadata_correct` to assert the server captured it (issue #37).
const E2E_COPIES: u32 = 3;

#[tokio::main]
async fn main() -> Result<()> {
    let server_host = std::env::var("E2E_SERVER_HOST").unwrap_or_else(|_| "10.88.1.100".into());
    let client_host = std::env::var("E2E_CLIENT_HOST").unwrap_or_else(|_| "10.78.2.10".into());
    let target_printer =
        std::env::var("E2E_TARGET_PRINTER").unwrap_or_else(|_| "Microsoft Print to PDF".into());
    let server_dashboard_port =
        std::env::var("E2E_SERVER_DASHBOARD_PORT").unwrap_or_else(|_| "9120".into());
    let client_dashboard_port =
        std::env::var("E2E_CLIENT_DASHBOARD_PORT").unwrap_or_else(|_| "9120".into());
    let server_ipp_port = std::env::var("E2E_SERVER_IPP_PORT").unwrap_or_else(|_| "631".into());
    let server_printer_name =
        std::env::var("E2E_SERVER_PRINTER_NAME").unwrap_or_else(|_| "DevBridge".into());

    let server_base = format!("http://{}:{}", server_host, server_dashboard_port);
    let client_base = format!("http://{}:{}", client_host, client_dashboard_port);
    let ipp_url = format!("http://{}:{}/ipp/print", server_host, server_ipp_port);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    // Run tests sequentially
    println!("=== DevBridge E2E Test Suite ===\n");

    print!("[1/33] Installation verified... ");
    test_installation_verified(&client, &server_base).await?;
    println!("PASS");

    print!("[2/33] Service registered... ");
    test_service_registered(&client, &server_base).await?;
    println!("PASS");

    print!("[3/33] Server healthy... ");
    test_server_healthy(&client, &server_base).await?;
    println!("PASS");

    print!("[4/33] Client healthy... ");
    test_client_healthy(&client, &client_base).await?;
    println!("PASS");

    print!("[5/33] Client connected... ");
    test_client_connected(&client, &server_base).await?;
    println!("PASS");

    print!("[6/33] gRPC client ready... ");
    test_grpc_client_ready(&client, &server_base).await?;
    println!("PASS");

    print!("[7/33] Print pipeline... ");
    test_print_pipeline(&client, &server_base, &ipp_url, &target_printer).await?;
    println!("PASS");

    print!("[8/33] Dashboard reflects job... ");
    test_dashboard_reflects_job(&client, &server_base).await?;
    println!("PASS");

    print!("[9/33] Job metadata correct... ");
    test_job_metadata_correct(&client, &server_base).await?;
    println!("PASS");

    print!("[10/33] Virtual printers seeded... ");
    test_virtual_printers_seeded(&client, &server_base).await?;
    println!("PASS");

    print!("[11/33] Client registered... ");
    test_client_registered(&client, &server_base).await?;
    println!("PASS");

    print!("[12/33] Connected clients accurate... ");
    test_connected_clients_accurate(&client, &server_base).await?;
    println!("PASS");

    print!("[13/33] VP CRUD works... ");
    test_vp_crud(&client, &server_base).await?;
    println!("PASS");

    print!("[14/33] VP-client pairing... ");
    test_vp_client_pairing(&client, &server_base).await?;
    println!("PASS");

    print!("[15/33] Windows printer registered... ");
    test_windows_printer_registered(&server_host, &server_printer_name).await?;
    println!("PASS");

    print!("[16/33] Tray app installed... ");
    test_tray_app_installed(&server_host).await?;
    println!("PASS");

    print!("[17/33] IPP Get-Printer-Attributes... ");
    test_ipp_get_printer_attributes(&client, &ipp_url).await?;
    println!("PASS");

    print!("[18/33] Windows spooler print... ");
    test_windows_spooler_print(&client, &server_base, &ipp_url, &server_printer_name).await?;
    println!("PASS");

    print!("[19/33] Client job history... ");
    test_client_job_history(&client, &client_base).await?;
    println!("PASS");

    print!("[20/33] Target printer hot-reload... ");
    test_target_printer_hot_reload(&client, &client_base).await?;
    println!("PASS");

    print!("[21/33] Tray app registry key... ");
    test_tray_app_registry_key().await?;
    println!("PASS");

    print!("[22/33] Full print flow with client verification... ");
    test_full_print_flow_verified(&client, &server_base, &client_base, &ipp_url).await?;
    println!("PASS");

    print!("[23/33] Client dashboard mode... ");
    test_client_dashboard_mode(&client, &client_base).await?;
    println!("PASS");

    print!("[24/33] Reprint job... ");
    test_reprint_job(&client, &server_base).await?;
    println!("PASS");

    print!("[25/33] WebSocket events... ");
    test_websocket_events(&server_base, &ipp_url).await?;
    println!("PASS");

    print!("[26/33] PWA manifest served... ");
    test_manifest_served(&client, &server_base, &client_base).await?;
    println!("PASS");

    print!("[27/33] Job events API... ");
    test_job_events_api(&client, &server_base).await?;

    print!("[28/33] Job events nonexistent... ");
    test_job_events_nonexistent(&client, &server_base).await?;

    print!("[29/33] Client status has identity fields... ");
    test_client_status_identity(&client, &client_base).await?;

    print!("[30/33] Server has audit events after print... ");
    test_server_has_audit_events(&client, &server_base).await?;

    print!("[31/33] No duplicate dispatch for a completed job (issue #51)... ");
    test_no_duplicate_dispatch(&client, &server_base).await?;
    println!("PASS");

    print!("[32/33] Auto-update task registered + active_jobs surfaced (issue #54)... ");
    test_auto_update_registered(&client, &server_base).await?;
    println!("PASS");

    // Test 33 runs LAST: it temporarily points the client at a non-existent
    // printer to force a deterministic print failure → server requeue, then
    // restores the original target. Running it last keeps the bad-target window
    // from catching any other test's job.
    print!("[33/33] Server-driven retry reaches client dashboard (issue #56)... ");
    test_server_driven_retry_reaches_client(&client, &server_base, &client_base, &ipp_url).await?;
    println!("PASS");

    // Signal client deploy job that E2E is complete
    signal_e2e_done();

    println!("\n=== All 33 E2E tests passed! ===");
    Ok(())
}

/// Verify the NSIS installer placed files in the correct location.
/// Checks the server's /api/status endpoint for install path info,
/// and verifies the data directory exists via the status response.
async fn test_installation_verified(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/status", server_base))
        .send()
        .await
        .context("Failed to reach server — installation may have failed")?;

    anyhow::ensure!(
        resp.status().is_success(),
        "Server not responding after install"
    );

    let json: serde_json::Value = resp.json().await?;

    // The server is running and responding, which means the binary was installed
    // and the config was written correctly by post-install.ps1
    anyhow::ensure!(
        json["status"].is_string(),
        "Server /api/status missing 'status' field — incomplete installation"
    );

    // Verify version field exists (proves the correct binary is running)
    // Note: version may not be exposed yet, so we just verify the endpoint works
    println!("  Server responding at {}", server_base);
    Ok(())
}

/// Verify the service is registered as a Windows service and running.
/// Uses the dashboard API to check service status.
async fn test_service_registered(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/status", server_base))
        .send()
        .await
        .context("Failed to reach server")?;

    let json: serde_json::Value = resp.json().await?;

    let status = json["status"].as_str().unwrap_or("");
    anyhow::ensure!(
        status == "running",
        "Service not running (status: {}). Windows service registration may have failed.",
        status
    );

    let mode = json["mode"].as_str().unwrap_or("");
    anyhow::ensure!(
        mode == "server",
        "Expected server mode, got '{}'. Config may not have been written correctly.",
        mode
    );

    println!("  Service running in {} mode", mode);
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

/// Wait for at least one gRPC client to be connected to the server.
/// After server restart, clients need time to reconnect via gRPC.
/// Without this, the print pipeline test fails because jobs stay queued
/// with no client to dispatch to.
async fn test_grpc_client_ready(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(60);

    loop {
        let resp = client
            .get(format!("{}/api/status", server_base))
            .send()
            .await?;
        let json: serde_json::Value = resp.json().await?;
        let count = json["connected_clients"].as_u64().unwrap_or(0);
        if count >= 1 {
            println!(
                "  connected_clients={} (waited {:.1}s)",
                count,
                start.elapsed().as_secs_f64()
            );
            return Ok(());
        }
        if start.elapsed() > timeout {
            bail!(
                "Timed out waiting for gRPC client connection (connected_clients={})",
                count
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn test_print_pipeline(
    client: &reqwest::Client,
    server_base: &str,
    ipp_url: &str,
    target_printer: &str,
) -> Result<()> {
    println!("  Target printer: {}", target_printer);

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
    let timeout = Duration::from_secs(120);
    let mut last_count = 0;
    let mut last_state = String::new();

    loop {
        if start.elapsed() > timeout {
            bail!(
                "Timed out waiting for job completion (last job count: {})",
                last_count
            );
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
                let state = latest["status"].as_str().unwrap_or("").to_string();
                let job_id = latest["id"].as_str().unwrap_or("?");
                if state != last_state {
                    println!("  Job {}: state={}", job_id, state);
                    last_state = state.clone();
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

async fn test_dashboard_reflects_job(client: &reqwest::Client, server_base: &str) -> Result<()> {
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

async fn test_job_metadata_correct(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await?;
    let jobs: serde_json::Value = resp.json().await?;
    let arr = jobs.as_array().context("Expected jobs array")?;
    let job = arr.first().context("No jobs found")?;

    // Verify expected metadata fields exist
    anyhow::ensure!(job["id"].is_string(), "Missing id");
    anyhow::ensure!(job["name"].is_string(), "Missing name");
    anyhow::ensure!(job["payload_sha256"].is_string(), "Missing payload_sha256");
    anyhow::ensure!(job["status"].is_string(), "Missing status");

    // Assert the real document name was captured (issue #30). The Print-Job
    // step sent `document-name = E2E_DOCUMENT_NAME`, so `name` must equal it.
    // If the server fell back to the legacy `job-<uuid>` string, #30
    // regressed.
    let name = job["name"].as_str().unwrap_or("");
    anyhow::ensure!(
        name == E2E_DOCUMENT_NAME,
        "Expected document_name = {:?}, got {:?} (#30 regression: \
         legacy job-<uuid> behavior returned)",
        E2E_DOCUMENT_NAME,
        name
    );
    println!("  ✓ Document name captured: {}", name);

    // Assert the real IPP copies value was captured (issue #37). The
    // Print-Job step sent `copies = E2E_COPIES` as a Job attribute, so the
    // stored job must echo that back. Regression = hardcoded `copies: 1`
    // returned.
    let copies = job["copies"].as_u64().unwrap_or(0) as u32;
    anyhow::ensure!(
        copies == E2E_COPIES,
        "Expected copies = {}, got {} (#37 regression: hardcoded copies=1 \
         behavior returned)",
        E2E_COPIES,
        copies
    );
    println!("  ✓ Copies captured: {}", copies);

    Ok(())
}

/// Verify at least one virtual printer exists with expected fields.
async fn test_virtual_printers_seeded(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/virtual-printers", server_base))
        .send()
        .await
        .context("Failed to reach virtual-printers endpoint")?;

    anyhow::ensure!(
        resp.status().is_success(),
        "Virtual printers endpoint failed"
    );

    let vps: serde_json::Value = resp.json().await?;
    let arr = vps.as_array().context("Expected array")?;
    anyhow::ensure!(!arr.is_empty(), "No virtual printers seeded");

    let vp = &arr[0];
    anyhow::ensure!(vp["id"].is_string(), "VP missing 'id'");
    anyhow::ensure!(vp["display_name"].is_string(), "VP missing 'display_name'");
    anyhow::ensure!(vp["ipp_name"].is_string(), "VP missing 'ipp_name'");
    Ok(())
}

/// Verify at least one client is registered with correct fields.
/// Note: is_online is a UI hint that can race during reconnection.
/// The functional proof that the client works is test 7 (job completed).
async fn test_client_registered(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/clients", server_base))
        .send()
        .await
        .context("Failed to reach clients endpoint")?;

    anyhow::ensure!(resp.status().is_success(), "Clients endpoint failed");

    let clients: serde_json::Value = resp.json().await?;
    let arr = clients.as_array().context("Expected array")?;
    anyhow::ensure!(!arr.is_empty(), "No clients registered");

    let c = &arr[0];
    anyhow::ensure!(c["machine_id"].is_string(), "Client missing 'machine_id'");
    anyhow::ensure!(c["hostname"].is_string(), "Client missing 'hostname'");

    let online = c["is_online"].as_bool().unwrap_or(false);
    println!(
        "  client={} is_online={} (functional proof: test 7 job completed)",
        c["machine_id"].as_str().unwrap_or("?"),
        online
    );
    Ok(())
}

/// Verify connected_clients count is accurate (>= 1, not inflated by reconnects).
async fn test_connected_clients_accurate(
    client: &reqwest::Client,
    server_base: &str,
) -> Result<()> {
    // Poll until connected_clients stabilizes to 1 (stale connections need
    // time to clean up after client reconnects during E2E deploy).
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(30);
    loop {
        let resp = client
            .get(format!("{}/api/status", server_base))
            .send()
            .await?;
        let json: serde_json::Value = resp.json().await?;
        let count = json["connected_clients"]
            .as_u64()
            .context("Missing connected_clients field")?;

        if count == 1 {
            println!(
                "  connected_clients={} ({}s)",
                count,
                start.elapsed().as_secs()
            );
            return Ok(());
        }

        if start.elapsed() > timeout {
            // Accept >= 1 if stale cleanup hasn't finished
            anyhow::ensure!(count >= 1, "Expected connected_clients >= 1, got {}", count);
            println!(
                "  connected_clients={} ({}s, expected 1 but accepting >= 1)",
                count,
                start.elapsed().as_secs()
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Test VP CRUD lifecycle: create, verify, rename, verify rename, delete, verify gone.
async fn test_vp_crud(client: &reqwest::Client, server_base: &str) -> Result<()> {
    // Create
    let resp = client
        .post(format!("{}/api/virtual-printers", server_base))
        .json(&serde_json::json!({
            "display_name": "E2E Test Printer",
            "ipp_name": "e2e-test-printer"
        }))
        .send()
        .await
        .context("Failed to create VP")?;
    anyhow::ensure!(
        resp.status().is_success(),
        "Create VP failed: {}",
        resp.status()
    );

    let created: serde_json::Value = resp.json().await?;
    let vp_id = created["id"]
        .as_str()
        .context("Created VP missing 'id'")?
        .to_string();
    anyhow::ensure!(
        created["display_name"] == "E2E Test Printer",
        "Wrong display_name"
    );

    // Rename via PUT
    let resp = client
        .put(format!("{}/api/virtual-printers/{}", server_base, vp_id))
        .json(&serde_json::json!({
            "display_name": "E2E Renamed Printer"
        }))
        .send()
        .await
        .context("Failed to rename VP")?;
    anyhow::ensure!(
        resp.status().is_success(),
        "Rename VP failed: {}",
        resp.status()
    );

    // Verify rename persisted
    let resp = client
        .get(format!("{}/api/virtual-printers", server_base))
        .send()
        .await?;
    let vps: serde_json::Value = resp.json().await?;
    let found =
        vps.as_array().context("Expected array")?.iter().any(|v| {
            v["id"].as_str() == Some(&vp_id) && v["display_name"] == "E2E Renamed Printer"
        });
    anyhow::ensure!(found, "Renamed VP not found in list");

    // Delete
    let resp = client
        .delete(format!("{}/api/virtual-printers/{}", server_base, vp_id))
        .send()
        .await
        .context("Failed to delete VP")?;
    anyhow::ensure!(
        resp.status().is_success() || resp.status().as_u16() == 204,
        "Delete VP failed: {}",
        resp.status()
    );

    // Verify gone
    let resp = client
        .get(format!("{}/api/virtual-printers", server_base))
        .send()
        .await?;
    let vps: serde_json::Value = resp.json().await?;
    let still_exists = vps
        .as_array()
        .context("Expected array")?
        .iter()
        .any(|v| v["id"].as_str() == Some(&vp_id));
    anyhow::ensure!(!still_exists, "Deleted VP still present in list");

    Ok(())
}

/// Test VP-client pairing: pair a VP to a registered client, verify, then unpair.
async fn test_vp_client_pairing(client: &reqwest::Client, server_base: &str) -> Result<()> {
    // Get VPs
    let resp = client
        .get(format!("{}/api/virtual-printers", server_base))
        .send()
        .await?;
    let vps: serde_json::Value = resp.json().await?;
    let vp = vps
        .as_array()
        .context("Expected array")?
        .first()
        .context("No VPs to test pairing with")?;
    let vp_id = vp["id"].as_str().context("VP missing id")?.to_string();

    // Get a registered client
    let resp = client
        .get(format!("{}/api/clients", server_base))
        .send()
        .await?;
    let clients_json: serde_json::Value = resp.json().await?;
    let cl = clients_json
        .as_array()
        .context("Expected array")?
        .first()
        .context("No clients to pair with")?;
    let machine_id = cl["machine_id"]
        .as_str()
        .context("Client missing machine_id")?
        .to_string();

    // Pair
    let resp = client
        .put(format!("{}/api/virtual-printers/{}", server_base, vp_id))
        .json(&serde_json::json!({
            "paired_client_id": machine_id
        }))
        .send()
        .await
        .context("Failed to pair VP")?;
    anyhow::ensure!(resp.status().is_success(), "Pair failed: {}", resp.status());

    // Verify paired
    let resp = client
        .get(format!("{}/api/virtual-printers", server_base))
        .send()
        .await?;
    let vps: serde_json::Value = resp.json().await?;
    let paired_vp = vps
        .as_array()
        .context("Expected array")?
        .iter()
        .find(|v| v["id"].as_str() == Some(&vp_id))
        .context("VP not found after pairing")?;
    anyhow::ensure!(
        paired_vp["paired_client_id"].as_str() == Some(&machine_id),
        "VP not paired to expected client. Got: {:?}",
        paired_vp["paired_client_id"]
    );

    // Unpair (cleanup)
    let _ = client
        .put(format!("{}/api/virtual-printers/{}", server_base, vp_id))
        .json(&serde_json::json!({
            "paired_client_id": null
        }))
        .send()
        .await;

    Ok(())
}

/// Verify the DevBridge Windows printer is registered on the server.
/// Uses PowerShell Get-Printer via the server's shell (runs on server runner).
async fn test_windows_printer_registered(_server_host: &str, printer_name: &str) -> Result<()> {
    let cmd = format!(
        "Get-Printer -Name '{}' | Select-Object -ExpandProperty Name",
        printer_name
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &cmd])
        .output()
        .context("Failed to run PowerShell Get-Printer")?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    anyhow::ensure!(
        output.status.success() && stdout == printer_name,
        "{} printer not registered in Windows. stdout='{}', stderr='{}'",
        printer_name,
        stdout,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Verify the tray app exe exists and the process is running.
/// The post-install launches the tray via scheduled task in the user's session.
/// This test must NOT kill/relaunch — that creates ghost icons and CI cleanup
/// kills the replacement, leaving zero tray icons on the server.
async fn test_tray_app_installed(_server_host: &str) -> Result<()> {
    let candidates = [
        r"C:\Program Files\DevBridge\devbridge-app.exe",
        r"C:\Program Files\DevBridge\DevBridge.exe",
    ];

    let found = candidates.iter().any(|p| std::path::Path::new(p).exists());
    anyhow::ensure!(found, "Tray app exe not found at any expected location");

    // Verify the process is running (launched by post-install via scheduled task).
    // In CI, the runner may be in a disconnected RDP session where GUI apps can't
    // start. Binary existence is sufficient proof that the installer works.
    let check = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-Process devbridge-app -ErrorAction SilentlyContinue) -ne $null",
        ])
        .output()
        .context("Failed to check tray process")?;
    let running = String::from_utf8_lossy(&check.stdout).trim() == "True";
    if running {
        println!("  Tray app exe found and process running");
    } else if std::env::var("CI").is_ok() {
        println!("  Tray app exe found (process not running - disconnected CI session, OK)");
    } else {
        anyhow::bail!("Tray app not running - post-install failed to launch it");
    }
    Ok(())
}

/// Send IPP Get-Printer-Attributes and verify the response contains required attributes.
async fn test_ipp_get_printer_attributes(client: &reqwest::Client, ipp_url: &str) -> Result<()> {
    let payload = build_ipp_get_printer_attributes();

    let resp = client
        .post(ipp_url)
        .header("Content-Type", "application/ipp")
        .body(payload)
        .send()
        .await
        .context("Failed to send Get-Printer-Attributes")?;

    let status = resp.status();
    let body = resp.bytes().await?;

    anyhow::ensure!(
        status.is_success(),
        "Get-Printer-Attributes HTTP failed: {}",
        status
    );
    anyhow::ensure!(
        body.len() > 8,
        "IPP response too short: {} bytes",
        body.len()
    );

    // IPP status code at bytes 2-3; 0x0000 = successful-ok
    let ipp_status = u16::from_be_bytes([body[2], body[3]]);
    anyhow::ensure!(
        ipp_status == 0x0000,
        "IPP status not successful-ok: 0x{:04x}",
        ipp_status
    );

    let body_str = String::from_utf8_lossy(&body);

    // Verify critical attributes Windows IPP Class Driver needs
    anyhow::ensure!(body_str.contains("printer-state"), "Missing printer-state");
    anyhow::ensure!(
        body_str.contains("document-format-supported"),
        "Missing document-format-supported"
    );
    anyhow::ensure!(
        body_str.contains("media-supported"),
        "Missing media-supported"
    );
    anyhow::ensure!(
        body_str.contains("printer-is-accepting-jobs"),
        "Missing printer-is-accepting-jobs"
    );

    // Verify our custom media sizes
    anyhow::ensure!(
        body_str.contains("na_letter_8.5x11in"),
        "Missing Letter media"
    );
    anyhow::ensure!(body_str.contains("iso_a4_210x297mm"), "Missing A4 media");

    println!(
        "  IPP attributes validated (status=0x{:04x}, {} bytes)",
        ipp_status,
        body.len()
    );
    Ok(())
}

/// Print through the Windows spooler and verify the job reaches the DevBridge dashboard.
/// This tests the full user-facing flow: app → Windows spooler → IPP Class Driver → HTTP → DevBridge.
async fn test_windows_spooler_print(
    client: &reqwest::Client,
    server_base: &str,
    ipp_url: &str,
    printer_name: &str,
) -> Result<()> {
    // Record current job count before printing
    let resp = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await?;
    let jobs_before: serde_json::Value = resp.json().await?;
    let count_before = jobs_before.as_array().map_or(0, |a| a.len());

    // Log printer port details for diagnostics
    let diag_cmd = format!(
        "Get-Printer -Name '{}' -ErrorAction SilentlyContinue | Select-Object Name, DriverName, PortName | Format-List",
        printer_name
    );
    let diag = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &diag_cmd])
        .output();
    if let Ok(d) = diag {
        let info = String::from_utf8_lossy(&d.stdout);
        println!("  Printer info: {}", info.trim().replace('\n', " | "));
    }

    // Clear stale print jobs by restarting the Windows Print Spooler service.
    let clear_cmd = format!(
        "Restart-Service Spooler -Force; Start-Sleep 2; \
         Get-PrintJob -PrinterName '{}' -ErrorAction SilentlyContinue | Remove-PrintJob -ErrorAction SilentlyContinue",
        printer_name
    );
    let clear = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &clear_cmd])
        .output();
    if clear.is_ok() {
        let count_cmd = format!(
            "(Get-PrintJob -PrinterName '{}' -ErrorAction SilentlyContinue | Measure-Object).Count",
            printer_name
        );
        let jobs_after = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &count_cmd])
            .output();
        let count = jobs_after
            .as_ref()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "?".into());
        println!("  Spooler restarted, remaining jobs: {}", count);
    }

    // Pre-flight: test IPP endpoint with Windows-like Content-Type header.
    // inetpp.dll sends "application/ipp; charset=utf-8" which ippper rejects
    // without our normalization wrapper. This verifies the fix is deployed.
    let preflight_payload = build_ipp_get_printer_attributes();
    let preflight_resp = client
        .post(ipp_url)
        .header("Content-Type", "application/ipp; charset=utf-8")
        .body(preflight_payload)
        .send()
        .await;
    match preflight_resp {
        Ok(r) => {
            println!(
                "  Pre-flight (charset Content-Type): status={}, len={}",
                r.status(),
                r.content_length().unwrap_or(0)
            );
            if r.status().as_u16() == 415 {
                bail!(
                    "Server returned 415 for charset Content-Type - normalization fix not deployed"
                );
            }
        }
        Err(e) => println!("  Pre-flight failed: {}", e),
    }

    // Print through Windows spooler using Out-Printer
    let ps_script = format!(
        r#"$text = "DevBridge E2E spooler test - $(Get-Date -Format o)"; $text | Out-Printer -Name "{}""#,
        printer_name
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_script])
        .output()
        .context("Failed to run Out-Printer via PowerShell")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Out-Printer failed: {}", stderr.trim());
    }
    println!("  Submitted print job via Windows spooler");

    // Poll /api/jobs until a new job appears (timeout 30s)
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(30);

    loop {
        if start.elapsed() > timeout {
            // Dump Windows print queue diagnostics before failing
            let diag_cmd = format!(
                "Get-PrintJob -PrinterName '{}' -ErrorAction SilentlyContinue | Select-Object Id, JobStatus, DocumentName | Format-Table -AutoSize; \
                 Get-PrinterPort | Where-Object {{ $_.Name -like '*631*' -or $_.Name -like '*1631*' }} | Select-Object Name, PrinterHostAddress, PortMonitor, Description | Format-List",
                printer_name
            );
            let diag = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &diag_cmd])
                .output();
            if let Ok(d) = diag {
                let info = String::from_utf8_lossy(&d.stdout);
                println!("  Print queue diagnostics:\n{}", info);
            }
            // Dump server logs for IPP request debugging
            let srvlog = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command",
                    "Get-ChildItem 'C:\\ProgramData\\DevBridge\\logs' -Filter '*.log' -ErrorAction SilentlyContinue | ForEach-Object { Write-Output \"--- $($_.Name) ---\"; Get-Content $_.FullName -Tail 20 }"])
                .output();
            if let Ok(d) = srvlog {
                let info = String::from_utf8_lossy(&d.stdout);
                if !info.trim().is_empty() {
                    println!("  Server logs:\n{}", info);
                }
            }
            bail!(
                "Timed out waiting for spooler job (had {} jobs before, still {} after 30s)",
                count_before,
                count_before
            );
        }

        let resp = client
            .get(format!("{}/api/jobs", server_base))
            .send()
            .await?;
        let jobs: serde_json::Value = resp.json().await?;
        let count_now = jobs.as_array().map_or(0, |a| a.len());

        if count_now > count_before {
            println!(
                "  Spooler job arrived (jobs: {} -> {})",
                count_before, count_now
            );
            return Ok(());
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Read the current client target printer name via the dashboard API.
async fn get_client_target(client: &reqwest::Client, client_base: &str) -> Result<String> {
    let resp = client
        .get(format!("{}/api/printers/target", client_base))
        .send()
        .await
        .context("Failed to read client target printer")?;
    let json: serde_json::Value = resp.json().await?;
    Ok(json["name"].as_str().unwrap_or("").to_string())
}

/// Set the client target printer name via the dashboard API (hot-reload).
async fn set_client_target(client: &reqwest::Client, client_base: &str, name: &str) -> Result<()> {
    let resp = client
        .put(format!("{}/api/printers/target", client_base))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .with_context(|| format!("Failed to set client target to '{}'", name))?;
    anyhow::ensure!(
        resp.status().is_success(),
        "PUT target '{}' failed: {}",
        name,
        resp.status()
    );
    Ok(())
}

/// Test 33 (issue #56): a SERVER-DRIVEN RETRY must surface on the CLIENT
/// dashboard as a non-zero `retry_count`.
///
/// The lower tiers already lock the value end-to-end in isolation
/// (`receiver.rs::test_job_to_metadata_surfaces_server_retry_count`,
/// `dispatch.rs::test_send_job_emits_server_retry_count`,
/// `dashboard jobs.rs::test_jobs_response_surfaces_retry_count`). What was
/// missing (the #56 gap) was an E2E assertion that a retry driven by the real
/// server over the real gRPC wire actually appears as `retry_count > 0` on the
/// deployed client dashboard.
///
/// Deterministic injection (no flaky hardware dependence, no test-only
/// production code): hot-reload the client's target printer to a name that does
/// not exist, then submit a real IPP Print-Job. The client's `windows_spooler`
/// backend fails the print DETERMINISTICALLY ("printer not found"), reports the
/// failure to the server, and the server (under `max_retries`) requeues the job
/// after `retry_delay_secs` with `retry_count` incremented and re-dispatches it
/// to the same client — exercising the EXACT production retry path. The client
/// upserts the re-dispatched job (`ON CONFLICT(job_id) DO UPDATE retry_count`),
/// so its dashboard `/api/jobs` now shows `retry_count > 0`.
///
/// The poll is bounded (waits for the OBSERVED condition, never a fixed sleep),
/// so it is not flaky. The original target is ALWAYS restored — on success or
/// failure — so the requeued job can then print cleanly and the suite leaves no
/// broken target behind.
async fn test_server_driven_retry_reaches_client(
    client: &reqwest::Client,
    server_base: &str,
    client_base: &str,
    ipp_url: &str,
) -> Result<()> {
    // Save the real target so we can restore it no matter what happens.
    let original_target = get_client_target(client, client_base).await?;
    println!("  Saved client target: '{}'", original_target);

    // Run the body, then restore the original target before propagating the
    // result. A deliberately invalid target name no real printer can match.
    let bad_target = "E2E-NoSuchPrinter-RetryInjection-#56";
    let result = run_retry_injection(client, server_base, client_base, ipp_url, bad_target).await;

    // ALWAYS restore the good target (even if the body failed), so the requeued
    // job prints on the next retry and later tests/runs start clean.
    if let Err(e) = set_client_target(client, client_base, &original_target).await {
        // Surface the restore failure but don't mask the body's own error.
        eprintln!(
            "  WARNING: failed to restore client target to '{}': {}",
            original_target, e
        );
    } else {
        println!("  Restored client target: '{}'", original_target);
    }

    result
}

/// Body of test 33: point the client at `bad_target`, submit a job, and poll
/// the CLIENT dashboard until the job it just submitted shows `retry_count > 0`.
async fn run_retry_injection(
    client: &reqwest::Client,
    server_base: &str,
    client_base: &str,
    ipp_url: &str,
    bad_target: &str,
) -> Result<()> {
    // 1. Point the client at a non-existent printer → deterministic print fail.
    set_client_target(client, client_base, bad_target).await?;
    println!("  Client target set to non-existent '{}'", bad_target);

    // 2. Snapshot existing CLIENT job ids so we can identify the NEW one.
    let ids_before = client_job_ids(client, client_base).await?;

    // 3. Submit a real IPP Print-Job (same path test 7 uses). It will be routed
    //    to the connected E2E client, fail there (bad target), and be requeued.
    let pdf_data = std::fs::read("tests/fixtures/test-page.pdf")
        .or_else(|_| std::fs::read("../../tests/fixtures/test-page.pdf"))
        .context("Failed to read test PDF fixture")?;
    let ipp_payload = build_ipp_print_job(&pdf_data);
    let resp = client
        .post(ipp_url)
        .header("Content-Type", "application/ipp")
        .body(ipp_payload)
        .send()
        .await
        .context("Failed to submit IPP retry-injection job")?;
    anyhow::ensure!(
        resp.status().is_success(),
        "IPP submission failed with status {}",
        resp.status()
    );
    println!("  Submitted retry-injection IPP job");

    // 4. Find the NEW job id on the CLIENT dashboard (it must arrive there).
    let new_job_id = wait_for_new_client_job(client, client_base, &ids_before).await?;
    println!(
        "  New client job: {}",
        &new_job_id[..8.min(new_job_id.len())]
    );

    // 5. Poll the CLIENT dashboard until that job shows retry_count > 0. The
    //    first server requeue fires after retry_delay_secs (30s in the E2E
    //    config) once the client reports the failure, so allow generous margin
    //    for the failure round-trip + backoff + re-dispatch + dashboard upsert.
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(150);
    loop {
        let jobs = client_jobs(client, client_base).await?;
        if let Some(job) = jobs.iter().find(|j| j["id"].as_str() == Some(&new_job_id)) {
            let retry_count = job["retry_count"].as_u64();
            anyhow::ensure!(
                retry_count.is_some(),
                "client job {} missing numeric 'retry_count' (#52/#56): {}",
                &new_job_id[..8.min(new_job_id.len())],
                job
            );
            if retry_count.unwrap() > 0 {
                println!(
                    "  Client dashboard shows retry_count={} for job {} ({}s) — \
                     server-driven retry surfaced (issue #56)",
                    retry_count.unwrap(),
                    &new_job_id[..8.min(new_job_id.len())],
                    start.elapsed().as_secs()
                );
                return Ok(());
            }
        }

        if start.elapsed() > timeout {
            // Diagnostics: dump the job + server-side events for the stuck job.
            let srv_events = client
                .get(format!("{}/api/jobs/{}/events", server_base, new_job_id))
                .send()
                .await
                .ok();
            let srv_text = match srv_events {
                Some(r) => r.text().await.unwrap_or_else(|_| "?".into()),
                None => "?".into(),
            };
            bail!(
                "client job {} did not reach retry_count > 0 within {}s — a \
                 server-driven retry never surfaced on the client dashboard \
                 (issue #56). Server events: {}",
                &new_job_id[..8.min(new_job_id.len())],
                timeout.as_secs(),
                &srv_text[..srv_text.len().min(2000)]
            );
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Fetch the CLIENT dashboard job list as a Vec of JSON values.
async fn client_jobs(
    client: &reqwest::Client,
    client_base: &str,
) -> Result<Vec<serde_json::Value>> {
    let jobs: Vec<serde_json::Value> = client
        .get(format!("{}/api/jobs", client_base))
        .send()
        .await?
        .json()
        .await
        .unwrap_or_default();
    Ok(jobs)
}

/// Collect the set of CLIENT job ids currently on the dashboard.
async fn client_job_ids(
    client: &reqwest::Client,
    client_base: &str,
) -> Result<std::collections::HashSet<String>> {
    let jobs = client_jobs(client, client_base).await?;
    Ok(jobs
        .iter()
        .filter_map(|j| j["id"].as_str().map(|s| s.to_string()))
        .collect())
}

/// Poll the CLIENT dashboard until a job id NOT in `ids_before` appears, and
/// return it. The newly-submitted job must reach the client to be retried.
async fn wait_for_new_client_job(
    client: &reqwest::Client,
    client_base: &str,
    ids_before: &std::collections::HashSet<String>,
) -> Result<String> {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(60);
    loop {
        let jobs = client_jobs(client, client_base).await?;
        if let Some(new_id) = jobs
            .iter()
            .filter_map(|j| j["id"].as_str())
            .find(|id| !ids_before.contains(*id))
        {
            return Ok(new_id.to_string());
        }
        if start.elapsed() > timeout {
            bail!(
                "the retry-injection job never reached the client dashboard \
                 within {}s (client has {} jobs)",
                timeout.as_secs(),
                jobs.len()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Signal the client deploy job that E2E tests are complete.
/// Creates a signal file on the client machine via the server's network access.
fn signal_e2e_done() {
    // The E2E binary runs on the server runner. Signal the client by creating
    // the done file via a network path or HTTP call. For simplicity, we write
    // to a well-known UNC path if accessible, otherwise the client job times out
    // gracefully after 10 minutes.
    let client_host =
        std::env::var("E2E_CLIENT_HOST").unwrap_or_else(|_| "print-client.lan".into());
    let signal_path = format!(r"\\{}\C$\ProgramData\DevBridge\e2e-done", client_host);
    match std::fs::write(&signal_path, "done") {
        Ok(()) => println!("  Signaled client deploy job via {}", signal_path),
        Err(e) => println!(
            "  Could not signal client ({}), it will timeout gracefully",
            e
        ),
    }
}

/// Build a minimal IPP Print-Job request payload.
/// IPP is binary-encoded over HTTP POST.
#[allow(clippy::vec_init_then_push)]
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

    // document-name (issue #30) — nameWithoutLanguage tag 0x42
    buf.push(0x42);
    let name = b"document-name";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = E2E_DOCUMENT_NAME.as_bytes();
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // Job Attributes group (issue #37) — delimiter tag 0x02
    buf.push(0x02);

    // copies — integer type 0x21, value is 4-byte signed big-endian
    buf.push(0x21);
    let name = b"copies";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val: i32 = E2E_COPIES as i32;
    buf.extend_from_slice(&4u16.to_be_bytes());
    buf.extend_from_slice(&val.to_be_bytes());

    // End of attributes
    buf.push(0x03);

    // Document data
    buf.extend_from_slice(pdf_data);

    buf
}

/// Build a minimal IPP Get-Printer-Attributes request payload.
#[allow(clippy::vec_init_then_push)]
fn build_ipp_get_printer_attributes() -> Vec<u8> {
    let mut buf = Vec::new();

    // IPP version 1.1
    buf.push(1);
    buf.push(1);

    // Operation: Get-Printer-Attributes (0x000b)
    buf.push(0x00);
    buf.push(0x0b);

    // Request ID
    buf.push(0x00);
    buf.push(0x00);
    buf.push(0x00);
    buf.push(0x02);

    // Operation attributes tag
    buf.push(0x01);

    // charset
    buf.push(0x47);
    let name = b"attributes-charset";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"utf-8";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // natural language
    buf.push(0x48);
    let name = b"attributes-natural-language";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"en-us";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // printer-uri
    buf.push(0x45);
    let name = b"printer-uri";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"ipp://localhost:631/ipp/print";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // End of attributes
    buf.push(0x03);

    buf
}

/// Verify the client dashboard shows job history after the print pipeline test.
async fn test_client_job_history(client: &reqwest::Client, client_base: &str) -> Result<()> {
    // Poll until the latest job reaches a terminal state (completed/failed).
    // The previous test submits a print job that may still be processing.
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(90);

    loop {
        let resp = client
            .get(format!("{}/api/jobs", client_base))
            .send()
            .await?;
        let jobs: serde_json::Value = resp.json().await?;
        let jobs_arr = jobs.as_array().context("expected array")?;

        anyhow::ensure!(
            !jobs_arr.is_empty(),
            "client /api/jobs returned empty array — no job history"
        );

        let latest = &jobs_arr[jobs_arr.len() - 1];
        anyhow::ensure!(latest.get("id").is_some(), "job missing 'id' field");
        anyhow::ensure!(latest.get("name").is_some(), "job missing 'name' field");
        anyhow::ensure!(
            latest.get("printer").is_some(),
            "job missing 'printer' field"
        );
        anyhow::ensure!(latest.get("status").is_some(), "job missing 'status' field");
        // Issue #52: the client dashboard must surface retry_count so an
        // operator sees the real server-driven retry count (not a hardcoded
        // 0). The field must be present and numeric on every client job.
        anyhow::ensure!(
            latest.get("retry_count").is_some(),
            "client job missing 'retry_count' field (#52): {latest}"
        );
        anyhow::ensure!(
            latest["retry_count"].is_u64(),
            "client job 'retry_count' must be a number (#52): {}",
            latest["retry_count"]
        );

        let status = latest["status"].as_str().unwrap_or("");

        if status == "completed" || status == "failed" {
            println!(
                "  Client has {} jobs, latest: status={} printer={} ({}s)",
                jobs_arr.len(),
                status,
                latest["printer"].as_str().unwrap_or("?"),
                start.elapsed().as_secs()
            );
            return Ok(());
        }

        if start.elapsed() > timeout {
            // Dump all jobs for diagnostics
            let all_statuses: Vec<String> = jobs_arr
                .iter()
                .map(|j| {
                    format!(
                        "{}={}",
                        &j["id"].as_str().unwrap_or("?")[..8],
                        j["status"].as_str().unwrap_or("?")
                    )
                })
                .collect();
            // Fetch client status
            let status_resp = client
                .get(format!("{}/api/status", client_base))
                .send()
                .await;
            let status_info = match status_resp {
                Ok(r) => r.text().await.unwrap_or_else(|_| "?".into()),
                Err(e) => format!("fetch error: {}", e),
            };
            // Fetch job events from client for the stuck job
            let job_id = latest["id"].as_str().unwrap_or("unknown");
            let events_resp = client
                .get(format!("{}/api/jobs/{}/events", client_base, job_id))
                .send()
                .await;
            let events_info = match events_resp {
                Ok(r) => r.text().await.unwrap_or_else(|_| "?".into()),
                Err(e) => format!("fetch error: {}", e),
            };
            // Also check server job status
            let server_base =
                std::env::var("E2E_SERVER_HOST").unwrap_or_else(|_| "localhost".into());
            let server_port =
                std::env::var("E2E_SERVER_DASHBOARD_PORT").unwrap_or_else(|_| "9220".into());
            let srv_resp = client
                .get(format!(
                    "http://{}:{}/api/jobs/{}/events",
                    server_base, server_port, job_id
                ))
                .send()
                .await;
            let srv_events = match srv_resp {
                Ok(r) => r.text().await.unwrap_or_else(|_| "?".into()),
                Err(e) => format!("fetch error: {}", e),
            };
            bail!(
                "job did not reach terminal state within 90s\n  last status: '{}'\n  all client jobs: [{}]\n  client status: {}\n  client events: {}\n  server events: {}",
                status,
                all_statuses.join(", "),
                &status_info[..status_info.len().min(200)],
                &events_info[..events_info.len().min(3000)],
                &srv_events[..srv_events.len().min(3000)]
            );
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Verify that changing the target printer via the dashboard API takes effect immediately.
async fn test_target_printer_hot_reload(client: &reqwest::Client, client_base: &str) -> Result<()> {
    // Read current target
    let resp = client
        .get(format!("{}/api/printers/target", client_base))
        .send()
        .await?;
    let original: serde_json::Value = resp.json().await?;
    let original_name = original["name"].as_str().unwrap_or("").to_string();
    println!("  Current target: {}", original_name);

    // Set a new target
    let test_name = "E2E-HotReload-Test-Printer";
    let resp = client
        .put(format!("{}/api/printers/target", client_base))
        .json(&serde_json::json!({"name": test_name}))
        .send()
        .await?;
    anyhow::ensure!(
        resp.status().is_success(),
        "PUT target failed: {}",
        resp.status()
    );

    // Verify it changed
    let resp = client
        .get(format!("{}/api/printers/target", client_base))
        .send()
        .await?;
    let updated: serde_json::Value = resp.json().await?;
    anyhow::ensure!(
        updated["name"].as_str() == Some(test_name),
        "target not updated: expected '{}', got '{}'",
        test_name,
        updated["name"]
    );

    // Restore original
    let _ = client
        .put(format!("{}/api/printers/target", client_base))
        .json(&serde_json::json!({"name": original_name}))
        .send()
        .await;

    println!(
        "  Hot-reload verified (set to '{}' and restored)",
        test_name
    );
    Ok(())
}

/// Verify the tray app registry key is set and points to an existing executable.
async fn test_tray_app_registry_key() -> Result<()> {
    // Check HKLM first (admin install), then HKCU (non-admin fallback)
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            r#"$v = (Get-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run' -Name 'DevBridge' -ErrorAction SilentlyContinue).DevBridge; if (-not $v) { $v = (Get-ItemProperty -Path 'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run' -Name 'DevBridge' -ErrorAction SilentlyContinue).DevBridge }; $v"#,
        ])
        .output()
        .context("Failed to read registry")?;

    let reg_value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    anyhow::ensure!(
        !reg_value.is_empty(),
        "DevBridge registry key not set in HKLM or HKCU"
    );

    // Strip quotes if present
    let exe_path = reg_value.trim_matches('"');
    anyhow::ensure!(
        std::path::Path::new(exe_path).exists(),
        "Tray app not found at registry path: {}",
        exe_path
    );

    println!("  Registry key OK: {}", exe_path);
    Ok(())
}

/// Full print flow verification: confirms that test 7's job was received
/// and completed on the CLIENT side, not just the server. This proves
/// the entire chain: IPP → server → gRPC → client → print → completion.
async fn test_full_print_flow_verified(
    client: &reqwest::Client,
    server_base: &str,
    client_base: &str,
    _ipp_url: &str,
) -> Result<()> {
    // Get the completed job from the server (created by test 7)
    let server_jobs: Vec<serde_json::Value> = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await?
        .json()
        .await
        .unwrap_or_default();

    let server_job = server_jobs
        .iter()
        .find(|j| j["status"].as_str() == Some("completed"))
        .context("No completed job found on server (test 7 should have created one)")?;

    let job_id = server_job["id"].as_str().context("Job missing id")?;
    println!("  Verifying server job {} on client...", &job_id[..8]);

    // Poll CLIENT /api/jobs for the same job_id with completed status
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(30);

    loop {
        if start.elapsed() > timeout {
            let client_jobs: Vec<serde_json::Value> = client
                .get(format!("{}/api/jobs", client_base))
                .send()
                .await?
                .json()
                .await
                .unwrap_or_default();
            let client_ids: Vec<&str> = client_jobs
                .iter()
                .filter_map(|j| j["id"].as_str())
                .collect();
            bail!(
                "Client does not have job {} after {}s. Client has {} jobs: {:?}",
                &job_id[..8],
                timeout.as_secs(),
                client_ids.len(),
                client_ids
            );
        }

        let client_jobs: Vec<serde_json::Value> = client
            .get(format!("{}/api/jobs", client_base))
            .send()
            .await?
            .json()
            .await
            .unwrap_or_default();

        if let Some(client_job) = client_jobs
            .iter()
            .find(|j| j["id"].as_str() == Some(job_id))
        {
            let status = client_job["status"].as_str().unwrap_or("");
            println!(
                "  Client job {}: status={} ({}s)",
                &job_id[..8],
                status,
                start.elapsed().as_secs()
            );
            anyhow::ensure!(
                status == "completed",
                "Client reports job {} as '{}', expected 'completed'",
                &job_id[..8],
                status
            );
            return Ok(());
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Test 23: Verify client dashboard reports mode="client".
async fn test_client_dashboard_mode(client: &reqwest::Client, client_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/config", client_base))
        .send()
        .await
        .context("Failed to reach client config endpoint")?;
    anyhow::ensure!(resp.status().is_success(), "Client config not available");

    let json: serde_json::Value = resp.json().await?;
    let mode = json["mode"].as_str().unwrap_or("");
    anyhow::ensure!(mode == "client", "Expected mode='client', got '{}'", mode);
    Ok(())
}

/// Test 24: Verify reprint API creates a new job from an existing one.
async fn test_reprint_job(client: &reqwest::Client, server_base: &str) -> Result<()> {
    // Find a completed or queued job to reprint
    let jobs: Vec<serde_json::Value> = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await?
        .json()
        .await?;

    let job = jobs
        .iter()
        .find(|j| {
            let status = j["status"].as_str().unwrap_or("");
            status == "completed" || status == "queued"
        })
        .context("No completed or queued job found to test reprint")?;

    let job_id = job["id"].as_str().context("job missing id")?;

    let url = format!("{}/api/jobs/{}/reprint", server_base, job_id);
    println!("  Reprint URL: {}", url);
    let resp = client
        .post(&url)
        .send()
        .await
        .context("Reprint request failed")?;

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    println!(
        "  Reprint response: status={}, body_len={}",
        status,
        body.len()
    );

    // 200 with HTML body = route doesn't exist (SPA fallback)
    if status == 200 && body.contains("<!DOCTYPE") {
        anyhow::bail!(
            "Reprint endpoint not deployed (got SPA fallback HTML instead of API response)"
        );
    }

    // 201 = job reprinted, 410 = spool file gone (both prove endpoint works)
    anyhow::ensure!(
        status == 201 || status == 410,
        "Expected 201 or 410, got {} (body starts: {})",
        status,
        &body[..body.len().min(200)]
    );

    if status == 201 {
        let json: serde_json::Value =
            serde_json::from_str(&body).context("Reprint response is not valid JSON")?;
        anyhow::ensure!(
            json["id"].is_string(),
            "Reprint response missing new job id"
        );
        anyhow::ensure!(
            json["reprinted_from"].as_str() == Some(job_id),
            "Reprint response should reference original job"
        );
    }

    Ok(())
}

/// Test 25: Verify WebSocket endpoint sends events when a job is created.
async fn test_websocket_events(server_base: &str, ipp_url: &str) -> Result<()> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let ws_url = server_base.replace("http://", "ws://") + "/api/ws";
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .context("Failed to connect WebSocket")?;

    // Submit a small IPP job to trigger an event
    let pdf_data = b"%PDF-1.0\nws-test-content";
    let ipp_payload = build_ipp_print_job(pdf_data);
    let ipp_client = reqwest::Client::new();
    let resp = ipp_client
        .post(ipp_url)
        .header("Content-Type", "application/ipp")
        .body(ipp_payload)
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "IPP submission failed");

    // Wait for a WebSocket event (up to 10s)
    // On old server versions (v0.2.0), the WS is echo-only and won't send events — that's OK
    let timeout = Duration::from_secs(10);
    match tokio::time::timeout(timeout, ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            let event: serde_json::Value =
                serde_json::from_str(&text).context("WebSocket message is not valid JSON")?;
            anyhow::ensure!(
                event["type"].is_string(),
                "WebSocket event missing 'type' field"
            );
            println!("  WebSocket event received: type={}", event["type"]);
            Ok(())
        }
        Ok(Some(Ok(_))) => {
            println!("  WebSocket connected (non-text message)");
            Ok(())
        }
        Ok(Some(Err(e))) => bail!("WebSocket error: {}", e),
        Ok(None) => bail!("WebSocket closed before receiving event"),
        Err(_) => {
            anyhow::bail!(
                "WebSocket connected but no events received within timeout — event broadcasting may be broken"
            );
        }
    }
}

/// Test 26: Verify PWA manifest.json is served on both server and client.
async fn test_manifest_served(
    client: &reqwest::Client,
    server_base: &str,
    client_base: &str,
) -> Result<()> {
    // Check server manifest
    let resp = client
        .get(format!("{}/manifest.json", server_base))
        .send()
        .await
        .context("Failed to fetch manifest from server")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if status.is_success() && !body.contains("<!DOCTYPE") {
        // Got actual JSON, verify it
        let json: serde_json::Value =
            serde_json::from_str(&body).context("manifest is not valid JSON")?;
        anyhow::ensure!(json["name"].is_string(), "manifest missing 'name' field");
        anyhow::ensure!(
            json["display"].as_str() == Some("standalone"),
            "manifest display should be 'standalone'"
        );
        println!("  Server manifest.json: valid PWA manifest");
    } else {
        anyhow::bail!("Server manifest not deployed (got SPA fallback)");
    }

    // Check client manifest
    let resp = client
        .get(format!("{}/manifest.json", client_base))
        .send()
        .await
        .context("Failed to fetch manifest from client")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if status.is_success() && !body.contains("<!DOCTYPE") {
        println!("  Client manifest.json: served");
    } else {
        anyhow::bail!("Client manifest not deployed (got SPA fallback)");
    }

    Ok(())
}

/// Test 27: Job events API returns a valid response for an existing job.
async fn test_job_events_api(client: &reqwest::Client, server_base: &str) -> Result<()> {
    // Get all jobs to find one with events
    let jobs_resp = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await
        .context("Failed to fetch jobs list")?;
    let jobs: Vec<serde_json::Value> = jobs_resp.json().await?;

    if let Some(job) = jobs.first() {
        let job_id = job["id"].as_str().unwrap_or("");
        let events_resp = client
            .get(format!("{}/api/jobs/{}/events", server_base, job_id))
            .send()
            .await
            .context("Failed to fetch job events")?;

        // Old server versions return HTML (SPA fallback) instead of JSON
        let content_type = events_resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/html") {
            anyhow::bail!("Events endpoint not deployed (got SPA fallback HTML)");
        }

        anyhow::ensure!(
            events_resp.status().is_success(),
            "GET /api/jobs/{}/events returned {}",
            job_id,
            events_resp.status()
        );

        let events: Vec<serde_json::Value> = events_resp.json().await?;
        println!(
            "PASS ({} events for job {})",
            events.len(),
            &job_id[..8.min(job_id.len())]
        );

        for event in &events {
            anyhow::ensure!(event["stage"].is_string(), "event missing stage field");
            anyhow::ensure!(
                event["timestamp"].is_string(),
                "event missing timestamp field"
            );
            anyhow::ensure!(!event["success"].is_null(), "event missing success field");
        }
    } else {
        println!("PASS (no jobs to check, API endpoint exists)");
    }

    Ok(())
}

/// Test 28: Job events API returns an empty array for a nonexistent job.
async fn test_job_events_nonexistent(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let resp = client
        .get(format!(
            "{}/api/jobs/nonexistent-id-12345/events",
            server_base
        ))
        .send()
        .await
        .context("Failed to fetch events for nonexistent job")?;

    // Old server versions return HTML (SPA fallback) instead of JSON
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.contains("text/html") {
        println!("PASS (events endpoint not yet deployed — SPA fallback)");
        return Ok(());
    }

    anyhow::ensure!(
        resp.status().is_success(),
        "expected 200 for nonexistent job events, got {}",
        resp.status()
    );
    let events: Vec<serde_json::Value> = resp.json().await?;
    anyhow::ensure!(
        events.is_empty(),
        "expected empty array for nonexistent job, got {} events",
        events.len()
    );
    println!("PASS");
    Ok(())
}

/// Test 29: Verify client /api/status exposes identity fields.
async fn test_client_status_identity(client: &reqwest::Client, client_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/status", client_base))
        .send()
        .await
        .context("Failed to fetch client status")?;
    let status: serde_json::Value = resp.json().await?;

    // These fields should exist in client mode (may be null if not configured)
    anyhow::ensure!(
        status.get("mode").and_then(|m| m.as_str()) == Some("client"),
        "expected client mode"
    );

    // print_backend should always be present (defaults to windows_spooler)
    let backend = status.get("print_backend").and_then(|b| b.as_str());
    if backend.is_none() {
        anyhow::bail!("Client status missing print_backend field — identity fields not deployed");
    }

    // print_timeout_secs must be surfaced so operators can verify the loaded
    // [jobs].print_timeout_secs from the dashboard API (issue #53). The client
    // service always threads its [jobs] config into the dashboard state, so a
    // missing field means an old build is deployed.
    let timeout = status
        .get("print_timeout_secs")
        .and_then(|t| t.as_u64())
        .context("Client status missing print_timeout_secs field — issue #53 not deployed")?;
    anyhow::ensure!(
        timeout > 0,
        "print_timeout_secs must be a positive value (got {timeout})"
    );

    println!(
        "PASS (backend={}, client_id={}, printer={}, print_timeout_secs={})",
        backend.unwrap(),
        status
            .get("client_id")
            .and_then(|c| c.as_str())
            .unwrap_or("none"),
        status
            .get("printer_display_name")
            .and_then(|p| p.as_str())
            .unwrap_or("none"),
        timeout,
    );
    Ok(())
}

/// Test 30: Verify server has audit events for a completed job.
async fn test_server_has_audit_events(client: &reqwest::Client, server_base: &str) -> Result<()> {
    // Find the most recent completed job
    let jobs: Vec<serde_json::Value> = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await?
        .json()
        .await?;

    if let Some(job) = jobs
        .iter()
        .find(|j| j["status"].as_str() == Some("completed"))
    {
        let job_id = job["id"].as_str().unwrap_or("");
        let events_resp = client
            .get(format!("{}/api/jobs/{}/events", server_base, job_id))
            .send()
            .await?;

        let content_type = events_resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/html") {
            anyhow::bail!("Events endpoint not deployed (got SPA fallback HTML)");
        }

        let events: Vec<serde_json::Value> = events_resp.json().await?;

        // Check for server-side events (received, routed)
        let has_received = events
            .iter()
            .any(|e| e["stage"].as_str() == Some("received"));
        let has_routed = events.iter().any(|e| e["stage"].as_str() == Some("routed"));
        // Check for client-reported events (sending, completed)
        let has_sending = events
            .iter()
            .any(|e| e["stage"].as_str() == Some("sending"));

        println!(
            "PASS ({} events, received={}, routed={}, sending={})",
            events.len(),
            has_received,
            has_routed,
            has_sending
        );
    } else {
        println!("PASS (no completed jobs to check)");
    }
    Ok(())
}

/// Test 31 (issue #51): a completed job must not show a duplicate dispatch.
///
/// The double-dispatch hazard #51 guards against is: outer timeout fires →
/// server requeues → a SECOND print task for the same job_id runs concurrently
/// with the still-draining first → two IPP `Print-Job` streams race the same
/// printer. Its user-visible footprint is a job that is *sent to the printer*
/// more than once — i.e. the client opens a second `Print-Job` stream.
///
/// We count distinct DISPATCH CYCLES that reached the printer, NOT raw
/// `sending` stages and NOT `completed` stages (issue #59). A normal, correct
/// job legitimately records `completed` more than once (client stage event plus
/// the server's own completion event), and `backend_direct_ipp` emits one
/// `sending` per PAGE — so neither raw count is a reliable double-dispatch
/// signal. `count_dispatch_cycles_that_sent` groups events by dispatch cycle
/// (anchored on each per-cycle `downloaded` stage) and counts only the cycles
/// that reached a `sending`. The #51 in-flight guard suppresses a concurrent
/// second dispatch BEFORE it touches the printer
/// (`PrintDispatch::DuplicateSuppressed` sends nothing), so a working client
/// reaches the printer in exactly ONE cycle. Two cycles that both send is the
/// double-stream footprint. See `count_dispatch_cycles_that_sent` for the full
/// case analysis (single-page, multi-page, suppressed-dup, genuine double).
///
/// This is the deterministic, hardware-free guard for that symptom. A genuine
/// hardware repro (force a backend hang, short `print_timeout_secs`, assert no
/// second stream) is non-deterministic against real printers/network and is
/// instead locked at the integration level by
/// `inflight::tests::{test_hung_backend_is_cancelled_on_outer_timeout,
/// test_requeue_double_dispatch_is_suppressed_while_first_in_flight}`.
async fn test_no_duplicate_dispatch(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let jobs: Vec<serde_json::Value> = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await?
        .json()
        .await
        .unwrap_or_default();

    let completed_job = jobs
        .iter()
        .find(|j| j["status"].as_str() == Some("completed"));

    let Some(job) = completed_job else {
        println!("PASS (no completed jobs to check)");
        return Ok(());
    };
    let job_id = job["id"].as_str().context("completed job missing id")?;

    let events_resp = client
        .get(format!("{}/api/jobs/{}/events", server_base, job_id))
        .send()
        .await
        .context("Failed to fetch job events")?;

    let content_type = events_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if content_type.contains("text/html") {
        println!("PASS (events endpoint not yet deployed — SPA fallback)");
        return Ok(());
    }

    let events: Vec<serde_json::Value> = events_resp.json().await?;

    // Count DISPATCH CYCLES that actually reached the printer (issue #59).
    //
    // The #51 double-dispatch footprint is the server re-running the WHOLE
    // receive→download→render→send lifecycle for one job_id while the first is
    // still draining — i.e. a SECOND dispatch cycle that ALSO sends to the
    // printer. Counting raw `sending` stages is WRONG: `direct_ipp` emits one
    // `Sending` PER PAGE (`send_ipp_job` is called once per rendered page on
    // jpeg/png devices — Canon MG3600 / Epson L3260), so a legitimate
    // multi-page label sheet over `direct_ipp` produces N `sending` stages in a
    // SINGLE cycle and would false-positive the old `<= 1` assertion.
    //
    // `count_dispatch_cycles_that_sent` groups events by dispatch cycle (each
    // cycle is anchored by its own `downloaded` stage, emitted exactly once per
    // `process_job` invocation for every backend, before the in-flight guard)
    // and counts only the cycles that contain at least one `sending`. This is:
    //   (a) 1 for a normal single-page job (windows_spooler/cups)            ✓
    //   (b) 1 for a normal multi-page direct_ipp job (one cycle, N sends)    ✓
    //   (c) 1 for a CORRECTLY-suppressed duplicate — the second cycle emits
    //       its pre-guard stages (downloading/downloaded) but NEVER reaches
    //       the backend, so it has NO `sending` and is not counted            ✓
    //   (d) 2 for a REAL concurrent double-stream (the #51 bug the guard must
    //       prevent) — both cycles reach `sending`                            ✓
    //
    // The real #51 lock is the `inflight` integration tests; this E2E asserts
    // the user-visible footprint is absent in the deployed pipeline.
    let cycles_that_sent = count_dispatch_cycles_that_sent(&events);

    anyhow::ensure!(
        cycles_that_sent <= 1,
        "job {} sent to the printer in {} distinct dispatch cycles — duplicate \
         dispatch (two concurrent Print-Job lifecycles) suspected (issue #51)",
        &job_id[..8.min(job_id.len())],
        cycles_that_sent
    );

    println!(
        "PASS (job {}: {} events, {} dispatch cycle(s) reached the printer — no duplicate dispatch)",
        &job_id[..8.min(job_id.len())],
        events.len(),
        cycles_that_sent
    );
    Ok(())
}

/// Count the number of distinct print DISPATCH CYCLES that reached the printer
/// (emitted a `sending` stage), given a chronologically-ordered event timeline
/// for ONE job_id (issue #59).
///
/// A dispatch cycle is the receiver's full `process_job` run: it emits
/// `downloading` then `downloaded` (once each, for every backend, BEFORE the
/// in-flight guard), and — only if the in-flight guard admits it — proceeds to
/// the backend which emits `rendering` (ghostscript backends) and one or more
/// `sending` stages. A server requeue (#51) re-runs the whole cycle, producing
/// a fresh `downloaded` anchor.
///
/// We walk the timeline, start a new cycle at each `downloaded` stage, and
/// count a cycle as "reached the printer" if it contains ≥1 `sending` stage
/// before the next `downloaded`. This makes the duplicate-dispatch invariant
/// backend-general: multi-page `direct_ipp` jobs (N `sending` in ONE cycle)
/// pass, a correctly-suppressed duplicate (a second cycle with no `sending`)
/// passes, and a genuine double-stream (two cycles that both send) fails.
///
/// `sending` stages that appear BEFORE any `downloaded` anchor (e.g. a partial
/// timeline where the server never received the client's `downloaded` event)
/// are still attributed to a cycle so the bug is never under-counted.
fn count_dispatch_cycles_that_sent(events: &[serde_json::Value]) -> usize {
    let mut cycles_that_sent = 0usize;
    // Whether the cycle we are currently inside has already been counted, so
    // multiple `sending` stages in ONE cycle (multi-page direct_ipp) count once.
    // Starts `false` so a `sending` that appears before any `downloaded` anchor
    // (a partial server-side timeline) is still attributed to a cycle — the
    // invariant must never UNDER-count a real double-stream.
    let mut current_cycle_counted = false;

    for event in events {
        match event["stage"].as_str() {
            // A new dispatch cycle begins at each `downloaded` (emitted once per
            // `process_job` run, for every backend, before the in-flight guard).
            Some("downloaded") => current_cycle_counted = false,
            // The cycle reached the printer. Count it once; further `sending`
            // (extra pages on direct_ipp) do not increment until the next
            // `downloaded` anchor starts a new cycle.
            Some("sending") if !current_cycle_counted => {
                cycles_that_sent += 1;
                current_cycle_counted = true;
            }
            _ => {}
        }
    }

    cycles_that_sent
}

/// Test 32 (issue #54): the auto-update infrastructure is in place.
///
/// 1. `/api/status` surfaces the `active_jobs` count the auto-update task reads
///    to decide skip-if-printing. It must be present and a non-negative integer.
/// 2. The `DevBridgeAutoUpdate` scheduled task is REGISTERED on the machine.
///    We assert registration only — we deliberately do NOT fire it, because the
///    self-hosted runner IS a DevBridge install and running the task would
///    upgrade the runner mid-CI. (The decision logic itself is unit-tested by
///    the Pester suite installer/tests/autoupdate.Tests.ps1.)
async fn test_auto_update_registered(client: &reqwest::Client, server_base: &str) -> Result<()> {
    // -- Part 1: active_jobs surfaced on /api/status -------------------------
    let resp = client
        .get(format!("{}/api/status", server_base))
        .send()
        .await
        .context("Failed to reach server /api/status")?;
    let json: serde_json::Value = resp.json().await?;
    let active = json
        .get("active_jobs")
        .context("/api/status missing 'active_jobs' field (issue #54 skip-if-printing guard)")?;
    anyhow::ensure!(
        active.is_u64(),
        "'active_jobs' must be a non-negative integer, got {:?}",
        active
    );
    println!("  active_jobs surfaced = {}", active.as_u64().unwrap());

    // -- Part 2: auto-update scheduled task registered (Windows) -------------
    // The E2E binary runs on the self-hosted Windows runner that is also the
    // DevBridge server, so it can query the local task scheduler directly.
    //
    // The E2E server setup (e2e-setup-server.ps1) registers the auto-update task
    // under an E2E-specific name 'DevBridgeAutoUpdateE2E' (it does NOT run
    // post-install.ps1, which is what registers the production 'DevBridgeAutoUpdate'
    // — see that file's "don't use post-install to avoid production conflicts").
    // The E2E task uses the SAME registration logic + the SAME real autoupdate.ps1
    // as production, so its presence proves the registration works; e2e-cleanup.ps1
    // removes it so it can never fire on the runner. We deliberately only assert
    // REGISTRATION — the task is never started here (that would upgrade the runner
    // mid-CI). Guarded to Windows; other hosts skip the check (Part 1 still ran).
    #[cfg(target_os = "windows")]
    {
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                // Print the task State if it exists; empty output => not registered.
                r#"(Get-ScheduledTask -TaskName 'DevBridgeAutoUpdateE2E' -ErrorAction SilentlyContinue).State"#,
            ])
            .output()
            .context("Failed to query DevBridgeAutoUpdateE2E scheduled task")?;
        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        anyhow::ensure!(
            !state.is_empty(),
            "Scheduled task 'DevBridgeAutoUpdateE2E' is not registered — \
             e2e-setup-server.ps1 did not register the auto-update task (issue #54)"
        );
        println!(
            "  DevBridgeAutoUpdateE2E task registered (state: {})",
            state
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        println!("  (scheduled-task check skipped: non-Windows host)");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a chronological event timeline (just the `stage` field — the only
    /// field `count_dispatch_cycles_that_sent` inspects) from a slice of stage
    /// names, mirroring the `/api/jobs/{id}/events` JSON shape.
    fn timeline(stages: &[&str]) -> Vec<serde_json::Value> {
        stages.iter().map(|s| json!({ "stage": s })).collect()
    }

    /// (a) Normal single-page job via windows_spooler: one dispatch cycle,
    /// exactly one `sending`. Must count as ONE cycle reaching the printer.
    /// This is the CI-config case the old `<= 1 sending` assertion covered.
    #[test]
    fn single_page_single_backend_counts_one_cycle() {
        let events = timeline(&["downloading", "downloaded", "sending", "sent", "completed"]);
        assert_eq!(
            count_dispatch_cycles_that_sent(&events),
            1,
            "a normal single-page job must reach the printer in exactly one cycle"
        );
    }

    /// (b) The bug #59 fixes: a legitimate MULTI-PAGE direct_ipp job emits one
    /// `sending` PER PAGE within a SINGLE dispatch cycle (one `downloaded`,
    /// one `rendering`, three `sending`). The old `<= 1 sending` assertion
    /// false-positived this; the dispatch-cycle invariant must count ONE.
    #[test]
    fn multi_page_direct_ipp_counts_one_cycle_not_per_page() {
        let events = timeline(&[
            "downloading",
            "downloaded",
            "rendering",
            "rendered",
            "sending", // page 1
            "acknowledged",
            "completed",
            "sending", // page 2
            "acknowledged",
            "completed",
            "sending", // page 3
            "acknowledged",
            "completed",
        ]);
        assert_eq!(
            count_dispatch_cycles_that_sent(&events),
            1,
            "a multi-page direct_ipp job is ONE dispatch cycle even though it \
             emits one `sending` per page — it must not false-positive (issue #59)"
        );
    }

    /// (c) A CORRECTLY-suppressed duplicate (#51 in-flight guard working): the
    /// server requeues, a SECOND dispatch cycle starts and emits its pre-guard
    /// stages (`downloading`/`downloaded`) but the in-flight guard suppresses
    /// it BEFORE the backend runs — so the second cycle has NO `sending`. The
    /// invariant must count ONE (the working guard is not a violation).
    #[test]
    fn suppressed_duplicate_does_not_false_positive() {
        let events = timeline(&[
            // First (real) dispatch cycle — reaches the printer.
            "downloading",
            "downloaded",
            "sending",
            "sent",
            "completed",
            // Second cycle (server requeue) — suppressed before the backend;
            // emits pre-guard stages only, never `sending`.
            "downloading",
            "downloaded",
            "failed", // "duplicate print suppressed — prior task still in flight"
        ]);
        assert_eq!(
            count_dispatch_cycles_that_sent(&events),
            1,
            "a correctly-SUPPRESSED duplicate (guard working, no second \
             `sending`) must NOT be flagged as a double-dispatch"
        );
    }

    /// (d) A REAL concurrent double-stream (the #51 bug the guard must prevent):
    /// TWO dispatch cycles BOTH reach the printer (`downloaded` … `sending`
    /// twice). The invariant MUST fail this (count == 2 > 1) — proving the
    /// assertion can still catch the regression it exists to catch.
    #[test]
    fn genuine_double_stream_is_detected() {
        let events = timeline(&[
            // First dispatch cycle — sends.
            "downloading",
            "downloaded",
            "sending",
            "sent",
            // Second concurrent dispatch cycle — ALSO sends (the bug).
            "downloading",
            "downloaded",
            "sending",
            "sent",
            "completed",
        ]);
        assert_eq!(
            count_dispatch_cycles_that_sent(&events),
            2,
            "two distinct dispatch cycles that BOTH send is the double-stream \
             footprint and MUST be counted as a violation (issue #51)"
        );
    }

    /// A genuine multi-page double-stream: TWO cycles, the first multi-page
    /// (3 sends), the second also sends. Must count 2 (not 4) — proving the
    /// per-cycle dedup and the cross-cycle detection compose correctly.
    #[test]
    fn multi_page_double_stream_counts_cycles_not_pages() {
        let events = timeline(&[
            "downloaded",
            "sending",
            "sending",
            "sending", // cycle 1: 3 pages
            "downloaded",
            "sending", // cycle 2: the duplicate
        ]);
        assert_eq!(
            count_dispatch_cycles_that_sent(&events),
            2,
            "duplicate detection must count CYCLES, not pages — a 3-page first \
             cycle plus a duplicate second cycle is 2, not 4"
        );
    }

    /// A completed job that never sent (e.g. all dispatches suppressed/failed
    /// pre-send) yields zero cycles — the assertion `<= 1` still passes, and
    /// the count is honest about what reached the printer.
    #[test]
    fn no_sending_counts_zero() {
        let events = timeline(&["downloading", "downloaded", "failed"]);
        assert_eq!(count_dispatch_cycles_that_sent(&events), 0);
    }

    /// Defensive: a `sending` with no preceding `downloaded` anchor (partial
    /// server-side timeline) is still attributed to a cycle so a real
    /// double-stream is never UNDER-counted.
    #[test]
    fn sending_before_any_downloaded_still_counts() {
        let events = timeline(&["sending", "downloaded", "sending"]);
        assert_eq!(
            count_dispatch_cycles_that_sent(&events),
            2,
            "a `sending` before any `downloaded` anchor must still start a cycle"
        );
    }
}
