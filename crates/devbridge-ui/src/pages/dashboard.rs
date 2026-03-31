use leptos::prelude::*;

use crate::api;
use crate::components::header::PageHeader;
use crate::components::status_badge::StatusBadge;
use crate::components::time_display::{CurrentTime, TimeWithAgo, TimeWithSeconds, date_group_label};

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

/// Server mode: stat cards + recent jobs overview.
#[component]
fn ServerDashboardView() -> impl IntoView {
    let status = LocalResource::new(|| api::fetch_status());
    let jobs = LocalResource::new(|| api::fetch_jobs_with_events());

    view! {
        <PageHeader title="Dashboard" />

        <div class="card-grid">
            <div class="card">
                <div class="stat-label">"Mode"</div>
                <div class="stat-value">
                    {move || {
                        status.read().as_ref().map(|res| {
                            match &**res {
                                Ok(v) => v.get("mode")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("unknown")
                                    .to_string(),
                                Err(_) => "offline".to_string(),
                            }
                        }).unwrap_or_else(|| "loading...".to_string())
                    }}
                </div>
            </div>
            <div class="card">
                <div class="stat-label">"Connected Clients"</div>
                <div class="stat-value">
                    {move || {
                        status.read().as_ref().map(|res| {
                            match &**res {
                                Ok(v) => v.get("connected_clients")
                                    .and_then(|c| c.as_u64())
                                    .unwrap_or(0)
                                    .to_string(),
                                Err(_) => "0".to_string(),
                            }
                        }).unwrap_or_else(|| "-".to_string())
                    }}
                </div>
            </div>
            <div class="card">
                <div class="stat-label">"Jobs Today"</div>
                <div class="stat-value">
                    {move || {
                        status.read().as_ref().map(|res| {
                            match &**res {
                                Ok(v) => v.get("jobs_today")
                                    .and_then(|j| j.as_u64())
                                    .unwrap_or(0)
                                    .to_string(),
                                Err(_) => "0".to_string(),
                            }
                        }).unwrap_or_else(|| "-".to_string())
                    }}
                </div>
            </div>
            <div class="card">
                <div class="stat-label">"Status"</div>
                <div class="stat-value">
                    {move || {
                        status.read().as_ref().map(|res| {
                            match &**res {
                                Ok(_) => view! { <StatusBadge status="online".to_string() /> }.into_any(),
                                Err(_) => view! { <StatusBadge status="offline".to_string() /> }.into_any(),
                            }
                        })
                    }}
                </div>
            </div>
        </div>

        <div class="card">
            <h3 style="margin-bottom: 1rem">"Recent Jobs"</h3>
            {move || {
                jobs.read().as_ref().map(|res| {
                    match &**res {
                        Ok(job_list) => {
                            let items: Vec<_> = job_list.iter().take(10).cloned().collect();
                            if items.is_empty() {
                                view! { <p class="text-muted">"No jobs yet."</p> }.into_any()
                            } else {
                                items.into_iter().map(|(job, events)| {
                                    let short_id = job.get("id").and_then(|v| v.as_str()).map(|s| if s.len() > 8 { s[..8].to_string() } else { s.to_string() }).unwrap_or_default();
                                    let name = job.get("name").and_then(|v| v.as_str()).unwrap_or("Untitled").to_string();
                                    let status = job.get("status").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                                    let created_at = job.get("created_at").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    let has_events = !events.is_empty();
                                    let (expanded, set_expanded) = signal(false);

                                    view! {
                                        <div style="padding: 0.4rem 0; border-bottom: 1px solid var(--border)">
                                            <div
                                                style="display: flex; align-items: center; gap: 0.5rem; cursor: pointer"
                                                on:click=move |_| set_expanded.update(|v| *v = !*v)
                                            >
                                                <span style="font-family: monospace; font-size: 0.85em; min-width: 11rem; white-space: nowrap">
                                                    {if !created_at.is_empty() {
                                                        Some(view! { <TimeWithAgo datetime=created_at /> })
                                                    } else {
                                                        None
                                                    }}
                                                </span>
                                                <StatusBadge status=status />
                                                <span style="flex: 1; font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap">{name}</span>
                                                <span style="font-family: monospace; font-size: 0.7em; color: var(--text-muted); opacity: 0.5">{short_id}</span>
                                                {if has_events {
                                                    Some(view! {
                                                        <span style="font-size: 0.7em; color: var(--text-muted)">
                                                            {move || if expanded.get() { "\u{25B2}" } else { "\u{25BC}" }}
                                                        </span>
                                                    })
                                                } else {
                                                    None
                                                }}
                                            </div>
                                            {move || {
                                                if expanded.get() && has_events {
                                                    Some(view! {
                                                        <div style="margin-top: 0.3rem; margin-left: 0.5rem; padding-left: 0.5rem; border-left: 2px solid var(--border); font-size: 0.8em; color: var(--text-muted)">
                                                            {events.iter().map(|evt| {
                                                                let stage = evt["stage"].as_str().unwrap_or("unknown").to_string();
                                                                let success = evt["success"].as_bool().unwrap_or(false);
                                                                let detail = evt["detail"].as_str().unwrap_or("").to_string();
                                                                let timestamp = evt["timestamp"].as_str().unwrap_or("").to_string();
                                                                let icon = if success { "\u{2705}" } else { "\u{274C}" };

                                                                view! {
                                                                    <div style="display: flex; gap: 0.4rem; padding: 0.1rem 0; font-family: monospace; font-size: 0.95em">
                                                                        <span style="min-width: 5.5rem; text-align: right">
                                                                            <TimeWithSeconds datetime=timestamp />
                                                                        </span>
                                                                        <span>{icon}</span>
                                                                        <span style="min-width: 5rem; font-weight: 600">{stage}</span>
                                                                        <span style="color: var(--text)">{detail}</span>
                                                                    </div>
                                                                }
                                                            }).collect_view()}
                                                        </div>
                                                    })
                                                } else {
                                                    None
                                                }
                                            }}
                                        </div>
                                    }
                                }).collect_view().into_any()
                            }
                        }
                        Err(e) => view! { <p class="text-muted">{format!("Error: {e}")}</p> }.into_any(),
                    }
                })
            }}
        </div>
    }
}

/// Client mode: focused print timeline with reprint support.
#[component]
fn ClientDashboardView() -> impl IntoView {
    let status = LocalResource::new(|| api::fetch_status());
    let (refresh_signal, set_refresh) = signal(0u32);
    let (feedback, set_feedback) = signal(Option::<(String, bool)>::None);

    let jobs = LocalResource::new(move || {
        let _ = refresh_signal.get();
        api::fetch_jobs_with_events()
    });

    // Auto-refresh every 10 seconds
    let set_refresh_timer = set_refresh.clone();
    leptos::task::spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(3_000).await;
            set_refresh_timer.update(|n| *n += 1);
        }
    });

    let reprint = move |job_id: String, name: String| {
        let set_refresh = set_refresh.clone();
        let set_feedback = set_feedback.clone();
        leptos::task::spawn_local(async move {
            match api::reprint_job(&job_id).await {
                Ok(_) => {
                    set_feedback.set(Some((format!("Reprinting: {name}"), true)));
                    set_refresh.update(|n| *n += 1);
                }
                Err(e) => {
                    set_feedback.set(Some((format!("Reprint failed: {e}"), false)));
                }
            }
        });
    };

    view! {
        // Title bar with current time and clear button
        <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 0.5rem">
            <h2 style="margin: 0">"Print Jobs"</h2>
            <div style="display: flex; align-items: center; gap: 1rem">
                <CurrentTime />
                <button
                    class="btn btn-sm"
                    style="font-size: 0.8em; color: var(--danger)"
                    on:click=move |_| {
                        let set_refresh = set_refresh.clone();
                        leptos::task::spawn_local(async move {
                            let _ = api::clear_jobs().await;
                            set_refresh.update(|n| *n += 1);
                        });
                    }
                >
                    "Clear History"
                </button>
            </div>
        </div>

        // Identity header
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

        // Job timeline
        <div class="card">
            {move || {
                jobs.read().as_ref().map(|res| {
                    match &**res {
                        Ok(job_list) => {
                            if job_list.is_empty() {
                                view! {
                                    <div style="text-align: center; padding: 2rem; color: var(--text-muted)">
                                        <p style="font-size: 1.2em; margin-bottom: 0.5rem">"No print jobs yet"</p>
                                        <p>"Jobs will appear here when documents are printed."</p>
                                    </div>
                                }.into_any()
                            } else {
                                // Group jobs by date
                                let mut groups: Vec<(String, Vec<(serde_json::Value, Vec<serde_json::Value>)>)> = Vec::new();
                                for (job, events) in job_list.iter() {
                                    let created = job.get("created_at")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let label = date_group_label(created);
                                    if let Some(last) = groups.last_mut() {
                                        if last.0 == label {
                                            last.1.push((job.clone(), events.clone()));
                                            continue;
                                        }
                                    }
                                    groups.push((label, vec![(job.clone(), events.clone())]));
                                }

                                let reprint = reprint.clone();
                                groups.into_iter().map(move |(label, group_jobs)| {
                                    let reprint = reprint.clone();
                                    view! {
                                        <div style="margin-bottom: 1.5rem">
                                            <h4 style="color: var(--text-muted); font-size: 0.85em; text-transform: uppercase; letter-spacing: 0.05em; margin-bottom: 0.5rem; padding-bottom: 0.25rem; border-bottom: 1px solid var(--border)">
                                                {label}
                                            </h4>
                                            {group_jobs.into_iter().map(|(job, events)| {
                                                let reprint = reprint.clone();
                                                let id = job.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                                let short_id = if id.len() > 8 { id[..8].to_string() } else { id.clone() };
                                                let name = job.get("name").and_then(|v| v.as_str()).unwrap_or("Untitled").to_string();
                                                let status = job.get("status").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                                                let created_at = job.get("created_at").and_then(|v| v.as_str()).unwrap_or("").to_string();

                                                let can_reprint = status == "completed" || status == "failed";
                                                let reprint_id = id.clone();
                                                let reprint_name = name.clone();
                                                let has_events = !events.is_empty();

                                                // Collapsed by default, click to expand
                                                let (expanded, set_expanded) = signal(false);

                                                view! {
                                                    <div style="padding: 0.4rem 0; border-bottom: 1px solid var(--border)">
                                                        // Job header row — click to expand
                                                        <div
                                                            style="display: flex; align-items: center; gap: 0.5rem; cursor: pointer"
                                                            on:click=move |_| set_expanded.update(|v| *v = !*v)
                                                        >
                                                            // 1. Time with seconds + ago
                                                            <span style="font-family: monospace; font-size: 0.85em; min-width: 11rem; white-space: nowrap">
                                                                {if !created_at.is_empty() {
                                                                    Some(view! { <TimeWithAgo datetime=created_at /> })
                                                                } else {
                                                                    None
                                                                }}
                                                            </span>
                                                            // 2. Status
                                                            <StatusBadge status=status />
                                                            // 3. Name (flex)
                                                            <span style="flex: 1; font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap">{name}</span>
                                                            // 4. Short ID (muted, small)
                                                            <span style="font-family: monospace; font-size: 0.7em; color: var(--text-muted); opacity: 0.5">{short_id}</span>
                                                            // 5. Expand indicator
                                                            {if has_events {
                                                                Some(view! {
                                                                    <span style="font-size: 0.7em; color: var(--text-muted)">
                                                                        {move || if expanded.get() { "\u{25B2}" } else { "\u{25BC}" }}
                                                                    </span>
                                                                })
                                                            } else {
                                                                None
                                                            }}
                                                            // 6. Reprint (stop propagation so click doesn't toggle expand)
                                                            {if can_reprint {
                                                                let reprint = reprint.clone();
                                                                Some(view! {
                                                                    <button
                                                                        class="btn btn-sm"
                                                                        style="font-size: 0.75em; padding: 0.15rem 0.4rem"
                                                                        on:click=move |ev| {
                                                                            ev.stop_propagation();
                                                                            reprint(reprint_id.clone(), reprint_name.clone());
                                                                        }
                                                                    >
                                                                        "Reprint"
                                                                    </button>
                                                                })
                                                            } else {
                                                                None
                                                            }}
                                                        </div>
                                                        // Expanded audit timeline
                                                        {move || {
                                                            if expanded.get() && has_events {
                                                                Some(view! {
                                                                    <div style="margin-top: 0.3rem; margin-left: 0.5rem; padding-left: 0.5rem; border-left: 2px solid var(--border); font-size: 0.8em; color: var(--text-muted)">
                                                                        {events.iter().map(|evt| {
                                                                            let stage = evt["stage"].as_str().unwrap_or("unknown").to_string();
                                                                            let success = evt["success"].as_bool().unwrap_or(false);
                                                                            let detail = evt["detail"].as_str().unwrap_or("").to_string();
                                                                            let timestamp = evt["timestamp"].as_str().unwrap_or("").to_string();
                                                                            let icon = if success { "\u{2705}" } else { "\u{274C}" };

                                                                            view! {
                                                                                <div style="display: flex; gap: 0.4rem; padding: 0.1rem 0; font-family: monospace; font-size: 0.95em">
                                                                                    <span style="min-width: 5.5rem; text-align: right">
                                                                                        <TimeWithSeconds datetime=timestamp />
                                                                                    </span>
                                                                                    <span>{icon}</span>
                                                                                    <span style="min-width: 5rem; font-weight: 600">{stage}</span>
                                                                                    <span style="color: var(--text)">{detail}</span>
                                                                                </div>
                                                                            }
                                                                        }).collect_view()}
                                                                    </div>
                                                                })
                                                            } else {
                                                                None
                                                            }
                                                        }}
                                                    </div>
                                                }
                                            }).collect_view()}
                                        </div>
                                    }
                                }).collect_view().into_any()
                            }
                        }
                        Err(e) => view! {
                            <p style="color: var(--danger); padding: 1rem">{format!("Error: {e}")}</p>
                        }.into_any(),
                    }
                })
            }}
        </div>
    }
}
