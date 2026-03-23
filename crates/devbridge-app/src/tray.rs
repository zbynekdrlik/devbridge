//! System tray icon, menu construction, and event handling.

use std::sync::Arc;
use std::time::Duration;

use tauri::{
    menu::{Menu, MenuEvent, MenuItem},
    tray::{TrayIcon, TrayIconBuilder},
    App, AppHandle,
};
use tokio::sync::Mutex;

use crate::ipc_client;

/// Set up the system tray icon with menu items and start status polling.
pub fn setup_tray(app: &App, dashboard_port: u16) -> Result<TrayIcon, Box<dyn std::error::Error>> {
    let open_dashboard =
        MenuItem::with_id(app, "open_dashboard", "Open Dashboard", true, None::<&str>)?;
    let start_service =
        MenuItem::with_id(app, "start_service", "Start Service", true, None::<&str>)?;
    let stop_service =
        MenuItem::with_id(app, "stop_service", "Stop Service", true, None::<&str>)?;
    let status = MenuItem::with_id(app, "status", "Status: Unknown", false, None::<&str>)?;
    let separator1 = MenuItem::with_id(app, "sep1", "─────────", false, None::<&str>)?;
    let separator2 = MenuItem::with_id(app, "sep2", "─────────", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &open_dashboard,
            &separator1,
            &start_service,
            &stop_service,
            &status,
            &separator2,
            &quit,
        ],
    )?;

    let port = dashboard_port;
    let tray = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("DevBridge")
        .on_menu_event(move |app, event| {
            handle_menu_event(app, &event, port);
        })
        .build(app)?;

    // Start background status polling
    let handle = app.handle().clone();
    let status_item = Arc::new(Mutex::new(status));
    tauri::async_runtime::spawn(poll_status(handle, status_item, dashboard_port));

    Ok(tray)
}

/// Handle tray menu item clicks.
fn handle_menu_event(app: &AppHandle, event: &MenuEvent, dashboard_port: u16) {
    match event.id().as_ref() {
        "open_dashboard" => {
            let url = format!("http://localhost:{}", dashboard_port);
            tracing::info!("Opening dashboard at {}", url);
            if let Err(e) = open::that(&url) {
                tracing::error!("Failed to open browser: {}", e);
            }
        }
        "start_service" => {
            tracing::info!("Requesting service start");
            tauri::async_runtime::spawn(async move {
                match start_service().await {
                    Ok(()) => tracing::info!("Service start requested"),
                    Err(e) => tracing::error!("Failed to start service: {}", e),
                }
            });
        }
        "stop_service" => {
            tracing::info!("Requesting service stop");
            tauri::async_runtime::spawn(async move {
                match stop_service().await {
                    Ok(()) => tracing::info!("Service stop requested"),
                    Err(e) => tracing::error!("Failed to stop service: {}", e),
                }
            });
        }
        "quit" => {
            tracing::info!("Quit requested");
            app.exit(0);
        }
        _ => {}
    }
}

/// Start the DevBridge service via IPC, falling back to sc.exe on Windows.
async fn start_service() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use devbridge_core::ipc::IpcRequest;

    match ipc_client::send_request(&IpcRequest::StartService).await {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!("IPC start failed ({}), trying sc.exe fallback", e);
            sc_command("start").await
        }
    }
}

/// Stop the DevBridge service via IPC, falling back to sc.exe on Windows.
async fn stop_service() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use devbridge_core::ipc::IpcRequest;

    match ipc_client::send_request(&IpcRequest::StopService).await {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!("IPC stop failed ({}), trying sc.exe fallback", e);
            sc_command("stop").await
        }
    }
}

/// Fallback: use sc.exe to control the Windows service.
#[cfg(target_os = "windows")]
async fn sc_command(action: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let output = tokio::process::Command::new("sc.exe")
        .args([action, "DevBridge"])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("sc.exe {} failed: {}", action, stderr).into());
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
async fn sc_command(action: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::warn!("sc.exe {} not available on this platform", action);
    Err("sc.exe is only available on Windows".into())
}

/// Periodically query the dashboard status endpoint and update the tray menu.
async fn poll_status(
    _handle: AppHandle,
    status_item: Arc<Mutex<MenuItem<tauri::Wry>>>,
    dashboard_port: u16,
) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let url = format!("http://localhost:{}/api/status", dashboard_port);

    loop {
        let label = match http.get(&url).send().await {
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(json) => {
                    let status = json["status"].as_str().unwrap_or("unknown");
                    let mode = json["mode"].as_str().unwrap_or("?");
                    format!("Status: {} ({})", status, mode)
                }
                Err(_) => "Status: Error".to_string(),
            },
            Err(_) => "Status: Stopped".to_string(),
        };

        let item = status_item.lock().await;
        if let Err(e) = item.set_text(&label) {
            tracing::warn!("Failed to update status text: {}", e);
        }

        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
