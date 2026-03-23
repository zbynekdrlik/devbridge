use leptos::prelude::*;

use crate::api;
use crate::components::header::PageHeader;
use crate::components::status_badge::StatusBadge;

#[component]
pub fn PrintersPage() -> impl IntoView {
    let config = LocalResource::new(|| api::fetch_config());

    view! {
        <PageHeader title="Printers" />
        {move || {
            config.read().as_ref().map(|res| {
                match &**res {
                    Ok(cfg) => {
                        let mode = cfg.get("mode")
                            .and_then(|m| m.as_str())
                            .unwrap_or("server")
                            .to_string();
                        if mode == "client" {
                            view! { <ClientPrintersView /> }.into_any()
                        } else {
                            view! { <ServerPrintersView /> }.into_any()
                        }
                    }
                    Err(_) => view! { <ServerPrintersView /> }.into_any(),
                }
            })
        }}
    }
}

/// Server mode: virtual printers management + registered clients table.
#[component]
fn ServerPrintersView() -> impl IntoView {
    let (refresh_signal, set_refresh) = signal(0u32);
    let (feedback, set_feedback) = signal(Option::<(String, bool)>::None);

    let virtual_printers = LocalResource::new(move || {
        let _ = refresh_signal.get();
        api::fetch_virtual_printers()
    });

    let clients = LocalResource::new(move || {
        let _ = refresh_signal.get();
        api::fetch_clients()
    });

    // Add VP form signals
    let (show_add_form, set_show_add_form) = signal(false);
    let (new_display_name, set_new_display_name) = signal(String::new());
    let (new_ipp_name, set_new_ipp_name) = signal(String::new());

    // Inline edit state: which VP id is being edited
    let (editing_id, set_editing_id) = signal(Option::<String>::None);
    let (edit_display_name, set_edit_display_name) = signal(String::new());
    let (edit_ipp_name, set_edit_ipp_name) = signal(String::new());

    let add_virtual_printer = move || {
        let name = new_display_name.get();
        let ipp = new_ipp_name.get();
        let set_refresh = set_refresh.clone();
        let set_feedback = set_feedback.clone();
        let set_show_add_form = set_show_add_form.clone();
        let set_new_display_name = set_new_display_name.clone();
        let set_new_ipp_name = set_new_ipp_name.clone();
        leptos::task::spawn_local(async move {
            match api::create_virtual_printer(&name, &ipp).await {
                Ok(_) => {
                    set_feedback.set(Some((format!("Created virtual printer '{name}'"), true)));
                    set_show_add_form.set(false);
                    set_new_display_name.set(String::new());
                    set_new_ipp_name.set(String::new());
                    set_refresh.update(|n| *n += 1);
                }
                Err(e) => {
                    set_feedback.set(Some((format!("Error: {e}"), false)));
                }
            }
        });
    };

    let delete_vp = move |id: String, name: String| {
        let set_refresh = set_refresh.clone();
        let set_feedback = set_feedback.clone();
        leptos::task::spawn_local(async move {
            match api::delete_virtual_printer(&id).await {
                Ok(()) => {
                    set_feedback.set(Some((format!("Deleted '{name}'"), true)));
                    set_refresh.update(|n| *n += 1);
                }
                Err(e) => {
                    set_feedback.set(Some((format!("Error: {e}"), false)));
                }
            }
        });
    };

    let pair_client = move |vp_id: String, client_id: Option<String>| {
        let set_refresh = set_refresh.clone();
        let set_feedback = set_feedback.clone();
        leptos::task::spawn_local(async move {
            let paired = client_id.as_deref();
            match api::update_virtual_printer(&vp_id, None, None, Some(paired)).await {
                Ok(_) => {
                    let msg = match paired {
                        Some(id) => format!("Paired to client {id}"),
                        None => "Unlinked client".to_string(),
                    };
                    set_feedback.set(Some((msg, true)));
                    set_refresh.update(|n| *n += 1);
                }
                Err(e) => {
                    set_feedback.set(Some((format!("Error: {e}"), false)));
                }
            }
        });
    };

    let save_edit = move |id: String| {
        let name = edit_display_name.get();
        let ipp = edit_ipp_name.get();
        let set_refresh = set_refresh.clone();
        let set_feedback = set_feedback.clone();
        let set_editing_id = set_editing_id.clone();
        leptos::task::spawn_local(async move {
            match api::update_virtual_printer(&id, Some(&name), Some(&ipp), None).await {
                Ok(_) => {
                    set_feedback.set(Some((format!("Updated '{name}'"), true)));
                    set_editing_id.set(None);
                    set_refresh.update(|n| *n += 1);
                }
                Err(e) => {
                    set_feedback.set(Some((format!("Error: {e}"), false)));
                }
            }
        });
    };

    view! {
        // Feedback toast
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

        // Virtual Printers section
        <div class="card" style="margin-bottom: 1.5rem">
            <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 1rem">
                <h3 style="margin: 0">"Virtual Printers"</h3>
                <button
                    class="btn btn-sm"
                    on:click=move |_| set_show_add_form.update(|v| *v = !*v)
                >
                    {move || if show_add_form.get() { "Cancel" } else { "Add Printer" }}
                </button>
            </div>

            // Add form
            {move || {
                if show_add_form.get() {
                    let add = add_virtual_printer.clone();
                    Some(view! {
                        <div style="display: flex; gap: 0.5rem; margin-bottom: 1rem; flex-wrap: wrap">
                            <input
                                type="text"
                                placeholder="Display Name (e.g. Store A - Receipt)"
                                style="flex: 1; min-width: 200px; padding: 0.4rem; background: var(--bg-secondary); color: var(--text-primary); border: 1px solid var(--border)"
                                prop:value=move || new_display_name.get()
                                on:input=move |ev| set_new_display_name.set(event_target_value(&ev))
                            />
                            <input
                                type="text"
                                placeholder="IPP Name (e.g. store-a-receipt)"
                                style="flex: 1; min-width: 200px; padding: 0.4rem; background: var(--bg-secondary); color: var(--text-primary); border: 1px solid var(--border)"
                                prop:value=move || new_ipp_name.get()
                                on:input=move |ev| set_new_ipp_name.set(event_target_value(&ev))
                            />
                            <button class="btn btn-sm" on:click=move |_| add()>"Create"</button>
                        </div>
                    })
                } else {
                    None
                }
            }}

            <table>
                <thead>
                    <tr>
                        <th>"Name"</th>
                        <th>"IPP Name"</th>
                        <th>"Paired Client"</th>
                        <th>"Actions"</th>
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        let client_list_for_dropdown = clients.read().as_ref().and_then(|res| {
                            match &**res {
                                Ok(list) => Some(list.clone()),
                                Err(_) => None,
                            }
                        }).unwrap_or_default();

                        virtual_printers.read().as_ref().map(|res| {
                            match &**res {
                                Ok(vp_list) => {
                                    if vp_list.is_empty() {
                                        view! {
                                            <tr>
                                                <td colspan="4" style="text-align:center; color: var(--text-muted)">
                                                    "No virtual printers configured."
                                                </td>
                                            </tr>
                                        }.into_any()
                                    } else {
                                        let delete = delete_vp.clone();
                                        let pair = pair_client.clone();
                                        let cl = client_list_for_dropdown.clone();
                                        vp_list.iter().cloned().map(move |vp| {
                                            let id = vp.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                            let display_name = vp.get("display_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                            let ipp_name = vp.get("ipp_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                            let paired = vp.get("paired_client_id").and_then(|v| v.as_str()).map(|s| s.to_string());

                                            let delete = delete.clone();
                                            let pair = pair.clone();
                                            let del_id = id.clone();
                                            let del_name = display_name.clone();
                                            let edit_id = id.clone();
                                            let edit_dn = display_name.clone();
                                            let edit_in = ipp_name.clone();
                                            let save_id = id.clone();
                                            let dropdown_clients = cl.clone();
                                            let pair_vp_id = id.clone();
                                            let current_paired = paired.clone().unwrap_or_default();

                                            let is_editing = move || editing_id.get().as_deref() == Some(edit_id.as_str());

                                            view! {
                                                <tr>
                                                    <td>
                                                        {let dn = edit_dn.clone(); move || {
                                                            if is_editing() {
                                                                view! {
                                                                    <input
                                                                        type="text"
                                                                        style="width: 100%; padding: 0.2rem; background: var(--bg-secondary); color: var(--text-primary); border: 1px solid var(--border)"
                                                                        prop:value=move || edit_display_name.get()
                                                                        on:input=move |ev| set_edit_display_name.set(event_target_value(&ev))
                                                                    />
                                                                }.into_any()
                                                            } else {
                                                                view! { <strong>{dn.clone()}</strong> }.into_any()
                                                            }
                                                        }}
                                                    </td>
                                                    <td style="font-family: monospace; font-size: 0.9em">
                                                        {let inp = edit_in.clone(); move || {
                                                            if is_editing() {
                                                                view! {
                                                                    <input
                                                                        type="text"
                                                                        style="width: 100%; padding: 0.2rem; background: var(--bg-secondary); color: var(--text-primary); border: 1px solid var(--border); font-family: monospace"
                                                                        prop:value=move || edit_ipp_name.get()
                                                                        on:input=move |ev| set_edit_ipp_name.set(event_target_value(&ev))
                                                                    />
                                                                }.into_any()
                                                            } else {
                                                                view! { <span>{inp.clone()}</span> }.into_any()
                                                            }
                                                        }}
                                                    </td>
                                                    <td>
                                                        {
                                                            let pair = pair.clone();
                                                            let pvid = pair_vp_id.clone();
                                                            let cp = current_paired.clone();
                                                            let dc = dropdown_clients.clone();
                                                            move || {
                                                                let pair = pair.clone();
                                                                let pvid = pvid.clone();
                                                                let cp = cp.clone();
                                                                let dc = dc.clone();
                                                                view! {
                                                                    <select
                                                                        style="padding: 0.2rem; background: var(--bg-secondary); color: var(--text-primary); border: 1px solid var(--border)"
                                                                        on:change=move |ev| {
                                                                            let val = event_target_value(&ev);
                                                                            if val.is_empty() {
                                                                                pair(pvid.clone(), None);
                                                                            } else {
                                                                                pair(pvid.clone(), Some(val));
                                                                            }
                                                                        }
                                                                    >
                                                                        <option value="" selected=cp.is_empty()>"Not paired"</option>
                                                                        {dc.iter().map(|c| {
                                                                            let mid = c.get("machine_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                                                            let hostname = c.get("hostname").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                                                                            let selected = mid == cp;
                                                                            let label = format!("{hostname} ({mid})");
                                                                            view! {
                                                                                <option value=mid selected=selected>{label}</option>
                                                                            }
                                                                        }).collect_view()}
                                                                    </select>
                                                                }
                                                            }
                                                        }
                                                    </td>
                                                    <td>
                                                        {
                                                            let del_id = del_id.clone();
                                                            let del_name = del_name.clone();
                                                            let delete = delete.clone();
                                                            let save_id = save_id.clone();
                                                            let edit_id2 = id.clone();
                                                            let dn_for_edit = display_name.clone();
                                                            let in_for_edit = ipp_name.clone();
                                                            move || {
                                                                if is_editing() {
                                                                    let sid = save_id.clone();
                                                                    view! {
                                                                        <button
                                                                            class="btn btn-sm"
                                                                            style="color: var(--success); margin-right: 0.25rem"
                                                                            on:click=move |_| save_edit(sid.clone())
                                                                        >"Save"</button>
                                                                        <button
                                                                            class="btn btn-sm"
                                                                            on:click=move |_| set_editing_id.set(None)
                                                                        >"Cancel"</button>
                                                                    }.into_any()
                                                                } else {
                                                                    let did = del_id.clone();
                                                                    let dnm = del_name.clone();
                                                                    let delete = delete.clone();
                                                                    let eid = edit_id2.clone();
                                                                    let edn = dn_for_edit.clone();
                                                                    let ein = in_for_edit.clone();
                                                                    view! {
                                                                        <button
                                                                            class="btn btn-sm"
                                                                            style="margin-right: 0.25rem"
                                                                            on:click=move |_| {
                                                                                set_edit_display_name.set(edn.clone());
                                                                                set_edit_ipp_name.set(ein.clone());
                                                                                set_editing_id.set(Some(eid.clone()));
                                                                            }
                                                                        >"Edit"</button>
                                                                        <button
                                                                            class="btn btn-sm"
                                                                            style="color: var(--danger)"
                                                                            on:click=move |_| delete(did.clone(), dnm.clone())
                                                                        >"Delete"</button>
                                                                    }.into_any()
                                                                }
                                                            }
                                                        }
                                                    </td>
                                                </tr>
                                            }
                                        }).collect_view().into_any()
                                    }
                                }
                                Err(e) => view! {
                                    <tr>
                                        <td colspan="4" style="text-align:center; color: var(--danger)">
                                            {format!("Error: {e}")}
                                        </td>
                                    </tr>
                                }.into_any(),
                            }
                        })
                    }}
                </tbody>
            </table>
        </div>

        // Registered Clients section
        <div class="card">
            <h3 style="margin-bottom: 1rem">"Registered Clients"</h3>
            <table>
                <thead>
                    <tr>
                        <th>"Hostname"</th>
                        <th>"Machine ID"</th>
                        <th>"Printers"</th>
                        <th>"Status"</th>
                        <th>"Last Seen"</th>
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        clients.read().as_ref().map(|res| {
                            match &**res {
                                Ok(client_list) => {
                                    if client_list.is_empty() {
                                        view! {
                                            <tr>
                                                <td colspan="5" style="text-align:center; color: var(--text-muted)">
                                                    "No clients registered yet. Clients auto-register when they connect."
                                                </td>
                                            </tr>
                                        }.into_any()
                                    } else {
                                        client_list.iter().cloned().map(move |client| {
                                            let hostname = client.get("hostname").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                                            let machine_id = client.get("machine_id").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                                            let printers = client.get("printer_names")
                                                .and_then(|v| v.as_array())
                                                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                                                .unwrap_or_else(|| "-".to_string());
                                            let is_online = client.get("is_online").and_then(|v| v.as_bool()).unwrap_or(false);
                                            let last_seen = client.get("last_seen").and_then(|v| v.as_str()).unwrap_or("-").to_string();
                                            let status = if is_online { "online" } else { "offline" };

                                            let mid_display = if machine_id.len() > 12 {
                                                format!("{}...", &machine_id[..12])
                                            } else {
                                                machine_id.clone()
                                            };

                                            view! {
                                                <tr>
                                                    <td><strong>{hostname}</strong></td>
                                                    <td style="font-family: monospace; font-size: 0.85em" title=machine_id.clone()>
                                                        {mid_display}
                                                    </td>
                                                    <td>{printers}</td>
                                                    <td><StatusBadge status=status.to_string() /></td>
                                                    <td style="font-size: 0.85em">{last_seen}</td>
                                                </tr>
                                            }
                                        }).collect_view().into_any()
                                    }
                                }
                                Err(e) => view! {
                                    <tr>
                                        <td colspan="5" style="text-align:center; color: var(--danger)">
                                            {format!("Error: {e}")}
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

/// Client mode: local printer list with target selection (unchanged behavior).
#[component]
fn ClientPrintersView() -> impl IntoView {
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
