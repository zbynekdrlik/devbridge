use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use anyhow::{Context, Result};
use chrono::Utc;
use devbridge_core::Config;
use devbridge_core::job::JobEvent;
use devbridge_core::virtual_printer::{VirtualPrinter, slugify};
use devbridge_dashboard::state::AppState;
use devbridge_server::dispatch::DispatchService;
use devbridge_server::ipp_service::IppServer;
use devbridge_server::printer_reconciler::{build_default, reconciler_loop};
use devbridge_server::queue::JobQueue;
use devbridge_server::storage::Storage;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, broadcast};
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};
use uuid::Uuid;

/// Initialise tracing and start all subsystems based on the configuration.
///
/// Writes logs to `<data_dir>/logs/service.log` (daily-rolled) AND stderr.
/// File logging is unconditional so `RUST_LOG` / installer wrappers aren't
/// required to capture audit trail (serial bridge byte counts, gRPC state,
/// Ghostscript output, etc.). Scheduled-task SYSTEM installs otherwise
/// discard stderr and we lose all visibility into the running service.
pub async fn run(config: Config, config_path: Option<PathBuf>) -> Result<()> {
    let log_dir = PathBuf::from(&config.general.data_dir).join("logs");
    // Best-effort: if mkdir fails, fall back to stderr-only below.
    let _ = std::fs::create_dir_all(&log_dir);
    // Daily rotation with 14-file retention so a long-running retail
    // install doesn't grow the log directory forever. ~14 days is enough
    // to diagnose incidents reported the next Monday without blowing up
    // tiny C: volumes.
    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("service")
        .filename_suffix("log")
        .max_log_files(14)
        .build(&log_dir)
        .expect("failed to build rolling file appender");
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.general.log_level));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_ansi(false).with_writer(file_writer))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    // Leak the guard so the non-blocking writer keeps running for the life
    // of the process. Without this, dropping `file_guard` flushes & stops
    // the writer thread and subsequent log lines vanish.
    std::mem::forget(file_guard);

    info!(mode = %config.general.mode, log_dir = %log_dir.display(), "Starting DevBridge service");

    match config.general.mode.as_str() {
        "server" => run_server(config, config_path).await,
        "client" => run_client(config, config_path).await,
        other => anyhow::bail!("Unknown mode: {other}"),
    }
}

async fn run_server(config: Config, config_path: Option<PathBuf>) -> Result<()> {
    let data_dir = PathBuf::from(&config.general.data_dir);
    let spool_dir = PathBuf::from(&config.server.spool_dir);
    let dashboard_port = config.server.dashboard_port;
    let grpc_port = config.server.grpc_port;
    let ipp_port = config.server.ipp_port;

    // Ensure directories exist
    tokio::fs::create_dir_all(&data_dir).await?;
    tokio::fs::create_dir_all(&spool_dir).await?;

    // Initialise storage and job queue
    let db_path = data_dir.join("devbridge.db");
    let storage = Storage::new(&db_path).context("Failed to open storage")?;
    let mut queue = JobQueue::new(storage).context("Failed to initialise job queue")?;

    // Job event broadcast channel (consumed by WebSocket clients)
    let (job_events_tx, _) = broadcast::channel::<JobEvent>(256);
    queue.set_job_events(job_events_tx.clone());

    // Wire the printer reconciler. The service is the single owner of
    // Windows-printer registration on the server: one PS1 spawn at startup
    // (catches reboots/upgrades/drift) plus one debounced spawn per
    // virtual-printer DB change (catches new client registrations).
    // set_reconciler_signal takes &mut self so it MUST run before Arc-wrap.
    // Reconciler failures are logged and swallowed; they never crash the service.
    // ipp_port is passed through so Windows printers point at THIS service's
    // IPP listener (production=631, E2E test instance=1631) — hardcoding 631
    // in the PS1 caused E2E submissions to route to the production server.
    let (reconciler_invoker, reconciler_tx, reconciler_rx) =
        build_default(data_dir.clone(), ipp_port);
    queue.set_reconciler_signal(reconciler_tx);

    let queue = Arc::new(queue);

    // Spawn the reconciler loop concurrently with the rest of startup.
    // The first (startup) invoke does not block dashboard/IPP/gRPC binding.
    {
        let queue_for_reconciler = Arc::clone(&queue);
        tokio::spawn(reconciler_loop(
            reconciler_rx,
            queue_for_reconciler,
            reconciler_invoker,
        ));
    }

    // Print job event broadcast channel (forwarded via WebSocket)
    let (print_event_tx, _) = broadcast::channel::<devbridge_core::job_event::PrintJobEvent>(256);

    // Clean slate: mark all clients offline on startup
    queue
        .set_all_clients_offline()
        .context("Failed to reset client states")?;

    // Seed default virtual printer from config if none exist
    let existing_vps = queue.list_virtual_printers()?;
    if existing_vps.is_empty() {
        let now = Utc::now();
        let default_vp = VirtualPrinter {
            id: Uuid::new_v4().to_string(),
            display_name: config.server.printer_name.clone(),
            ipp_name: slugify(&config.server.printer_name),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        queue.insert_virtual_printer(&default_vp)?;
        info!(
            display_name = %default_vp.display_name,
            ipp_name = %default_vp.ipp_name,
            "seeded default virtual printer from config"
        );
    }

    // Shared connected client count
    let connected_clients = Arc::new(AtomicU64::new(0));

    // IPP server — load all virtual printers
    let ipp_server = Arc::new(IppServer::new(
        ipp_port,
        Arc::clone(&queue),
        spool_dir.clone(),
    ));
    for vp in queue.list_virtual_printers()? {
        ipp_server.add_printer(&vp).await?;
    }

    // Serial bridge manager
    let serial_bridge = Arc::new(devbridge_server::serial_bridge::SerialBridgeManager::new(
        config.server.serial_bridges.clone(),
    ));
    if !config.server.serial_bridges.is_empty() {
        info!(
            count = config.server.serial_bridges.len(),
            "serial bridge mappings configured"
        );
    }

    // gRPC dispatch server
    let max_retries = config.jobs.max_retries;
    let retry_delay_secs = config.jobs.retry_delay_secs;
    let dispatch = DispatchService::new(
        Arc::clone(&queue),
        spool_dir,
        Arc::clone(&connected_clients),
        max_retries,
        retry_delay_secs,
        serial_bridge,
    );

    // Dashboard — with ipp_server for live printer name updates
    let mut app_state = AppState::new("server".into())
        .with_queue(Arc::clone(&queue))
        .with_ipp_server(Arc::clone(&ipp_server))
        .with_target_printer(config.server.printer_name.clone())
        .with_connected_clients(Arc::clone(&connected_clients))
        .with_job_events(job_events_tx.clone())
        .with_print_events(print_event_tx.clone());
    if let Some(path) = config_path {
        app_state = app_state.with_config_path(path);
    }
    let dashboard = devbridge_dashboard::build_router(app_state);
    let dashboard_listener = TcpListener::bind(format!("0.0.0.0:{dashboard_port}"))
        .await
        .context("Failed to bind dashboard port")?;
    info!(port = dashboard_port, "Dashboard listening");

    // Background task: requeue stale/failed jobs periodically
    let requeue_queue = Arc::clone(&queue);
    let retry_delay_secs = config.jobs.retry_delay_secs;
    let stale_timeout_secs = retry_delay_secs * 10;
    let requeue_task = async move {
        // Initial delay to let services stabilize
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(retry_delay_secs)).await;
            if let Ok(stale) = requeue_queue.get_stale_jobs(stale_timeout_secs) {
                for job in stale {
                    if job.retry_count < max_retries {
                        let _ = requeue_queue.requeue_job(&job.job_id, "stale: stuck in progress");
                    }
                }
            }
        }
    };

    tokio::select! {
        res = ipp_server.run() => {
            res.context("IPP server error")?;
        }
        res = dispatch.run(grpc_port) => {
            res.context("gRPC dispatch server error")?;
        }
        res = axum::serve(dashboard_listener, dashboard) => {
            res.context("Dashboard server error")?;
        }
        _ = requeue_task => {}
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down");
        }
    }

    Ok(())
}

async fn run_client(config: Config, config_path: Option<PathBuf>) -> Result<()> {
    let data_dir = PathBuf::from(&config.general.data_dir);
    let spool_dir = data_dir.join("spool");
    let dashboard_port = config.client.dashboard_port;

    // Refuse to start when target_printer / printer_address is invalid.
    // See crates/devbridge-client/src/startup_validation.rs and
    // docs/superpowers/specs/2026-04-10-installer-hardening-design.md
    devbridge_client::startup_validation::validate_client_config(&config.client)
        .context("client config validation failed")?;

    tokio::fs::create_dir_all(&spool_dir).await?;

    // Persistent storage for client job history
    let db_path = data_dir.join("devbridge.db");
    let storage = Storage::new(&db_path).context("Failed to open client storage")?;
    let mut queue = JobQueue::new(storage).context("Failed to initialise client job queue")?;

    // Job event broadcast channel (consumed by WebSocket clients)
    let (job_events_tx, _) = broadcast::channel::<JobEvent>(256);
    queue.set_job_events(job_events_tx.clone());
    let queue = Arc::new(queue);

    // Print job event broadcast channel (forwarded via WebSocket)
    let (print_event_tx, _) = broadcast::channel::<devbridge_core::job_event::PrintJobEvent>(256);

    // Shared target printer — updated from dashboard, read by receiver
    let target_printer = Arc::new(RwLock::new(config.client.target_printer.clone()));

    // Receiver (gRPC client)
    let receiver = devbridge_client::receiver::Receiver::new(&config.client);
    let receiver_spool = spool_dir.clone();
    let receiver_target = Arc::clone(&target_printer);
    let receiver_queue = Arc::clone(&queue);

    // Dashboard — now with queue for job history visibility
    let mut app_state = AppState::new("client".into())
        .with_shared_target_printer(Arc::clone(&target_printer))
        .with_queue(Arc::clone(&queue))
        .with_job_events(job_events_tx.clone())
        .with_print_events(print_event_tx.clone())
        .with_client_config(&config.client);
    if let Some(path) = config_path {
        app_state = app_state.with_config_path(path);
    }
    let dashboard = devbridge_dashboard::build_router(app_state);
    let dashboard_listener = TcpListener::bind(format!("0.0.0.0:{dashboard_port}"))
        .await
        .context("Failed to bind dashboard port")?;
    info!(port = dashboard_port, "Dashboard listening");

    tokio::select! {
        res = receiver.run(receiver_spool, receiver_target, Some(receiver_queue)) => {
            res.context("Receiver error")?;
        }
        res = axum::serve(dashboard_listener, dashboard) => {
            res.context("Dashboard server error")?;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down");
        }
    }

    Ok(())
}

/// Convert a display name to a URL-safe slug.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("DevBridge"), "devbridge");
        assert_eq!(
            slugify("Store A - Receipt Printer"),
            "store-a-receipt-printer"
        );
        assert_eq!(slugify("My Printer!"), "my-printer");
        assert_eq!(slugify("  spaces  "), "spaces");
    }
}
