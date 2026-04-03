use leptos::prelude::*;
use serde_json::Value;

use crate::api;
use crate::components::header::PageHeader;
use crate::components::status_badge::StatusBadge;
use crate::components::time_display::{
    CurrentTime, TimeWithSeconds, date_group_label, format_time_ago,
};

#[component]
pub fn DashboardPage() -> impl IntoView {
    let config = LocalResource::new(|| api::fetch_config());

    view! {
        {move || {
            config.read().as_ref().map(|res| {
                match &**res {
                    Ok(cfg) => {
                        let mode = cfg.get("mode")
                            .and_then(|m| m.as_str())
                            .unwrap_or("server")
                            .to_string();
                        if mode == "client" {
                            view! { <ClientDashboardView /> }.into_any()
                        } else {
                            view! { <ServerDashboardView /> }.into_any()
                        }
                    }
                    Err(_) => view! { <ServerDashboardView /> }.into_any(),
                }
            })
        }}
    }
}

// ---------------------------------------------------------------------------
// Shared: job data signal + WebSocket live updates
// ---------------------------------------------------------------------------

type JobList = Vec<(Value, Vec<Value>)>;

/// Fetch initial jobs and wire up WebSocket for live updates.
/// Returns an `RwSignal<JobList>` that updates reactively.
/// The WS loop is cancelled when the component is cleaned up.
fn use_live_jobs() -> RwSignal<JobList> {
    let jobs = RwSignal::new(Vec::<(Value, Vec<Value>)>::new());

    // Initial fetch
    let jobs_init = jobs;
    leptos::task::spawn_local(async move {
        if let Ok(list) = api::fetch_jobs_with_events().await {
            jobs_init.set(list);
        }
    });

    // Cancellation flag for WS loop (Arc<AtomicBool> is Send+Sync for on_cleanup)
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancelled_cleanup = cancelled.clone();
    on_cleanup(move || {
        cancelled_cleanup.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    // WebSocket live updates with cancellation
    let jobs_ws = jobs;
    leptos::task::spawn_local(async move {
        ws_update_loop(jobs_ws, cancelled).await;
    });

    jobs
}

async fn ws_update_loop(
    jobs: RwSignal<JobList>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use futures_util::StreamExt;

    loop {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        match api::connect_ws() {
            Ok(ws) => {
                let (_write, mut read) = ws.split();
                while let Some(msg) = read.next().await {
                    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    match msg {
                        Ok(gloo_net::websocket::Message::Text(text)) => {
                            handle_ws_message(&text, jobs);
                        }
                        Err(_) => break,
                        _ => {}
                    }
                }
            }
            Err(_) => {}
        }
        // Reconnect after 5 seconds
        gloo_timers::future::TimeoutFuture::new(5_000).await;
    }
}

fn handle_ws_message(text: &str, jobs: RwSignal<JobList>) {
    let Ok(event) = serde_json::from_str::<Value>(text) else {
        return;
    };

    let event_type = event
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("");

    match event_type {
        "created" => {
            // Fetch the new job's full data and prepend
            leptos::task::spawn_local(async move {
                if let Ok(list) = api::fetch_jobs_with_events().await {
                    jobs.set(list);
                }
            });
        }
        "state_changed" => {
            let job_id = event
                .get("job_id")
                .and_then(|j| j.as_str())
                .unwrap_or("")
                .to_string();
            let new_state = event
                .get("new_state")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if !job_id.is_empty() && !new_state.is_empty() {
                let now = js_sys::Date::new_0().to_iso_string().as_string().unwrap_or_default();
                jobs.update(|list| {
                    for (job, events) in list.iter_mut() {
                        if job.get("id").and_then(|v| v.as_str()) == Some(&job_id) {
                            job["status"] = Value::String(new_state.clone());
                            // Append synthetic event so timeline updates without refetch
                            events.push(serde_json::json!({
                                "job_id": job_id,
                                "stage": new_state,
                                "success": true,
                                "detail": "",
                                "timestamp": now,
                            }));
                            break;
                        }
                    }
                });
            }
        }
        // PrintJobEvent — append to the matching job's events
        _ => {
            let job_id = event
                .get("job_id")
                .and_then(|j| j.as_str())
                .unwrap_or("")
                .to_string();
            if !job_id.is_empty() {
                jobs.update(|list| {
                    for (job, events) in list.iter_mut() {
                        if job.get("id").and_then(|v| v.as_str()) == Some(&job_id) {
                            events.push(event.clone());
                            break;
                        }
                    }
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared: "ago" refresh tick (30s timer, only for display)
// ---------------------------------------------------------------------------

fn use_ago_tick() -> ReadSignal<u32> {
    let (tick, set_tick) = signal(0u32);
    leptos::task::spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(30_000).await;
            set_tick.update(|n| *n = n.wrapping_add(1));
        }
    });
    tick
}

// ---------------------------------------------------------------------------
// Shared: status color helpers
// ---------------------------------------------------------------------------

fn status_color(status: &str) -> &'static str {
    match status {
        "completed" => "var(--success, #22c55e)",
        "failed" => "var(--danger, #ef4444)",
        "printing" | "processing" | "downloading" | "rendering" | "sending" => {
            "var(--info, #3b82f6)"
        }
        _ => "var(--border, #555)",
    }
}

fn status_icon(success: bool, is_last: bool, job_status: &str) -> &'static str {
    if !success {
        "\u{274C}" // red X
    } else if is_last
        && (job_status == "printing"
            || job_status == "processing"
            || job_status == "downloading"
            || job_status == "rendering"
            || job_status == "sending")
    {
        "\u{23F3}" // hourglass for in-progress
    } else {
        "\u{2705}" // green check
    }
}

// ---------------------------------------------------------------------------
// Shared: Job Card component
// ---------------------------------------------------------------------------

#[component]
fn JobCard(
    job: Value,
    events: Vec<Value>,
    ago_tick: ReadSignal<u32>,
    #[prop(optional)] show_reprint: Option<Box<dyn Fn(String, String) + 'static>>,
) -> impl IntoView {
    let id = job
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let short_id = if id.len() > 8 {
        id[..8].to_string()
    } else {
        id.clone()
    };
    let name = job
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled")
        .to_string();
    let status = job
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let created_at = job
        .get("created_at")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let color = status_color(&status);
    let can_reprint = status == "completed" || status == "failed";

    let event_count = events.len();

    // For "ago" display that updates with tick
    let created_at_ago = created_at.clone();

    view! {
        <div
            class="card"
            style:margin-bottom="0.75rem"
            style:border-left=format!("4px solid {color}")
            style:padding="0.75rem 1rem"
        >
            // Header row: timestamp + status + name + reprint
            <div style="display: flex; align-items: center; gap: 0.75rem; margin-bottom: 0.25rem">
                // Large timestamp
                <div style="min-width: 6rem">
                    {if !created_at.is_empty() {
                        let ts = created_at.clone();
                        Some(view! {
                            <div style:font-family="monospace" style:font-size="1.4em" style:font-weight="bold" style:color=color>
                                <TimeWithSeconds datetime=ts />
                            </div>
                            <div style="font-size: 0.8em; color: var(--text-muted)">
                                {move || {
                                    let _ = ago_tick.get();
                                    format_time_ago(&created_at_ago)
                                }}
                            </div>
                        })
                    } else {
                        None
                    }}
                </div>

                // Status badge
                <StatusBadge status=status.clone() />

                // Document name
                <span style="flex: 1; font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap">
                    {name.clone()}
                </span>

                // Reprint button
                {if can_reprint {
                    if let Some(reprint_fn) = show_reprint {
                        let reprint_id = id.clone();
                        let reprint_name = name.clone();
                        Some(view! {
                            <button
                                class="btn btn-sm"
                                style="font-size: 0.8em; padding: 0.2rem 0.6rem"
                                on:click=move |_| {
                                    reprint_fn(reprint_id.clone(), reprint_name.clone());
                                }
                            >
                                "Reprint"
                            </button>
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }}
            </div>

            // Audit trail — always visible
            {if event_count > 0 {
                let job_status = status.clone();
                Some(view! {
                    <div style:margin-top="0.5rem" style:padding-left="0.5rem" style:border-left=format!("2px solid {color}") style:font-size="0.85em">
                        {events.iter().enumerate().map(|(i, evt)| {
                            let stage = evt["stage"].as_str().unwrap_or("unknown").to_string();
                            let success = evt["success"].as_bool().unwrap_or(false);
                            let detail = evt["detail"].as_str().unwrap_or("").to_string();
                            let timestamp = evt["timestamp"].as_str().unwrap_or("").to_string();
                            let is_last = i == event_count - 1;
                            let icon = status_icon(success, is_last, &job_status);
                            let detail_color = if !success { "var(--danger, #ef4444)" } else { "var(--text)" };

                            view! {
                                <div style="display: flex; gap: 0.5rem; padding: 0.15rem 0; font-family: monospace; font-size: 0.95em; color: var(--text-muted)">
                                    <span style="min-width: 5.5rem; text-align: right">
                                        <TimeWithSeconds datetime=timestamp />
                                    </span>
                                    <span>{icon}</span>
                                    <span style="min-width: 6rem; font-weight: 600">{stage}</span>
                                    <span style:color=detail_color>{detail}</span>
                                </div>
                            }
                        }).collect_view()}
                    </div>
                })
            } else {
                None
            }}

            // Job ID at bottom right
            <div style="text-align: right; font-family: monospace; font-size: 0.65em; color: var(--text-muted); opacity: 0.5; margin-top: 0.25rem">
                {short_id}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Pending Clients
// ---------------------------------------------------------------------------

#[component]
fn PendingClients(refresh: RwSignal<u32>) -> impl IntoView {
    let clients = LocalResource::new(move || {
        let _ = refresh.get();
        api::fetch_clients()
    });

    view! {
        {move || {
            clients.read().as_ref().and_then(|res| {
                match &**res {
                    Ok(all_clients) => {
                        let pending: Vec<Value> = all_clients
                            .iter()
                            .filter(|c| {
                                c.get("pairing_state")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("")
                                    == "pending"
                            })
                            .cloned()
                            .collect();

                        if pending.is_empty() {
                            return None;
                        }

                        let count = pending.len();

                        Some(view! {
                            <div
                                class="card"
                                style="margin-bottom: 1rem; border-left: 4px solid #f59e0b; padding: 0.75rem 1rem"
                            >
                                <h4 style="margin: 0 0 0.75rem 0; color: #f59e0b">
                                    {format!("Pending Clients ({})", count)}
                                </h4>
                                {pending.into_iter().map(|client| {
                                    let machine_id = client
                                        .get("machine_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("unknown")
                                        .to_string();
                                    let hostname = client
                                        .get("hostname")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let printer_name = client
                                        .get("requested_printer_name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();

                                    let approve_id = machine_id.clone();
                                    let reject_id = machine_id.clone();

                                    let subtitle = match (hostname.is_empty(), printer_name.is_empty()) {
                                        (false, false) => format!("{hostname} \u{2014} {printer_name}"),
                                        (false, true) => hostname.clone(),
                                        (true, false) => printer_name.clone(),
                                        (true, true) => String::new(),
                                    };

                                    view! {
                                        <div style="display: flex; align-items: center; gap: 0.75rem; padding: 0.4rem 0; border-bottom: 1px solid var(--border)">
                                            <div style="flex: 1; min-width: 0">
                                                <div style="font-weight: 700; overflow: hidden; text-overflow: ellipsis; white-space: nowrap">
                                                    {machine_id.clone()}
                                                </div>
                                                {if !subtitle.is_empty() {
                                                    Some(view! {
                                                        <div style="font-size: 0.85em; color: var(--text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap">
                                                            {subtitle}
                                                        </div>
                                                    })
                                                } else {
                                                    None
                                                }}
                                            </div>
                                            <button
                                                class="btn btn-sm"
                                                style="background: #22c55e; color: #fff; border: none; padding: 0.25rem 0.75rem; border-radius: 4px; cursor: pointer; font-size: 0.85em"
                                                on:click={
                                                    let refresh = refresh;
                                                    move |_| {
                                                        let id = approve_id.clone();
                                                        let refresh = refresh;
                                                        leptos::task::spawn_local(async move {
                                                            let _ = api::approve_client(&id).await;
                                                            refresh.update(|n| *n = n.wrapping_add(1));
                                                        });
                                                    }
                                                }
                                            >
                                                "Approve"
                                            </button>
                                            <button
                                                class="btn btn-sm"
                                                style="background: #ef4444; color: #fff; border: none; padding: 0.25rem 0.75rem; border-radius: 4px; cursor: pointer; font-size: 0.85em"
                                                on:click={
                                                    let refresh = refresh;
                                                    move |_| {
                                                        let id = reject_id.clone();
                                                        let refresh = refresh;
                                                        leptos::task::spawn_local(async move {
                                                            let _ = api::reject_client(&id).await;
                                                            refresh.update(|n| *n = n.wrapping_add(1));
                                                        });
                                                    }
                                                }
                                            >
                                                "Reject"
                                            </button>
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        })
                    }
                    Err(_) => None,
                }
            })
        }}
    }
}

// ---------------------------------------------------------------------------
// Server Dashboard
// ---------------------------------------------------------------------------

#[component]
fn ServerDashboardView() -> impl IntoView {
    let status = LocalResource::new(|| api::fetch_status());
    let jobs = use_live_jobs();
    let ago_tick = use_ago_tick();
    let pending_refresh = RwSignal::new(0u32);

    view! {
        <PageHeader title="Dashboard" />

        // Compact stats bar
        <div class="card" style="margin-bottom: 1rem; padding: 0.5rem 1rem; font-size: 0.9em; display: flex; gap: 1.5rem; align-items: center">
            {move || {
                status.read().as_ref().map(|res| {
                    match &**res {
                        Ok(v) => {
                            let mode = v.get("mode").and_then(|m| m.as_str()).unwrap_or("server").to_string();
                            let clients = v.get("connected_clients").and_then(|c| c.as_u64()).unwrap_or(0);
                            let jobs_today = v.get("jobs_today").and_then(|j| j.as_u64()).unwrap_or(0);
                            view! {
                                <span style="font-weight: 700">{mode}</span>
                                <span>{format!("{clients} client{}", if clients != 1 { "s" } else { "" })}</span>
                                <span>{format!("{jobs_today} jobs today")}</span>
                                <StatusBadge status="online".to_string() />
                            }.into_any()
                        }
                        Err(_) => view! {
                            <span style="font-weight: 700">"server"</span>
                            <StatusBadge status="offline".to_string() />
                        }.into_any(),
                    }
                })
            }}
        </div>

        // Pending clients (only shown when there are pending clients)
        <PendingClients refresh=pending_refresh />

        // Recent Jobs heading
        <h3 style="margin-bottom: 0.75rem">"Recent Jobs"</h3>

        // Timeline feed — last 10 jobs
        {move || {
            let list = jobs.get();
            if list.is_empty() {
                view! {
                    <div class="card" style="text-align: center; padding: 2rem; color: var(--text-muted)">
                        <p style="font-size: 1.2em; margin-bottom: 0.5rem">"No jobs yet"</p>
                        <p>"Jobs will appear here when documents are printed."</p>
                    </div>
                }.into_any()
            } else {
                list.into_iter().take(10).map(|(job, events)| {
                    view! {
                        <JobCard job=job events=events ago_tick=ago_tick />
                    }
                }).collect_view().into_any()
            }
        }}
    }
}

// ---------------------------------------------------------------------------
// Client Dashboard
// ---------------------------------------------------------------------------

#[component]
fn ClientDashboardView() -> impl IntoView {
    let status = LocalResource::new(|| api::fetch_status());
    let jobs = use_live_jobs();
    let ago_tick = use_ago_tick();
    let (feedback, set_feedback) = signal(Option::<(String, bool)>::None);

    let reprint = move |job_id: String, name: String| {
        let set_feedback = set_feedback;
        let jobs = jobs;
        leptos::task::spawn_local(async move {
            match api::reprint_job(&job_id).await {
                Ok(_) => {
                    set_feedback.set(Some((format!("Reprinting: {name}"), true)));
                    // Refresh all jobs to pick up the new reprint job
                    if let Ok(list) = api::fetch_jobs_with_events().await {
                        jobs.set(list);
                    }
                }
                Err(e) => {
                    set_feedback.set(Some((format!("Reprint failed: {e}"), false)));
                }
            }
            // Auto-clear feedback after 5 seconds
            gloo_timers::future::TimeoutFuture::new(5_000).await;
            set_feedback.set(None);
        });
    };

    let clear_handler = move |_| {
        let jobs = jobs;
        leptos::task::spawn_local(async move {
            let _ = api::clear_jobs().await;
            jobs.set(Vec::new());
        });
    };

    view! {
        // Title bar: "Print Jobs" + clock + "Clear History"
        <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 0.5rem">
            <h2 style="margin: 0">"Print Jobs"</h2>
            <div style="display: flex; align-items: center; gap: 1rem">
                <CurrentTime />
                <button
                    class="btn btn-sm"
                    style="font-size: 0.8em; color: var(--danger)"
                    on:click=clear_handler
                >
                    "Clear History"
                </button>
            </div>
        </div>

        // Identity card
        <div class="card" style="margin-bottom: 1rem; padding: 0.75rem 1rem">
            {move || {
                status.read().as_ref().map(|res| {
                    match &**res {
                        Ok(v) => {
                            let client_id = v.get("client_id").and_then(|c| c.as_str()).unwrap_or("unknown").to_string();
                            let printer_name = v.get("printer_display_name").and_then(|p| p.as_str()).unwrap_or("unknown").to_string();
                            let printer_addr = v.get("printer_address").and_then(|a| a.as_str()).unwrap_or("").to_string();
                            let backend = v.get("print_backend").and_then(|b| b.as_str()).unwrap_or("windows_spooler").to_string();
                            let server_addr = v.get("server_address").and_then(|s| s.as_str()).unwrap_or("unknown").to_string();
                            let is_online = v.get("connected_clients").and_then(|c| c.as_u64()).unwrap_or(0) > 0
                                || v.get("status").and_then(|s| s.as_str()) == Some("running");

                            view! {
                                <div style="display: flex; flex-direction: column; gap: 0.25rem">
                                    <div style="display: flex; justify-content: space-between; align-items: center">
                                        <span style="font-weight: 700; font-size: 1.1em">
                                            "DevBridge Client: " {client_id}
                                        </span>
                                        <StatusBadge status=if is_online { "online".to_string() } else { "offline".to_string() } />
                                    </div>
                                    <div style="color: var(--text-muted); font-size: 0.9em">
                                        "Printer: " <strong>{printer_name}</strong>
                                        {if !printer_addr.is_empty() {
                                            format!(" ({}) \u{2014} {}", printer_addr, backend)
                                        } else {
                                            format!(" \u{2014} {}", backend)
                                        }}
                                    </div>
                                    <div style="display: flex; justify-content: space-between; align-items: center">
                                        <div style="color: var(--text-muted); font-size: 0.85em">
                                            "Server: " {server_addr}
                                        </div>
                                        <a href="/printers" style="color: var(--primary); text-decoration: none; font-size: 0.9em">"Change Printer"</a>
                                    </div>
                                </div>
                            }.into_any()
                        }
                        Err(_) => view! {
                            <div style="display: flex; justify-content: space-between; align-items: center">
                                <div style="display: flex; align-items: center; gap: 0.75rem">
                                    <StatusBadge status="offline".to_string() />
                                    <span style="font-weight: 600">"DevBridge Client \u{2014} disconnected"</span>
                                </div>
                                <a href="/printers" style="color: var(--primary); text-decoration: none; font-size: 0.9em">"Change Printer"</a>
                            </div>
                        }.into_any(),
                    }
                })
            }}
        </div>

        // Feedback toast
        {move || {
            feedback.get().map(|(msg, ok)| {
                let color = if ok { "var(--success)" } else { "var(--danger)" };
                view! {
                    <div
                        class="card"
                        style:padding="0.5rem 1rem"
                        style:margin-bottom="1rem"
                        style:border-left=format!("3px solid {color}")
                        style:color=color
                    >
                        {msg}
                    </div>
                }
            })
        }}

        // Job timeline — all jobs, grouped by date
        {move || {
            let list = jobs.get();
            if list.is_empty() {
                view! {
                    <div class="card" style="text-align: center; padding: 2rem; color: var(--text-muted)">
                        <p style="font-size: 1.2em; margin-bottom: 0.5rem">"No print jobs yet"</p>
                        <p>"Jobs will appear here when documents are printed."</p>
                    </div>
                }.into_any()
            } else {
                // Group by date
                let mut groups: Vec<(String, Vec<(Value, Vec<Value>)>)> = Vec::new();
                for (job, events) in list.into_iter() {
                    let created = job.get("created_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let label = date_group_label(created);
                    if let Some(last) = groups.last_mut() {
                        if last.0 == label {
                            last.1.push((job, events));
                            continue;
                        }
                    }
                    groups.push((label, vec![(job, events)]));
                }

                groups.into_iter().map(move |(label, group_jobs)| {
                    let reprint = reprint.clone();
                    view! {
                        <div style="margin-bottom: 1.5rem">
                            <h4 style="color: var(--text-muted); font-size: 0.85em; text-transform: uppercase; letter-spacing: 0.05em; margin-bottom: 0.5rem; padding-bottom: 0.25rem; border-bottom: 1px solid var(--border)">
                                {label}
                            </h4>
                            {group_jobs.into_iter().map(|(job, events)| {
                                let reprint = reprint.clone();
                                let reprint_box: Box<dyn Fn(String, String) + 'static> =
                                    Box::new(move |id, name| reprint(id, name));
                                view! {
                                    <JobCard job=job events=events ago_tick=ago_tick show_reprint=reprint_box />
                                }
                            }).collect_view()}
                        </div>
                    }
                }).collect_view().into_any()
            }
        }}
    }
}
