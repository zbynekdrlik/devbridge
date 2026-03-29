use leptos::prelude::*;
use serde_json::Value;

use crate::components::status_badge::StatusBadge;
use crate::components::time_display::TimeOnly;

#[component]
pub fn JobCard(job: Value) -> impl IntoView {
    let name = job
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled")
        .to_string();
    let printer = job
        .get("printer")
        .and_then(|v| v.as_str())
        .unwrap_or("-")
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

    view! {
        <div class="job-row">
            <span class="job-name">{name}</span>
            <span class="job-printer">{printer}</span>
            <StatusBadge status=status />
            {if !created_at.is_empty() {
                Some(view! { <span class="job-time"><TimeOnly datetime=created_at /></span> })
            } else {
                None
            }}
        </div>
    }
}
