#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ipc_client;
mod tray;

use tracing;

fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting DevBridge tray application");

    tauri::Builder::default()
        .plugin(tauri::plugin::TrayIconPlugin::init())
        .setup(|app| {
            tray::setup_tray(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running DevBridge tray application");
}
