use leptos::prelude::*;

use crate::api;
use crate::components::header::PageHeader;
use crate::components::status_badge::StatusBadge;

#[component]
pub fn JobsPage() -> impl IntoView {
    let jobs = LocalResource::new(|| api::fetch_jobs());

    view! {
        <PageHeader title="Jobs" />

        <div class="card">
            <table>
                <thead>
                    <tr>
                        <th>"ID"</th>
                        <th>"Name"</th>
                        <th>"Printer"</th>
                        <th>"Status"</th>
                        <th>"Created"</th>
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        jobs.read().as_ref().map(|res| {
                            match res {
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
                                            let id = job.get("id")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("-")
                                                .to_string();
                                            let name = job.get("name")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("Untitled")
                                                .to_string();
                                            let printer = job.get("printer")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("-")
                                                .to_string();
                                            let status = job.get("status")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("unknown")
                                                .to_string();
                                            let created = job.get("created_at")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("-")
                                                .to_string();

                                            view! {
                                                <tr>
                                                    <td>{id}</td>
                                                    <td>{name}</td>
                                                    <td>{printer}</td>
                                                    <td><StatusBadge status=status /></td>
                                                    <td>{created}</td>
                                                </tr>
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
