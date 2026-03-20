use leptos::prelude::*;
use serde_json::Value;

use crate::components::status_badge::StatusBadge;

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

    view! {
        <div class="job-row">
            <span class="job-name">{name}</span>
            <span class="job-printer">{printer}</span>
            <StatusBadge status=status />
        </div>
    }
}
