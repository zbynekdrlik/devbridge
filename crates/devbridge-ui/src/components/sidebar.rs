use devbridge_ui_util::version_label;
use leptos::prelude::*;

use crate::api;

#[component]
pub fn Sidebar() -> impl IntoView {
    let config = LocalResource::new(|| api::fetch_config());

    view! {
        <nav class="sidebar">
            <div class="sidebar-brand">
                <h1>"DevBridge"</h1>
                // Deployed version (issue #82), from the /api/config response
                // this sidebar already loads. Loading or error -> nothing.
                {move || {
                    config.read().as_ref().and_then(|res| {
                        match &**res {
                            Ok(cfg) => cfg.get("version")
                                .and_then(|v| v.as_str())
                                .and_then(version_label),
                            Err(_) => None,
                        }
                    }).map(|label| view! {
                        <div class="app-version" data-testid="app-version">{label}</div>
                    })
                }}
            </div>
            <a href="/">"Dashboard"</a>
            {move || {
                let is_client = config.read().as_ref().map(|res| {
                    match &**res {
                        Ok(cfg) => cfg.get("mode")
                            .and_then(|m| m.as_str())
                            .unwrap_or("server") == "client",
                        Err(_) => false,
                    }
                }).unwrap_or(false);

                if is_client {
                    view! {
                        <a href="/printers">"Printers"</a>
                    }.into_any()
                } else {
                    view! {
                        <a href="/jobs">"Jobs"</a>
                        <a href="/printers">"Printers"</a>
                        <a href="/config">"Config"</a>
                        <a href="/logs">"Logs"</a>
                    }.into_any()
                }
            }}
        </nav>
    }
}
