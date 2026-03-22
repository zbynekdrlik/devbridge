#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ipc_client;
mod tray;

fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting DevBridge tray application");

    // Try to determine dashboard port from config
    let dashboard_port = resolve_dashboard_port();
    tracing::info!("Dashboard port: {}", dashboard_port);

    tauri::Builder::default()
        .setup(move |app| {
            tray::setup_tray(app, dashboard_port)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running DevBridge tray application");
}

/// Resolve the dashboard port from config or environment, defaulting to 9120.
fn resolve_dashboard_port() -> u16 {
    // Check env var first (set by post-install or CI)
    if let Ok(port) = std::env::var("DEVBRIDGE_DASHBOARD_PORT") {
        if let Ok(p) = port.parse::<u16>() {
            return p;
        }
    }

    // Try loading config from standard locations
    let config_paths = [
        std::path::PathBuf::from(r"C:\ProgramData\DevBridge\config.toml"),
        std::path::PathBuf::from("config/default.toml"),
    ];

    for path in &config_paths {
        if let Ok(config) = devbridge_core::Config::load(path) {
            // Return whichever port is relevant based on mode
            if config.general.mode == "client" {
                return config.client.dashboard_port;
            }
            return config.server.dashboard_port;
        }
    }

    9120
}
