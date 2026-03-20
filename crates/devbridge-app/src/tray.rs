//! System tray icon, menu construction, and event handling.

use tauri::{
    menu::{Menu, MenuItem},
    tray::{TrayIcon, TrayIconBuilder},
    App, Manager,
};

/// Set up the system tray icon with menu items.
pub fn setup_tray(app: &App) -> Result<TrayIcon, Box<dyn std::error::Error>> {
    let open_dashboard =
        MenuItem::with_id(app, "open_dashboard", "Open Dashboard", true, None::<&str>)?;
    let start_service =
        MenuItem::with_id(app, "start_service", "Start Service", true, None::<&str>)?;
    let stop_service = MenuItem::with_id(app, "stop_service", "Stop Service", true, None::<&str>)?;
    let status = MenuItem::with_id(app, "status", "Status: Unknown", false, None::<&str>)?;
    let separator = MenuItem::with_id(app, "sep", "─────────", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &open_dashboard,
            &separator,
            &start_service,
            &stop_service,
            &status,
            &separator,
            &quit,
        ],
    )?;

    let tray = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("DevBridge")
        .on_menu_event(move |_app, event| {
            handle_menu_event(event.id().as_ref());
        })
        .build(app)?;

    Ok(tray)
}

/// Handle tray menu item clicks.
fn handle_menu_event(id: &str) {
    match id {
        "open_dashboard" => {
            tracing::info!("Opening dashboard");
            // TODO: Open browser or embedded webview to dashboard URL
        }
        "start_service" => {
            tracing::info!("Requesting service start");
            // TODO: Send IPC request to start the service
        }
        "stop_service" => {
            tracing::info!("Requesting service stop");
            // TODO: Send IPC request to stop the service
        }
        "quit" => {
            tracing::info!("Quit requested");
            std::process::exit(0);
        }
        _ => {}
    }
}
