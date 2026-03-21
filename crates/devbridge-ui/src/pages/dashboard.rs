use leptos::prelude::*;

use crate::api;
use crate::components::header::PageHeader;
use crate::components::job_card::JobCard;
use crate::components::status_badge::StatusBadge;

#[component]
pub fn DashboardPage() -> impl IntoView {
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
