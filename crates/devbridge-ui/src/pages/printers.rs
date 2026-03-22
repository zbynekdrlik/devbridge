use leptos::prelude::*;

use crate::api;
use crate::components::header::PageHeader;
use crate::components::status_badge::StatusBadge;

#[component]
pub fn PrintersPage() -> impl IntoView {
    let (refresh_signal, set_refresh) = signal(0u32);
    let printers = LocalResource::new(move || {
        let _ = refresh_signal.get();
        api::fetch_printers()
    });

    let (feedback, set_feedback) = signal(Option::<(String, bool)>::None);

    let select_printer = move |name: String| {
        let set_refresh = set_refresh.clone();
        let set_feedback = set_feedback.clone();
        leptos::task::spawn_local(async move {
            match api::set_target_printer(&name).await {
                Ok(_) => {
                    set_feedback.set(Some((format!("Target set to {name}"), true)));
                    set_refresh.update(|n| *n += 1);
                }
                Err(e) => {
                    set_feedback.set(Some((format!("Error: {e}"), false)));
                }
            }
        });
    };

    view! {
        <PageHeader title="Printers" />

        {move || {
            feedback.get().map(|(msg, ok)| {
                let color = if ok { "var(--success)" } else { "var(--danger)" };
                view! {
                    <div
                        class="card"
                        style:padding="0.5rem 1rem"
                        style:margin-bottom="1rem"
                        style:border-left=format!("3px solid {color}")
                        style:color=color
                    >
                        {msg}
                    </div>
                }
            })
        }}

        <div class="card">
            <table>
                <thead>
                    <tr>
                        <th>"Name"</th>
                        <th>"Driver"</th>
                        <th>"Status"</th>
                        <th>"Jobs"</th>
                        <th>"Target"</th>
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        printers.read().as_ref().map(|res| {
                            match &**res {
                                Ok(printer_list) => {
                                    if printer_list.is_empty() {
                                        view! {
                                            <tr>
                                                <td colspan="5" style="text-align:center; color: var(--text-muted)">
                                                    "No printers found."
                                                </td>
                                            </tr>
                                        }.into_any()
                                    } else {
                                        let select = select_printer.clone();
                                        printer_list.iter().cloned().map(move |printer| {
                                            let name = printer.get("name")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("Unknown")
                                                .to_string();
                                            let driver = printer.get("driver")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("-")
                                                .to_string();
                                            let status = printer.get("status")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("unknown")
                                                .to_string();
                                            let jobs = printer.get("jobs")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0);
                                            let is_target = printer.get("is_target")
                                                .and_then(|v| v.as_bool())
                                                .unwrap_or(false);

                                            let select = select.clone();
                                            let select_name = name.clone();

                                            view! {
                                                <tr style:background-color=if is_target { "var(--bg-highlight, rgba(59, 130, 246, 0.1))" } else { "transparent" }>
                                                    <td>
                                                        <strong>{name}</strong>
                                                    </td>
                                                    <td>{driver}</td>
                                                    <td><StatusBadge status=status /></td>
                                                    <td>{jobs.to_string()}</td>
                                                    <td>
                                                        {if is_target {
                                                            view! { <span style="color: var(--success); font-weight: 600">"Active"</span> }.into_any()
                                                        } else {
                                                            let select = select.clone();
                                                            let name = select_name.clone();
                                                            view! {
                                                                <button
                                                                    class="btn btn-sm"
                                                                    on:click=move |_| select(name.clone())
                                                                >
                                                                    "Select"
                                                                </button>
                                                            }.into_any()
                                                        }}
                                                    </td>
                                                </tr>
                                            }
                                        }).collect_view().into_any()
                                    }
                                }
                                Err(e) => view! {
                                    <tr>
                                        <td colspan="5" style="text-align:center; color: var(--danger)">
                                            {format!("Error loading printers: {e}")}
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
