use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/jobs", get(get_jobs))
}

async fn get_jobs(State(state): State<AppState>) -> Json<Value> {
    let Some(queue) = &state.queue else {
        return Json(json!([]));
    };

    match queue.get_all_jobs() {
        Ok(jobs) => {
            let jobs_json: Vec<Value> = jobs
                .iter()
                .map(|j| {
                    json!({
                        "id": j.job_id,
                        "name": j.document_name,
                        "printer": j.target_printer,
                        "target_client_id": j.target_client_id,
                        "status": format!("{:?}", j.state).to_lowercase(),
                        "copies": j.copies,
                        "paper_size": j.paper_size,
                        "duplex": j.duplex,
                        "color": j.color,
                        "payload_size": j.payload_size,
                        "payload_sha256": j.payload_sha256,
                        "created_at": j.created_at.to_rfc3339(),
                        "updated_at": j.updated_at.to_rfc3339(),
                    })
                })
                .collect();
            Json(json!(jobs_json))
        }
        Err(_) => Json(json!([])),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::state::AppState;

    #[tokio::test]
    async fn test_jobs_endpoint_returns_array() {
        let app = crate::build_router(AppState::new("server".into()));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/jobs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.is_array(), "Expected JSON array, got: {}", json);
    }

    #[tokio::test]
    async fn test_jobs_response_matches_ui_contract() {
        use std::sync::Arc;

        // Create a queue with a test job so the response is non-empty
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = devbridge_server::JobQueue::new(storage).unwrap();

        let job = devbridge_core::job::JobMetadata {
            job_id: "test-contract-1".into(),
            document_name: "receipt.pdf".into(),
            target_printer: "EPSON L3270".into(),
            target_client_id: None,
            copies: 1,
            paper_size: "A4".into(),
            duplex: false,
            color: true,
            payload_size: 512,
            payload_sha256: "deadbeef".into(),
            state: devbridge_core::job::JobState::Queued,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        queue.push(job, "/tmp/test.pdf".into()).unwrap();

        let state = AppState::new("server".into()).with_queue(Arc::new(queue));
        let app = crate::build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/jobs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = json.as_array().expect("response must be an array");
        assert_eq!(arr.len(), 1);

        let job_obj = arr[0].as_object().expect("each job must be a JSON object");

        // Assert the exact keys the UI reads
        assert!(job_obj.contains_key("id"), "missing 'id' field");
        assert!(job_obj.contains_key("name"), "missing 'name' field");
        assert!(job_obj.contains_key("printer"), "missing 'printer' field");
        assert!(job_obj.contains_key("status"), "missing 'status' field");
        assert!(
            job_obj.contains_key("created_at"),
            "missing 'created_at' field"
        );

        // Assert types
        assert!(job_obj["id"].is_string(), "'id' must be a string");
        assert!(job_obj["name"].is_string(), "'name' must be a string");
        assert!(job_obj["printer"].is_string(), "'printer' must be a string");
        assert!(job_obj["status"].is_string(), "'status' must be a string");
        assert!(
            job_obj["created_at"].is_string(),
            "'created_at' must be a string"
        );

        // Assert values match what we inserted
        assert_eq!(job_obj["id"].as_str().unwrap(), "test-contract-1");
        assert_eq!(job_obj["name"].as_str().unwrap(), "receipt.pdf");
        assert_eq!(job_obj["printer"].as_str().unwrap(), "EPSON L3270");
        assert_eq!(job_obj["status"].as_str().unwrap(), "queued");

        // Assert old keys do NOT exist (prevent regression)
        assert!(
            !job_obj.contains_key("job_id"),
            "old key 'job_id' must not exist"
        );
        assert!(
            !job_obj.contains_key("document_name"),
            "old key 'document_name' must not exist"
        );
        assert!(
            !job_obj.contains_key("target_printer"),
            "old key 'target_printer' must not exist"
        );
        assert!(
            !job_obj.contains_key("state"),
            "old key 'state' must not exist"
        );
    }
}
