use devbridge_ui_util::job_status_label;
use leptos::prelude::*;

#[component]
pub fn StatusBadge(#[prop(into)] status: String) -> impl IntoView {
    let class = match status.as_str() {
        "completed" | "online" | "ready" | "success" => "badge badge-success",
        // #114 `unconfirmed` = „možno vytlačené": a warning, not a failure
        "pending" | "queued" | "waiting" | "unconfirmed" => "badge badge-warning",
        "failed" | "error" | "offline" => "badge badge-danger",
        _ => "badge badge-info",
    };
    let label = job_status_label(&status).to_string();

    view! {
        <span class=class>{label}</span>
    }
}
