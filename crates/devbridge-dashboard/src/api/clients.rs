use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use devbridge_core::client_registration::PairingState;
use devbridge_core::virtual_printer::{VirtualPrinter, normalize_driver_name, slugify};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/clients", get(list_clients))
        .route("/clients/{id}/approve", post(approve_client))
        .route("/clients/{id}/reject", post(reject_client))
}

async fn list_clients(State(state): State<AppState>) -> Json<Value> {
    let Some(queue) = &state.queue else {
        return Json(json!([]));
    };

    match queue.list_clients() {
        Ok(clients) => {
            let json_clients: Vec<Value> = clients
                .iter()
                .map(|c| {
                    json!({
                        "machine_id": c.machine_id,
                        "hostname": c.hostname,
                        "printer_names": c.printer_names,
                        "client_version": c.client_version,
                        "last_seen": c.last_seen.to_rfc3339(),
                        "is_online": c.is_online,
                        "pairing_state": c.pairing_state.to_string(),
                        "virtual_printer_name": c.virtual_printer_name,
                        "virtual_printer_driver": c.virtual_printer_driver,
                    })
                })
                .collect();
            Json(json!(json_clients))
        }
        Err(_) => Json(json!([])),
    }
}

async fn approve_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(queue) = &state.queue else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "no queue available"})),
        );
    };

    // Look up the client
    let client = match queue.get_client(&id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "client not found"})),
            );
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "database error"})),
            );
        }
    };

    // Already approved?
    if client.pairing_state == PairingState::Approved {
        return (StatusCode::OK, Json(json!({"status": "already_approved"})));
    }

    // Set pairing state to Approved
    if queue
        .update_pairing_state(&id, PairingState::Approved)
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to update pairing state"})),
        );
    }

    // Auto-create virtual printer if the client specified a name
    let mut vp_info: Option<Value> = None;
    let mut vp_error: Option<String> = None;
    if let Some(ref name) = client.virtual_printer_name {
        match normalize_driver_name(client.virtual_printer_driver.as_deref()) {
            Err(reason) => {
                // Loud and explicit: an unusable driver override must never
                // silently fall back to the IPP Class Driver (a label printer
                // would then print PDF garbage). No VP is created; the
                // operator fixes the client's virtual_printer_driver.
                tracing::error!(
                    client_id = %id,
                    virtual_printer = %name,
                    driver = ?client.virtual_printer_driver,
                    %reason,
                    "invalid virtual_printer_driver — virtual printer NOT created"
                );
                vp_error = Some(format!("invalid virtual_printer_driver: {reason}"));
            }
            Ok(driver) => {
                let now = Utc::now();
                let vp = VirtualPrinter {
                    id: Uuid::new_v4().to_string(),
                    display_name: name.clone(),
                    ipp_name: slugify(name),
                    paired_client_id: Some(id.clone()),
                    driver,
                    created_at: now,
                    updated_at: now,
                };

                // insert_virtual_printer signals the printer reconciler, which
                // is the ONLY owner of the server-side Windows printer (it
                // registers it with vp.effective_driver()). The old direct
                // `Add-Printer ... -PortName NUL` spawn here raced it (#88).
                if let Err(e) = queue.insert_virtual_printer(&vp) {
                    // Log but don't fail the approval
                    tracing::warn!(error = %e, "Failed to create virtual printer for client {id}: {name}");
                } else {
                    tracing::info!(
                        client_id = %id,
                        display_name = %vp.display_name,
                        ipp_name = %vp.ipp_name,
                        driver = %vp.effective_driver(),
                        "virtual printer auto-created on approval"
                    );
                    // Register in IPP server if available
                    if let Some(ipp) = &state.ipp_server {
                        let _ = ipp.add_printer(&vp).await;
                    }

                    vp_info = Some(json!({
                        "id": vp.id,
                        "display_name": vp.display_name,
                        "ipp_name": vp.ipp_name,
                        "driver": vp.driver,
                    }));
                }
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "status": "approved",
            "client_id": id,
            "virtual_printer": vp_info,
            "virtual_printer_error": vp_error,
        })),
    )
}

async fn reject_client(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let Some(queue) = &state.queue else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "no queue available"})),
        );
    };

    // Look up the client
    match queue.get_client(&id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "client not found"})),
            );
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "database error"})),
            );
        }
    };

    // Set pairing state to Rejected
    if queue
        .update_pairing_state(&id, PairingState::Rejected)
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to update pairing state"})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "status": "rejected",
            "client_id": id,
        })),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::state::AppState;

    fn test_state_with_queue() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = devbridge_server::JobQueue::new(storage).unwrap();
        (
            AppState::new("server".into()).with_queue(Arc::new(queue)),
            dir,
        )
    }

    #[tokio::test]
    async fn test_list_clients_empty() {
        let (state, _dir) = test_state_with_queue();
        let app = crate::build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clients")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_list_clients_with_registered_client() {
        let (state, _dir) = test_state_with_queue();
        let queue = state.queue.as_ref().unwrap();

        let reg = devbridge_core::client_registration::ClientRegistration {
            machine_id: "test-mc".into(),
            hostname: "test-host".into(),
            printer_names: vec!["Printer1".into()],
            client_version: "0.1.0".into(),
            last_seen: chrono::Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Approved,
            virtual_printer_name: None,
            virtual_printer_driver: None,
        };
        queue.upsert_client(&reg).unwrap();

        let app = crate::build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clients")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);

        let client = arr[0].as_object().unwrap();
        assert!(client.contains_key("machine_id"));
        assert!(client.contains_key("hostname"));
        assert!(client.contains_key("printer_names"));
        assert!(client.contains_key("client_version"));
        assert!(client.contains_key("last_seen"));
        assert!(client.contains_key("is_online"));
        assert_eq!(client["machine_id"], "test-mc");
        assert_eq!(client["hostname"], "test-host");
        assert!(client["is_online"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_list_clients_includes_pairing_state() {
        let (state, _dir) = test_state_with_queue();
        let queue = state.queue.as_ref().unwrap();

        let reg = devbridge_core::client_registration::ClientRegistration {
            machine_id: "ps-test".into(),
            hostname: "host-ps".into(),
            printer_names: vec![],
            client_version: "0.1.0".into(),
            last_seen: chrono::Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Pending,
            virtual_printer_name: Some("My Printer".into()),
            virtual_printer_driver: None,
        };
        queue.upsert_client(&reg).unwrap();

        let app = crate::build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clients")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);

        let client = &arr[0];
        assert_eq!(client["pairing_state"], "pending");
        assert_eq!(client["virtual_printer_name"], "My Printer");
    }

    #[tokio::test]
    async fn test_approve_client_creates_virtual_printer() {
        let (state, _dir) = test_state_with_queue();
        let queue = state.queue.as_ref().unwrap();

        let reg = devbridge_core::client_registration::ClientRegistration {
            machine_id: "approve-test".into(),
            hostname: "host-approve".into(),
            printer_names: vec!["Printer1".into()],
            client_version: "0.1.0".into(),
            last_seen: chrono::Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Pending,
            virtual_printer_name: Some("Store A Printer".into()),
            virtual_printer_driver: None,
        };
        queue.upsert_client(&reg).unwrap();

        let app = crate::build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clients/approve-test/approve")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "approved");
        assert_eq!(json["client_id"], "approve-test");
        assert!(json["virtual_printer"].is_object());

        let vp = &json["virtual_printer"];
        assert_eq!(vp["display_name"], "Store A Printer");
        assert_eq!(vp["ipp_name"], "store-a-printer");
        // No override → no driver (reconciler uses Microsoft IPP Class Driver)
        assert!(vp["driver"].is_null());
        assert!(json["virtual_printer_error"].is_null());

        // Verify pairing state was updated in DB
        let updated = queue.get_client("approve-test").unwrap().unwrap();
        assert_eq!(
            updated.pairing_state,
            devbridge_core::client_registration::PairingState::Approved
        );
    }

    async fn approve(state: &AppState, machine_id: &str) -> serde_json::Value {
        let app = crate::build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/clients/{machine_id}/approve"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    fn pending_client(
        machine_id: &str,
        vp_name: &str,
        driver: Option<&str>,
    ) -> devbridge_core::client_registration::ClientRegistration {
        devbridge_core::client_registration::ClientRegistration {
            machine_id: machine_id.into(),
            hostname: "SPISSKA-PC".into(),
            printer_names: vec!["TSC ML241P".into()],
            client_version: "0.8.40".into(),
            last_seen: chrono::Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Pending,
            virtual_printer_name: Some(vp_name.into()),
            virtual_printer_driver: driver.map(String::from),
        }
    }

    #[tokio::test]
    async fn test_approve_creates_vp_with_client_driver_override() {
        let (state, _dir) = test_state_with_queue();
        let queue = state.queue.as_ref().unwrap();
        queue
            .upsert_client(&pending_client(
                "spisska-client",
                "spisska stitky",
                Some(" TSC ML241P "),
            ))
            .unwrap();

        let json = approve(&state, "spisska-client").await;
        assert_eq!(json["status"], "approved");
        assert_eq!(json["virtual_printer"]["driver"], "TSC ML241P");

        // Persisted (trimmed) — this is what the reconciler JSON carries.
        let vps = queue.list_virtual_printers().unwrap();
        assert_eq!(vps.len(), 1);
        assert_eq!(vps[0].display_name, "spisska stitky");
        assert_eq!(vps[0].paired_client_id.as_deref(), Some("spisska-client"));
        assert_eq!(vps[0].driver.as_deref(), Some("TSC ML241P"));

        // /api/clients shows what the client asked for.
        let app = crate::build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clients")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let clients: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(clients[0]["virtual_printer_driver"], " TSC ML241P ");
    }

    #[tokio::test]
    async fn test_approve_with_invalid_driver_creates_no_vp() {
        let (state, _dir) = test_state_with_queue();
        let queue = state.queue.as_ref().unwrap();
        queue
            .upsert_client(&pending_client(
                "evil-client",
                "evil",
                Some("TSC\" /r \"http://x"),
            ))
            .unwrap();

        let json = approve(&state, "evil-client").await;
        // Approval itself still succeeds …
        assert_eq!(json["status"], "approved");
        // … but no virtual printer, and the reason is surfaced.
        assert!(json["virtual_printer"].is_null());
        assert!(
            json["virtual_printer_error"]
                .as_str()
                .unwrap()
                .contains("invalid virtual_printer_driver"),
            "{json}"
        );
        assert!(queue.list_virtual_printers().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_reject_client() {
        let (state, _dir) = test_state_with_queue();
        let queue = state.queue.as_ref().unwrap();

        let reg = devbridge_core::client_registration::ClientRegistration {
            machine_id: "reject-test".into(),
            hostname: "host-reject".into(),
            printer_names: vec![],
            client_version: "0.1.0".into(),
            last_seen: chrono::Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Pending,
            virtual_printer_name: None,
            virtual_printer_driver: None,
        };
        queue.upsert_client(&reg).unwrap();

        let app = crate::build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clients/reject-test/reject")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "rejected");
        assert_eq!(json["client_id"], "reject-test");

        // Verify pairing state was updated in DB
        let updated = queue.get_client("reject-test").unwrap().unwrap();
        assert_eq!(
            updated.pairing_state,
            devbridge_core::client_registration::PairingState::Rejected
        );
    }

    #[tokio::test]
    async fn test_approve_nonexistent_client_404() {
        let (state, _dir) = test_state_with_queue();

        let app = crate::build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clients/nonexistent/approve")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 404);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "client not found");
    }
}
