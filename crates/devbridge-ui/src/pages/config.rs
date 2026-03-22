use leptos::prelude::*;

use crate::api;
use crate::components::header::PageHeader;

#[component]
pub fn ConfigPage() -> impl IntoView {
    let config = LocalResource::new(|| api::fetch_config());

    view! {
        <PageHeader title="Configuration" />

        <div class="card">
            {move || {
                config.read().as_ref().map(|res| {
                    match &**res {
                        Ok(cfg) => {
                            let formatted = serde_json::to_string_pretty(cfg)
                                .unwrap_or_else(|_| "{}".to_string());
                            view! {
                                <pre style="white-space: pre-wrap; font-family: 'Cascadia Code', 'Fira Code', monospace; font-size: 0.875rem;">
                                    {formatted}
                                </pre>
                            }.into_any()
                        }
                        Err(e) => view! {
                            <p style="color: var(--danger)">{format!("Error loading config: {e}")}</p>
                        }.into_any(),
                    }
                })
            }}
        </div>
    }
}
