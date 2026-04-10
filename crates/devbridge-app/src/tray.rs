//! System tray icon, menu construction, job history, and notifications.

use std::sync::Arc;
use std::time::Duration;

use tauri::Wry;
use tauri::{
    App, AppHandle,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{TrayIcon, TrayIconBuilder},
};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::{Mutex, mpsc};

use crate::ipc_client;
use crate::job_tracker::{IconState, JobDisplayStatus, JobTracker};
use crate::ws_client::{WsEvent, run_ws_client};
use devbridge_core::job::JobEvent;
use devbridge_core::job_event::PrintStage;

// Single tray icon — the DevBridge printer logo. Status is conveyed via
// the menu text and balloon notifications, not by swapping icons (users
// already have many tray icons and a generic colored bullet adds noise).

/// Set up the system tray icon with menu items and start event processing.
pub fn setup_tray(app: &App, dashboard_port: u16) -> Result<TrayIcon, Box<dyn std::error::Error>> {
    let dashboard_url = format!("http://127.0.0.1:{dashboard_port}");

    // Tracker starts with no filter; the async event loop detects server mode
    // and sets the filter user before fetching initial jobs. This avoids a
    // blocking HTTP call on the Tauri main thread during startup.
    let tracker = Arc::new(Mutex::new(JobTracker::new(None)));

    // Build initial menu with empty job list
    let initial_tracker = JobTracker::new(None);
    let menu = build_menu(app, &initial_tracker)?;

    // Menu events are handled globally via app.on_menu_event() in main.rs
    // — that handler fires reliably even after tray.set_menu() rebuilds.
    let tray = TrayIconBuilder::with_id("main")
        .icon(tauri::include_image!("../../assets/icons/tray-icon.png"))
        .menu(&menu)
        .tooltip("DevBridge")
        .build(app)?;

    // Spawn the async event processing loop
    let handle = app.handle().clone();
    let tracker_clone = tracker.clone();
    let url_clone = dashboard_url.clone();

    tauri::async_runtime::spawn(async move {
        run_event_loop(handle, url_clone, tracker_clone).await;
    });

    // Spawn status poll loop (10s interval)
    let handle2 = app.handle().clone();
    let tracker_poll = tracker.clone();
    let url_poll = dashboard_url.clone();

    tauri::async_runtime::spawn(async move {
        poll_status_loop(handle2, url_poll, tracker_poll).await;
    });

    Ok(tray)
}

/// Main event processing loop: connects WebSocket, receives events, updates tray.
async fn run_event_loop(app: AppHandle, dashboard_url: String, tracker: Arc<Mutex<JobTracker>>) {
    // Detect server mode and set filter_user before fetching initial jobs.
    // Done async here so it doesn't block the Tauri main thread in setup_tray.
    let filter_user = detect_filter_user(&dashboard_url).await;
    tracing::info!("Tray filter_user: {:?}", filter_user);
    {
        let mut t = tracker.lock().await;
        t.set_filter_user(filter_user);
    }

    let (tx, mut rx) = mpsc::channel::<WsEvent>(64);

    // Spawn WebSocket client
    let ws_url = dashboard_url.clone();
    tauri::async_runtime::spawn(async move {
        run_ws_client(ws_url, tx).await;
    });

    // Fetch initial jobs to populate tracker
    fetch_initial_jobs(&dashboard_url, &tracker).await;

    // Mark service as online and update tray
    {
        let mut t = tracker.lock().await;
        t.set_online(true);
    }
    update_tray(&app, &tracker).await;

    // Process incoming events
    while let Some(event) = rx.recv().await {
        match event {
            WsEvent::Job(JobEvent::Created {
                job_id,
                document_name,
                requesting_user,
            }) => {
                let should_notify = {
                    let mut t = tracker.lock().await;
                    if t.should_process(&requesting_user) {
                        t.on_job_created(job_id, document_name.clone());
                        true
                    } else {
                        false
                    }
                };
                if should_notify {
                    show_notification(&app, "Print Job Received", &document_name);
                    update_tray(&app, &tracker).await;
                }
            }
            WsEvent::Job(JobEvent::StateChanged { .. }) => {
                // Ignore — covered by PrintJobEvent
            }
            WsEvent::Print(evt) => {
                let job_in_tracker = {
                    let t = tracker.lock().await;
                    t.recent_jobs.iter().any(|j| j.job_id == evt.job_id)
                };
                if job_in_tracker {
                    {
                        let mut t = tracker.lock().await;
                        t.on_print_event(&evt.job_id, evt.stage, evt.success);
                    }

                    // Show notification for key stages only
                    match (evt.stage, evt.success) {
                        (PrintStage::Completed, true) => {
                            show_notification(&app, "Printed", &format!("{} ✓", evt.detail));
                        }
                        (PrintStage::Failed, _) | (_, false) => {
                            show_notification(
                                &app,
                                "Print Failed",
                                &format!("Failed: {}", evt.detail),
                            );
                        }
                        _ => {} // other stages — no notification
                    }

                    update_tray(&app, &tracker).await;
                }
            }
        }
    }
}

/// Periodically poll the status endpoint and update tray online/offline state.
async fn poll_status_loop(app: AppHandle, dashboard_url: String, tracker: Arc<Mutex<JobTracker>>) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client build");
    let url = format!("{}/api/status", dashboard_url);

    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;

        let online = http.get(&url).send().await.is_ok();
        {
            let mut t = tracker.lock().await;
            t.set_online(online);
        }
        update_tray(&app, &tracker).await;
    }
}

/// Async HTTP call to detect whether this is a server-mode instance.
/// On server mode, returns the USERNAME env var for per-user filtering.
/// On client mode or on any error, returns None.
async fn detect_filter_user(dashboard_url: &str) -> Option<String> {
    let url = format!("{}/api/status", dashboard_url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;

    let mode = json["mode"].as_str()?;
    if mode != "server" {
        return None;
    }

    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()
}

/// Fetch up to 5 recent jobs from the dashboard API and populate the tracker.
async fn fetch_initial_jobs(dashboard_url: &str, tracker: &Arc<Mutex<JobTracker>>) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client build");

    // Use requesting_user filter on terminal server so only this user's jobs show
    let filter_user = {
        let t = tracker.lock().await;
        t.filter_user().cloned()
    };
    let url = match &filter_user {
        Some(u) => format!("{dashboard_url}/api/jobs?limit=5&requesting_user={u}"),
        None => format!("{dashboard_url}/api/jobs?limit=5"),
    };

    match http.get(&url).send().await {
        Ok(resp) => match resp.json::<Vec<serde_json::Value>>().await {
            Ok(jobs) => {
                let mut t = tracker.lock().await;
                // Jobs come newest-first from API; add in reverse so push_front gives correct order
                for job in jobs.iter().rev() {
                    let job_id = job["job_id"].as_str().unwrap_or("").to_string();
                    let document_name = job["document_name"]
                        .as_str()
                        .unwrap_or("Unknown")
                        .to_string();
                    let state = job["state"].as_str().unwrap_or("queued");

                    if job_id.is_empty() {
                        continue;
                    }

                    t.on_job_created(job_id.clone(), document_name);

                    // Reflect final state from API response
                    match state {
                        "completed" => t.on_print_event(&job_id, PrintStage::Completed, true),
                        "failed" | "cancelled" => {
                            t.on_print_event(&job_id, PrintStage::Failed, false)
                        }
                        _ => {} // queued / downloading / printing → InProgress / Yellow
                    }
                }
            }
            Err(e) => tracing::warn!("Failed to parse initial jobs: {e}"),
        },
        Err(e) => tracing::warn!("Failed to fetch initial jobs: {e}"),
    }
}

/// Build the tray menu from the current tracker state.
fn build_menu(
    app: &impl tauri::Manager<Wry>,
    tracker: &JobTracker,
) -> Result<Menu<Wry>, Box<dyn std::error::Error>> {
    let recent_header =
        MenuItem::with_id(app, "recent_header", "Recent Jobs", false, None::<&str>)?;

    let mut items: Vec<Box<dyn tauri::menu::IsMenuItem<Wry>>> = vec![Box::new(recent_header)];

    for (i, job) in tracker.recent_jobs.iter().enumerate() {
        let icon = match job.status {
            JobDisplayStatus::Completed => "✓",
            JobDisplayStatus::Failed => "✗",
            JobDisplayStatus::InProgress => "⏳",
        };
        let age = format_age(job.timestamp);
        let label = format!("{} {}  {}", icon, job.document_name, age);
        let id = format!("job_{i}");
        let item = MenuItem::with_id(app, &id, &label, false, None::<&str>)?;
        items.push(Box::new(item));
    }

    if tracker.recent_jobs.is_empty() {
        let empty = MenuItem::with_id(app, "no_jobs", "(no recent jobs)", false, None::<&str>)?;
        items.push(Box::new(empty));
    }

    items.push(Box::new(PredefinedMenuItem::separator(app)?));

    let open_dashboard =
        MenuItem::with_id(app, "open_dashboard", "Open Dashboard", true, None::<&str>)?;
    items.push(Box::new(open_dashboard));

    items.push(Box::new(PredefinedMenuItem::separator(app)?));

    let status_label = match tracker.icon_state {
        IconState::Green => "● Online",
        IconState::Yellow => "◐ Printing...",
        IconState::Red => "● Error",
        IconState::Gray => "○ Offline",
    };
    let status_item = MenuItem::with_id(app, "status", status_label, false, None::<&str>)?;
    items.push(Box::new(status_item));

    let start_service =
        MenuItem::with_id(app, "start_service", "Start Service", true, None::<&str>)?;
    items.push(Box::new(start_service));

    let stop_service = MenuItem::with_id(app, "stop_service", "Stop Service", true, None::<&str>)?;
    items.push(Box::new(stop_service));

    items.push(Box::new(PredefinedMenuItem::separator(app)?));

    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    items.push(Box::new(quit));

    let item_refs: Vec<&dyn tauri::menu::IsMenuItem<Wry>> =
        items.iter().map(|b| b.as_ref()).collect();
    Ok(Menu::with_items(app, &item_refs)?)
}

/// Update the tray menu from the current tracker state.
/// The icon stays the same (DevBridge printer logo); status is shown
/// in the menu status line and via balloon notifications.
async fn update_tray(app: &AppHandle, tracker: &Arc<Mutex<JobTracker>>) {
    let t = tracker.lock().await;

    let menu = match build_menu(app, &t) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("Failed to build tray menu: {e}");
            return;
        }
    };

    if let Some(tray) = app.tray_by_id("main") {
        // Update tooltip to reflect current state (visible on hover)
        let tooltip = match t.icon_state {
            IconState::Green => "DevBridge — Online",
            IconState::Yellow => "DevBridge — Printing...",
            IconState::Red => "DevBridge — Error",
            IconState::Gray => "DevBridge — Offline",
        };
        let _ = tray.set_tooltip(Some(tooltip));
        if let Err(e) = tray.set_menu(Some(menu)) {
            tracing::warn!("Failed to set tray menu: {e}");
        }
    }
}

/// Show a desktop notification.
fn show_notification(app: &AppHandle, title: &str, body: &str) {
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        tracing::warn!("Failed to show notification: {e}");
    }
}

/// Handle tray menu item clicks. Called from the global app menu event
/// handler in main.rs (which fires reliably regardless of menu rebuilds).
pub fn handle_menu_event(app: &AppHandle, dashboard_url: &str, event_id: &str) {
    match event_id {
        "open_dashboard" => {
            tracing::info!("Opening dashboard at {}", dashboard_url);
            if let Err(e) = open::that(dashboard_url) {
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

/// Format a UTC timestamp as a human-readable age string.
fn format_age(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    let secs = chrono::Utc::now()
        .signed_duration_since(timestamp)
        .num_seconds()
        .max(0);

    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Start the DevBridge service via IPC, falling back to sc.exe on Windows.
async fn start_service() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use devbridge_core::ipc::IpcRequest;

    match ipc_client::send_request(&IpcRequest::StartService).await {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!("IPC start failed ({}), trying system fallback", e);
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
            tracing::warn!("IPC stop failed ({}), trying system fallback", e);
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
    let plist = "/Library/LaunchDaemons/com.devbridge.service.plist";
    let args: Vec<&str> = match action {
        "start" => vec!["load", "-w", plist],
        "stop" => vec!["unload", plist],
        _ => return Err(format!("Unknown action: {}", action).into()),
    };

    let output = tokio::process::Command::new("launchctl")
        .args(&args)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("launchctl {} failed: {}", action, stderr).into());
    }
    Ok(())
}
