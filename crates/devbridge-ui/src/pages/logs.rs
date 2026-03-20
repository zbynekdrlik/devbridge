use leptos::prelude::*;

use crate::components::header::PageHeader;

#[component]
pub fn LogsPage() -> impl IntoView {
    let (logs, _set_logs) = signal(vec![
        ("info", "DevBridge started"),
        ("info", "Listening on 0.0.0.0:3000"),
        ("info", "WebSocket server ready"),
        ("warn", "No printers detected yet"),
        ("info", "Waiting for connections..."),
    ]);

    view! {
        <PageHeader title="Logs" />

        <div class="card">
            <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 1rem;">
                <span style="color: var(--text-muted); font-size: 0.875rem;">
                    "Live log viewer (placeholder)"
                </span>
                <button class="btn btn-primary">"Connect WebSocket"</button>
            </div>
            <div class="log-viewer">
                {move || {
                    logs.get().iter().map(|(level, msg)| {
                        let class = match *level {
                            "warn" => "log-line log-warn",
                            "error" => "log-line log-error",
                            _ => "log-line log-info",
                        };
                        let prefix = match *level {
                            "warn" => "[WARN]",
                            "error" => "[ERROR]",
                            _ => "[INFO]",
                        };
                        view! {
                            <div class=class>
                                {format!("{prefix} {msg}")}
                            </div>
                        }
                    }).collect_view()
                }}
            </div>
        </div>
    }
}
