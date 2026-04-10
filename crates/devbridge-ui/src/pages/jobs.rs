use leptos::prelude::*;

use crate::api;
use crate::components::header::PageHeader;
use crate::components::status_badge::StatusBadge;
use crate::components::time_display::{TimeDisplay, TimeWithSeconds, format_time_ago};

#[component]
pub fn JobsPage() -> impl IntoView {
    let jobs = LocalResource::new(|| api::fetch_jobs());
    let (selected_job, set_selected_job) = signal(None::<String>);

    view! {
        <PageHeader title="Jobs" />

        <div class="card">
            <table>
                <thead>
                    <tr>
                        <th>"Time"</th>
                        <th>"User"</th>
                        <th>"Printer"</th>
                        <th>"Status"</th>
                        <th>"Ago"</th>
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        jobs.read().as_ref().map(|res| {
                            match &**res {
                                Ok(job_list) => {
                                    if job_list.is_empty() {
                                        view! {
                                            <tr>
                                                <td colspan="5" style="text-align:center; color: var(--text-muted)">
                                                    "No jobs found."
                                                </td>
                                            </tr>
                                        }.into_any()
                                    } else {
                                        job_list.iter().cloned().map(|job| {
                                            let row_id = job.get("id")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("-")
                                                .to_string();
                                            let timeline_id = row_id.clone();
                                            let user = job.get("requesting_user")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .unwrap_or("\u{2014}")
                                                .to_string();
                                            let printer = job.get("printer")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .unwrap_or("\u{2014}")
                                                .to_string();
                                            let status = job.get("status")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("unknown")
                                                .to_string();
                                            let created = job.get("created_at")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string();
                                            let ago = format_time_ago(&created);

                                            view! {
                                                <tr
                                                    style="cursor: pointer;"
                                                    on:click=move |_| {
                                                        let clicked = row_id.clone();
                                                        set_selected_job.update(|sel| {
                                                            if sel.as_deref() == Some(&clicked) {
                                                                *sel = None;
                                                            } else {
                                                                *sel = Some(clicked);
                                                            }
                                                        });
                                                    }
                                                >
                                                    <td><TimeWithSeconds datetime=created /></td>
                                                    <td>{user}</td>
                                                    <td>{printer}</td>
                                                    <td><StatusBadge status=status /></td>
                                                    <td>{ago}</td>
                                                </tr>
                                                {move || {
                                                    if selected_job.get().as_deref() == Some(&timeline_id) {
                                                        Some(view! { <JobEventTimeline job_id=timeline_id.clone() /> })
                                                    } else {
                                                        None
                                                    }
                                                }}
                                            }
                                        }).collect_view().into_any()
                                    }
                                }
                                Err(e) => view! {
                                    <tr>
                                        <td colspan="5" style="text-align:center; color: var(--danger)">
                                            {format!("Error loading jobs: {e}")}
                                        </td>
                                    </tr>
                                }.into_any(),
                            }
                        })
                    }}
                </tbody>
            </table>
        </div>
    }
}

#[component]
fn JobEventTimeline(job_id: String) -> impl IntoView {
    let id = job_id.clone();
    let events = LocalResource::new(move || {
        let id = id.clone();
        async move { api::fetch_job_events(&id).await.unwrap_or_default() }
    });

    view! {
        <tr>
            <td colspan="5" class="timeline-cell">
                <Suspense fallback=move || view! { <p>"Loading events..."</p> }>
                    {move || events.get().map(|evts| {
                        if evts.is_empty() {
                            view! { <p class="text-muted">"No audit events recorded"</p> }.into_any()
                        } else {
                            view! {
                                <div class="job-timeline">
                                    {evts.iter().map(|evt| {
                                        let stage = evt["stage"].as_str().unwrap_or("unknown").to_string();
                                        let success = evt["success"].as_bool().unwrap_or(false);
                                        let detail = evt["detail"].as_str().unwrap_or("").to_string();
                                        let timestamp = evt["timestamp"].as_str().unwrap_or("").to_string();
                                        let icon = if success { "✅" } else { "❌" };

                                        view! {
                                            <div class="timeline-item">
                                                <span class="timeline-time">
                                                    <TimeDisplay datetime=timestamp />
                                                </span>
                                                <span class="timeline-icon">{icon}</span>
                                                <span class="timeline-stage">{stage}</span>
                                                <span class="timeline-detail">{detail}</span>
                                            </div>
                                        }
                                    }).collect_view()}
                                </div>
                            }.into_any()
                        }
                    })}
                </Suspense>
            </td>
        </tr>
    }
}
