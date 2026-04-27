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
use crate::job_tracker::{FilterState, IconState, JobDisplayStatus, JobTracker};
use crate::ws_client::{WsEvent, run_ws_client};
use devbridge_core::job::JobEvent;

// Single tray icon — the DevBridge printer logo. Status is conveyed via
// the menu text and balloon notifications, not by swapping icons (users
// already have many tray icons and a generic colored bullet adds noise).

/// Set up the system tray icon with menu items and start event processing.
pub fn setup_tray(app: &App, dashboard_port: u16) -> Result<TrayIcon, Box<dyn std::error::Error>> {
    let dashboard_url = format!("http://127.0.0.1:{dashboard_port}");

    // Tracker starts with no filter; the async event loop detects server mode
    // and sets the filter user before fetching initial jobs. This avoids a
    // blocking HTTP call on the Tauri main thread during startup.
    let tracker = Arc::new(Mutex::new(JobTracker::new()));

    // Build initial menu with empty job list
    let initial_tracker = JobTracker::new();
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
        t.set_filter_state(filter_user);
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
                document_name: _,
                requesting_user,
                target_printer,
            }) => {
                let should_notify = {
                    let mut t = tracker.lock().await;
                    if t.should_process(&requesting_user) {
                        t.on_job_created(job_id, target_printer.clone());
                        true
                    } else {
                        false
                    }
                };
                if should_notify {
                    show_notification(
                        &app,
                        "Print Job Received",
                        &format!("→ {target_printer}"),
                    );
                    update_tray(&app, &tracker).await;
                }
            }
            WsEvent::Job(JobEvent::StateChanged {
                job_id,
                new_state,
                requesting_user,
            }) => {
                // StateChanged is the AUTHORITATIVE source for completion.
                // The server emits this on every state transition, including
                // Completed/Failed when the client reports the final result.
                let (terminal_status, target_printer) = {
                    let mut t = tracker.lock().await;
                    if !t.should_process(&requesting_user) {
                        continue;
                    }
                    let printer = t
                        .recent_jobs
                        .iter()
                        .find(|j| j.job_id == job_id)
                        .map(|j| j.target_printer.clone())
                        .unwrap_or_default();
                    (t.on_state_changed(&job_id, new_state), printer)
                };
                if let Some(status) = terminal_status {
                    let title = match status {
                        JobDisplayStatus::Completed => "Printed ✓",
                        JobDisplayStatus::Failed => "Print Failed",
                        JobDisplayStatus::InProgress => continue,
                    };
                    show_notification(&app, title, &format!("→ {target_printer}"));
                    update_tray(&app, &tracker).await;
                }
            }
            WsEvent::Print(evt) => {
                // Stage events are advisory — used to keep the audit trail
                // detailed but the authoritative source for completion is
                // JobEvent::StateChanged. Update the tracker if the job is
                // already known but don't show duplicate notifications.
                let mut t = tracker.lock().await;
                if t.recent_jobs.iter().any(|j| j.job_id == evt.job_id) {
                    t.on_print_event(&evt.job_id, evt.stage, evt.success);
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

/// TEMPORARY stub. Task 5 replaces this with `detect_filter_loop`
/// which does proper retry-with-backoff. Kept here only so the file
/// compiles between Task 3 and Task 5.
async fn detect_filter_user(_dashboard_url: &str) -> FilterState {
    FilterState::Pending
}

/// Fetch up to 5 recent jobs from the dashboard API and populate the tracker.
async fn fetch_initial_jobs(dashboard_url: &str, tracker: &Arc<Mutex<JobTracker>>) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client build");

    // Use requesting_user filter on terminal server so only this user's jobs show
    let filter_state = {
        let t = tracker.lock().await;
        t.filter_state()
    };
    let url = match &filter_state {
        FilterState::User(u) => {
            format!("{dashboard_url}/api/jobs?limit=5&requesting_user={u}")
        }
        FilterState::Disabled | FilterState::Pending => {
            format!("{dashboard_url}/api/jobs?limit=5")
        }
    };

    match http.get(&url).send().await {
        Ok(resp) => match resp.json::<Vec<serde_json::Value>>().await {
            Ok(jobs) => {
                let mut t = tracker.lock().await;
                // Jobs come newest-first from API; add in reverse so push_front gives correct order
                for job in jobs.iter().rev() {
                    // Dashboard API uses "id" / "name" / "printer" / "status",
                    // not the underlying JobMetadata field names.
                    let job_id = job["id"].as_str().unwrap_or("").to_string();
                    let target_printer = job["printer"]
                        .as_str()
                        .unwrap_or("unknown printer")
                        .to_string();
                    let status_str = job["status"].as_str().unwrap_or("queued");
                    let created_at_str = job["created_at"].as_str().unwrap_or("");

                    if job_id.is_empty() {
                        continue;
                    }

                    let timestamp = chrono::DateTime::parse_from_rfc3339(created_at_str)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());

                    let display_status = match status_str {
                        "completed" => JobDisplayStatus::Completed,
                        "failed" | "cancelled" => JobDisplayStatus::Failed,
                        _ => JobDisplayStatus::InProgress,
                    };

                    t.add_job(job_id, target_printer, timestamp, display_status);
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
        // Format: "{status} {age}  → {target_printer}"
        // The user asked for date/time + destination printer instead of
        // the useless job-{uuid} document name (IPP doesn't expose the
        // real document name via the ippper crate we use).
        let label = format!("{icon} {age}  → {}", job.target_printer);
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
            // Primary: let `open` crate pick the default browser.
            // Fallback: on Windows, `cmd /c start` is more reliable in
            // disconnected-RDP / session-0 contexts where `open::that`
            // returns without spawning anything visible. Tried in order:
            //  1. open::that
            //  2. cmd /c start "" "<url>"  (Windows)
            //  3. xdg-open <url>  (Linux/mac fallback)
            let opened_ok = open::that(dashboard_url).is_ok();
            if !opened_ok {
                tracing::warn!("open::that failed, trying shell fallback");
                #[cfg(windows)]
                let shell_ok = std::process::Command::new("cmd")
                    .args(["/c", "start", "", dashboard_url])
                    .spawn()
                    .is_ok();
                #[cfg(not(windows))]
                let shell_ok = std::process::Command::new("xdg-open")
                    .arg(dashboard_url)
                    .spawn()
                    .is_ok();
                if !shell_ok {
                    tracing::error!(
                        "Failed to open browser for {}. Dashboard URL: {}",
                        dashboard_url,
                        dashboard_url
                    );
                    // Show a notification with the URL so the user can at
                    // least see it and paste into a browser manually.
                    use tauri_plugin_notification::NotificationExt;
                    let _ = app
                        .notification()
                        .builder()
                        .title("DevBridge: Open Dashboard manually")
                        .body(format!("Could not auto-open. URL: {}", dashboard_url))
                        .show();
                }
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
