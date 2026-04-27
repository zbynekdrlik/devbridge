#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ipc_client;
mod job_tracker;
mod tray;
mod ws_client;

fn main() {
    init_logging();
    tracing::info!("Starting DevBridge tray application");

    // Try to determine dashboard port from config
    let dashboard_port = resolve_dashboard_port();
    tracing::info!("Dashboard port: {}", dashboard_port);

    let dashboard_url = format!("http://127.0.0.1:{dashboard_port}");
    let dashboard_url_handler = dashboard_url.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {}))
        .plugin(tauri_plugin_notification::init())
        // Global menu event handler — fires for ALL menu events from any
        // menu (including tray menus rebuilt via tray.set_menu()). The
        // tray-level on_menu_event handler doesn't fire reliably after
        // set_menu() rebuilds the menu items.
        .on_menu_event(move |app, event| {
            tray::handle_menu_event(app, &dashboard_url_handler, event.id().as_ref());
        })
        .setup(move |app| {
            tray::setup_tray(app, dashboard_port)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running DevBridge tray application");
}

/// Initialise tracing. In release builds the tray runs under
/// `windows_subsystem = "windows"`, which closes stdout — without a file
/// appender every `tracing::info!` would vanish. We mirror the service's
/// pattern (`devbridge-service::runtime::run`): daily rotation, 7-file
/// retention, non-blocking writer with the guard leaked for process lifetime.
///
/// Per-user log dir (each RDP session has its own tray instance):
/// - Windows: `%LOCALAPPDATA%\DevBridge\logs\tray.YYYY-MM-DD.log`
/// - macOS / Linux: `$HOME/.devbridge/logs/tray.YYYY-MM-DD.log`
///
/// If file logging fails to initialise we fall back to `tracing_subscriber::fmt::init()`
/// (visible in debug builds, silent under windows_subsystem in release).
fn init_logging() {
    if let Some(log_dir) = tray_log_dir()
        && std::fs::create_dir_all(&log_dir).is_ok()
        && let Ok(appender) = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("tray")
            .filename_suffix("log")
            .max_log_files(7)
            .build(&log_dir)
    {
        let (writer, guard) = tracing_appender::non_blocking(appender);
        // Leak the guard so the non-blocking writer keeps running for the
        // life of the process. Dropping it would flush + stop the writer
        // thread and subsequent log lines would vanish.
        std::mem::forget(guard);
        tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .init();
        tracing::info!(log_dir = %log_dir.display(), "Tray logging to file");
        return;
    }
    tracing_subscriber::fmt::init();
}

/// Per-user log directory for the tray. Returns None only in stripped
/// environments where neither `LOCALAPPDATA` nor `HOME` is set.
fn tray_log_dir() -> Option<std::path::PathBuf> {
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        return Some(
            std::path::PathBuf::from(local_appdata)
                .join("DevBridge")
                .join("logs"),
        );
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(
            std::path::PathBuf::from(home)
                .join(".devbridge")
                .join("logs"),
        );
    }
    None
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
