use leptos::prelude::*;

#[component]
pub fn StatusBadge(#[prop(into)] status: String) -> impl IntoView {
    let class = match status.as_str() {
        "completed" | "online" | "ready" | "success" => "badge badge-success",
        "pending" | "queued" | "waiting" => "badge badge-warning",
        "failed" | "error" | "offline" => "badge badge-danger",
        _ => "badge badge-info",
    };

    view! {
        <span class=class>{status}</span>
    }
}
