use leptos::prelude::*;

use crate::api;
use crate::components::header::PageHeader;
use crate::components::status_badge::StatusBadge;

#[component]
pub fn PrintersPage() -> impl IntoView {
    let printers = LocalResource::new(|| api::fetch_printers());

    view! {
        <PageHeader title="Printers" />

        <div class="card">
            <table>
                <thead>
                    <tr>
                        <th>"Name"</th>
                        <th>"Driver"</th>
                        <th>"Status"</th>
                        <th>"Jobs"</th>
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
                                                <td colspan="4" style="text-align:center; color: var(--text-muted)">
                                                    "No printers found."
                                                </td>
                                            </tr>
                                        }.into_any()
                                    } else {
                                        printer_list.iter().cloned().map(|printer| {
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

                                            view! {
                                                <tr>
                                                    <td>{name}</td>
                                                    <td>{driver}</td>
                                                    <td><StatusBadge status=status /></td>
                                                    <td>{jobs.to_string()}</td>
                                                </tr>
                                            }
                                        }).collect_view().into_any()
                                    }
                                }
                                Err(e) => view! {
                                    <tr>
                                        <td colspan="4" style="text-align:center; color: var(--danger)">
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
