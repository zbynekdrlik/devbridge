use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use devbridge_core::virtual_printer::{VirtualPrinter, slugify};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/virtual-printers", get(list_virtual_printers))
        .route("/virtual-printers", post(create_virtual_printer))
        .route("/virtual-printers/{id}", put(update_virtual_printer))
        .route("/virtual-printers/{id}", delete(delete_virtual_printer))
}

async fn list_virtual_printers(State(state): State<AppState>) -> Json<Value> {
    let Some(queue) = &state.queue else {
        return Json(json!([]));
    };

    match queue.list_virtual_printers() {
        Ok(vps) => {
            let json_vps: Vec<Value> = vps
                .iter()
                .map(|vp| {
                    json!({
                        "id": vp.id,
                        "display_name": vp.display_name,
                        "ipp_name": vp.ipp_name,
                        "paired_client_id": vp.paired_client_id,
                        "created_at": vp.created_at.to_rfc3339(),
                        "updated_at": vp.updated_at.to_rfc3339(),
                    })
                })
                .collect();
            Json(json!(json_vps))
        }
        Err(_) => Json(json!([])),
    }
}

#[derive(Deserialize)]
struct CreateRequest {
    display_name: String,
}

async fn create_virtual_printer(
    State(state): State<AppState>,
    Json(body): Json<CreateRequest>,
) -> Result<Json<Value>, StatusCode> {
    let Some(queue) = &state.queue else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    if body.display_name.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let name = body.display_name.trim().to_string();
    let now = Utc::now();
    let vp = VirtualPrinter {
        id: Uuid::new_v4().to_string(),
        ipp_name: slugify(&name),
        display_name: name,
        paired_client_id: None,
        created_at: now,
        updated_at: now,
    };

    queue
        .insert_virtual_printer(&vp)
        .map_err(|_| StatusCode::CONFLICT)?;

    Ok(Json(json!({
        "id": vp.id,
        "display_name": vp.display_name,
        "ipp_name": vp.ipp_name,
        "paired_client_id": vp.paired_client_id,
        "created_at": vp.created_at.to_rfc3339(),
        "updated_at": vp.updated_at.to_rfc3339(),
    })))
}

#[derive(Deserialize)]
struct UpdateRequest {
    display_name: Option<String>,
    paired_client_id: Option<Option<String>>,
}

async fn update_virtual_printer(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRequest>,
) -> Result<Json<Value>, StatusCode> {
    let Some(queue) = &state.queue else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    let mut vp = queue
        .get_virtual_printer(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let old_display_name = vp.display_name.clone();
    let old_ipp_name = vp.ipp_name.clone();
    let mut name_changed = false;

    if let Some(name) = body.display_name.filter(|n| *n != old_display_name) {
        vp.ipp_name = slugify(&name);
        vp.display_name = name;
        name_changed = true;
    }
    if let Some(client_id) = body.paired_client_id {
        vp.paired_client_id = client_id;
    }

    queue
        .update_virtual_printer(&vp)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // When name changes, update IPP service and Windows printer registration
    if name_changed {
        // Update IPP service in-memory registry
        if let Some(ipp) = &state.ipp_server {
            ipp.remove_printer(&old_ipp_name).await;
            let _ = ipp.add_printer(&vp).await;
        }

        // Re-register Windows printer with new name (server mode only)
        if cfg!(target_os = "windows") && state.mode == "server" {
            let new_name = vp.display_name.clone();
            let old_name = old_display_name.clone();
            tokio::task::spawn_blocking(move || {
                let script = format!(
                    r#"$old = Get-Printer -Name '{}' -ErrorAction SilentlyContinue; if ($old) {{ Remove-Printer -Name '{}' }}; $port = 'http://127.0.0.1:631/ipp/print'; rundll32.exe printui.dll,PrintUIEntry /if /b "{}" /r "$port" /m "Microsoft IPP Class Driver" /q"#,
                    old_name, old_name, new_name
                );
                let _ = std::process::Command::new("powershell")
                    .args(["-NoProfile", "-Command", &script])
                    .output();
            });
        }
    }

    Ok(Json(json!({
        "id": vp.id,
        "display_name": vp.display_name,
        "ipp_name": vp.ipp_name,
        "paired_client_id": vp.paired_client_id,
        "created_at": vp.created_at.to_rfc3339(),
        "updated_at": vp.updated_at.to_rfc3339(),
    })))
}

async fn delete_virtual_printer(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let Some(queue) = &state.queue else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    queue
        .delete_virtual_printer(&id)
        .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::state::AppState;

    fn test_state_with_queue() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = devbridge_server::JobQueue::new(storage).unwrap();
        // Leak the tempdir so it lives for the test
        std::mem::forget(dir);
        AppState::new("server".into()).with_queue(Arc::new(queue))
    }

    #[tokio::test]
    async fn test_list_virtual_printers_empty() {
        let app = crate::build_router(test_state_with_queue());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/virtual-printers")
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
    async fn test_create_and_list_virtual_printer() {
        let state = test_state_with_queue();
        let app = crate::build_router(state);

        // Create
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/virtual-printers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"display_name": "Store A", "ipp_name": "store-a"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(created["display_name"], "Store A");
        assert_eq!(created["ipp_name"], "store-a");
        assert!(created["id"].is_string());

        // List
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/virtual-printers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_virtual_printer_contract() {
        let state = test_state_with_queue();
        let app = crate::build_router(state);

        // Create a VP
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/virtual-printers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"display_name": "Test VP", "ipp_name": "test-vp"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // List and check contract
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/virtual-printers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = json.as_array().unwrap();
        let vp = arr[0].as_object().unwrap();

        assert!(vp.contains_key("id"));
        assert!(vp.contains_key("display_name"));
        assert!(vp.contains_key("ipp_name"));
        assert!(vp.contains_key("paired_client_id"));
        assert!(vp.contains_key("created_at"));
        assert!(vp.contains_key("updated_at"));
    }

    #[tokio::test]
    async fn test_create_rejects_empty_name() {
        let app = crate::build_router(test_state_with_queue());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/virtual-printers")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"display_name": "", "ipp_name": "valid"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
}
