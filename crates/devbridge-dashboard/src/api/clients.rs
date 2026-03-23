use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/clients", get(list_clients))
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
                    })
                })
                .collect();
            Json(json!(json_clients))
        }
        Err(_) => Json(json!([])),
    }
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
        std::mem::forget(dir);
        AppState::new("server".into()).with_queue(Arc::new(queue))
    }

    #[tokio::test]
    async fn test_list_clients_empty() {
        let app = crate::build_router(test_state_with_queue());
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
        let state = test_state_with_queue();
        let queue = state.queue.as_ref().unwrap();

        let reg = devbridge_core::client_registration::ClientRegistration {
            machine_id: "test-mc".into(),
            hostname: "test-host".into(),
            printer_names: vec!["Printer1".into()],
            client_version: "0.1.0".into(),
            last_seen: chrono::Utc::now(),
            is_online: true,
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
}
