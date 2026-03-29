use leptos::prelude::*;

use crate::api;
use crate::components::header::PageHeader;
use crate::components::job_card::JobCard;
use crate::components::status_badge::StatusBadge;
use crate::components::time_display::{TimeOnly, date_group_label};

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
    let jobs = LocalResource::new(|| api::fetch_jobs());

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
            <div class="job-list">
                {move || {
                    jobs.read().as_ref().map(|res| {
                        match &**res {
                            Ok(job_list) => {
                                let items: Vec<_> = job_list.iter().take(10).cloned().collect();
                                if items.is_empty() {
                                    view! { <p class="text-muted">"No jobs yet."</p> }.into_any()
                                } else {
                                    items.into_iter().map(|job| {
                                        view! { <JobCard job=job /> }
                                    }).collect_view().into_any()
                                }
                            }
                            Err(e) => view! { <p class="text-muted">{format!("Error: {e}")}</p> }.into_any(),
                        }
                    })
                }}
            </div>
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
        api::fetch_jobs()
    });

    // Auto-refresh every 10 seconds
    let set_refresh_timer = set_refresh.clone();
    leptos::task::spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(10_000).await;
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
        <PageHeader title="Print Jobs" />

        // Connection status bar
        <div class="card" style="margin-bottom: 1rem; padding: 0.75rem 1rem; display: flex; justify-content: space-between; align-items: center">
            <div style="display: flex; align-items: center; gap: 0.75rem">
                {move || {
                    status.read().as_ref().map(|res| {
                        match &**res {
                            Ok(_) => view! { <StatusBadge status="online".to_string() /> }.into_any(),
                            Err(_) => view! { <StatusBadge status="offline".to_string() /> }.into_any(),
                        }
                    })
                }}
                <span style="font-weight: 600">"DevBridge Client"</span>
            </div>
            <a href="/printers" style="color: var(--primary); text-decoration: none; font-size: 0.9em">"Change Printer"</a>
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
                                let mut groups: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
                                for job in job_list.iter() {
                                    let created = job.get("created_at")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let label = date_group_label(created);
                                    if let Some(last) = groups.last_mut() {
                                        if last.0 == label {
                                            last.1.push(job.clone());
                                            continue;
                                        }
                                    }
                                    groups.push((label, vec![job.clone()]));
                                }

                                let reprint = reprint.clone();
                                groups.into_iter().map(move |(label, group_jobs)| {
                                    let reprint = reprint.clone();
                                    view! {
                                        <div style="margin-bottom: 1.5rem">
                                            <h4 style="color: var(--text-muted); font-size: 0.85em; text-transform: uppercase; letter-spacing: 0.05em; margin-bottom: 0.5rem; padding-bottom: 0.25rem; border-bottom: 1px solid var(--border)">
                                                {label}
                                            </h4>
                                            {group_jobs.into_iter().map(|job| {
                                                let reprint = reprint.clone();
                                                let id = job.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                                let name = job.get("name").and_then(|v| v.as_str()).unwrap_or("Untitled").to_string();
                                                let status = job.get("status").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                                                let created_at = job.get("created_at").and_then(|v| v.as_str()).unwrap_or("").to_string();

                                                // Show reprint button for completed/failed jobs
                                                let can_reprint = status == "completed" || status == "failed";
                                                let reprint_id = id.clone();
                                                let reprint_name = name.clone();

                                                view! {
                                                    <div class="job-timeline-item" style="display: flex; align-items: center; gap: 0.75rem; padding: 0.5rem 0; border-bottom: 1px solid var(--border)">
                                                        <StatusBadge status=status />
                                                        <span style="flex: 1; font-weight: 500">{name}</span>
                                                        {if !created_at.is_empty() {
                                                            Some(view! {
                                                                <span style="color: var(--text-muted); font-size: 0.85em">
                                                                    <TimeOnly datetime=created_at />
                                                                </span>
                                                            })
                                                        } else {
                                                            None
                                                        }}
                                                        {if can_reprint {
                                                            let reprint = reprint.clone();
                                                            Some(view! {
                                                                <button
                                                                    class="btn btn-sm"
                                                                    style="font-size: 0.8em"
                                                                    on:click=move |_| reprint(reprint_id.clone(), reprint_name.clone())
                                                                >
                                                                    "Reprint"
                                                                </button>
                                                            })
                                                        } else {
                                                            None
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
