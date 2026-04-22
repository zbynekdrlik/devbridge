use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/jobs", get(get_jobs).delete(clear_jobs))
        .route("/jobs/events", get(get_all_events_batch))
        .route("/jobs/{id}/reprint", post(reprint_job))
        .route("/jobs/{id}/events", get(get_job_events))
}

async fn clear_jobs(State(state): State<AppState>) -> StatusCode {
    if let Some(queue) = &state.queue {
        let _ = queue.clear_jobs();
    }
    StatusCode::NO_CONTENT
}

async fn get_job_events(State(state): State<AppState>, Path(job_id): Path<String>) -> Json<Value> {
    let Some(queue) = &state.queue else {
        return Json(json!([]));
    };

    let events = queue.get_job_events(&job_id).unwrap_or_default();

    let json_events: Vec<Value> = events
        .iter()
        .map(|e| {
            json!({
                "job_id": e.job_id,
                "stage": e.stage,
                "success": e.success,
                "detail": e.detail,
                "verification_method": e.verification_method,
                "verification_evidence": e.verification_evidence,
                "timestamp": e.timestamp.to_rfc3339(),
            })
        })
        .collect();

    Json(json!(json_events))
}

async fn get_all_events_batch(State(state): State<AppState>) -> Json<Value> {
    let Some(queue) = &state.queue else {
        return Json(json!({}));
    };

    match queue.get_all_job_events() {
        Ok(events_map) => {
            let json_map: serde_json::Map<String, Value> = events_map
                .into_iter()
                .map(|(job_id, events)| {
                    let json_events: Vec<Value> = events
                        .iter()
                        .map(|e| {
                            json!({
                                "job_id": e.job_id,
                                "stage": e.stage,
                                "success": e.success,
                                "detail": e.detail,
                                "verification_method": e.verification_method,
                                "verification_evidence": e.verification_evidence,
                                "timestamp": e.timestamp.to_rfc3339(),
                            })
                        })
                        .collect();
                    (job_id, Value::Array(json_events))
                })
                .collect();
            Json(Value::Object(json_map))
        }
        Err(_) => Json(json!({})),
    }
}

async fn get_jobs(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let filter_user: Option<String> = params.get("requesting_user").cloned();

    let Some(queue) = &state.queue else {
        return Json(json!([]));
    };

    match queue.get_all_jobs() {
        Ok(jobs) => {
            let jobs_json: Vec<Value> = jobs
                .iter()
                .filter(|j| {
                    filter_user
                        .as_ref()
                        .is_none_or(|u| j.requesting_user.as_ref() == Some(u))
                })
                .take(limit)
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
                        "requesting_user": j.requesting_user,
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

async fn reprint_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let queue = state.queue.as_ref().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "no queue"})),
    ))?;

    // Look up the original job
    let original = queue
        .get_job(&id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "job not found"})),
        ))?;

    // Check 48-hour expiry
    let age = chrono::Utc::now() - original.created_at;
    if age > chrono::Duration::hours(48) {
        let hours = age.num_hours();
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!(
                "This job is too old to reprint ({} hours, limit is 48). The print file has been cleaned up.",
                hours
            )})),
        ));
    }

    // Verify spool file exists
    let spool_path = queue
        .get_spool_path(&id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?
        .ok_or((
            StatusCode::GONE,
            Json(json!({"error": "Print file has been cleaned up and is no longer available for reprint. Re-send the print from the original application."})),
        ))?;

    if !std::path::Path::new(&spool_path).exists() {
        return Err((
            StatusCode::GONE,
            Json(
                json!({"error": "Print file was deleted from disk. Re-send the print from the original application."}),
            ),
        ));
    }

    // Create a new job from the original
    let new_job_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let new_job = devbridge_core::job::JobMetadata {
        job_id: new_job_id.clone(),
        document_name: original.document_name.clone(),
        target_printer: original.target_printer.clone(),
        target_client_id: original.target_client_id.clone(),
        copies: original.copies,
        paper_size: original.paper_size.clone(),
        duplex: original.duplex,
        color: original.color,
        payload_size: original.payload_size,
        payload_sha256: original.payload_sha256.clone(),
        state: devbridge_core::job::JobState::Queued,
        retry_count: 0,
        error_detail: String::new(),
        requesting_user: original.requesting_user.clone(),
        created_at: now,
        updated_at: now,
    };

    // Copy spool file so reprint owns its own copy
    let reprint_spool = format!("{}.reprint-{}", spool_path, new_job_id);
    std::fs::copy(&spool_path, &reprint_spool).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to copy spool file: {}", e)})),
        )
    })?;

    queue.push(new_job, reprint_spool).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
    })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": new_job_id,
            "name": original.document_name,
            "status": "queued",
            "reprinted_from": id,
        })),
    ))
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

    fn test_queue_with_job(
        dir: &tempfile::TempDir,
        job_id: &str,
        created_at: chrono::DateTime<chrono::Utc>,
    ) -> (std::sync::Arc<devbridge_server::JobQueue>, String) {
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = devbridge_server::JobQueue::new(storage).unwrap();

        let spool_path = dir.path().join(format!("{job_id}.pdf"));
        std::fs::write(&spool_path, b"fake pdf content").unwrap();

        let job = devbridge_core::job::JobMetadata {
            job_id: job_id.into(),
            document_name: "receipt.pdf".into(),
            target_printer: "EPSON L3270".into(),
            target_client_id: None,
            copies: 1,
            paper_size: "A4".into(),
            duplex: false,
            color: true,
            payload_size: 512,
            payload_sha256: "deadbeef".into(),
            state: devbridge_core::job::JobState::Completed,
            retry_count: 0,
            error_detail: String::new(),
            requesting_user: None,
            created_at,
            updated_at: created_at,
        };
        queue
            .push(job, spool_path.to_str().unwrap().to_string())
            .unwrap();

        (
            std::sync::Arc::new(queue),
            spool_path.to_str().unwrap().to_string(),
        )
    }

    #[tokio::test]
    async fn test_reprint_within_48h_returns_201() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, _) = test_queue_with_job(&dir, "reprint-1", chrono::Utc::now());
        let state = AppState::new("server".into()).with_queue(queue);
        let app = crate::build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/jobs/reprint-1/reprint")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 201);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["id"].is_string());
        assert_eq!(json["reprinted_from"], "reprint-1");
    }

    #[tokio::test]
    async fn test_reprint_expired_returns_422() {
        let dir = tempfile::tempdir().unwrap();
        let old_time = chrono::Utc::now() - chrono::Duration::hours(49);
        let (queue, _) = test_queue_with_job(&dir, "reprint-old", old_time);
        let state = AppState::new("server".into()).with_queue(queue);
        let app = crate::build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/jobs/reprint-old/reprint")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 422);
    }

    #[tokio::test]
    async fn test_reprint_missing_spool_returns_410() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = devbridge_server::JobQueue::new(storage).unwrap();

        let job = devbridge_core::job::JobMetadata {
            job_id: "reprint-nospool".into(),
            document_name: "receipt.pdf".into(),
            target_printer: "EPSON L3270".into(),
            target_client_id: None,
            copies: 1,
            paper_size: "A4".into(),
            duplex: false,
            color: true,
            payload_size: 512,
            payload_sha256: "deadbeef".into(),
            state: devbridge_core::job::JobState::Completed,
            retry_count: 0,
            error_detail: String::new(),
            requesting_user: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        queue
            .push(job, "/nonexistent/path/fake.pdf".to_string())
            .unwrap();

        let state = AppState::new("server".into()).with_queue(std::sync::Arc::new(queue));
        let app = crate::build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/jobs/reprint-nospool/reprint")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 410);
    }

    #[tokio::test]
    async fn test_reprint_not_found_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = std::sync::Arc::new(devbridge_server::JobQueue::new(storage).unwrap());
        let state = AppState::new("server".into()).with_queue(queue);
        let app = crate::build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/jobs/nonexistent/reprint")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 404);
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
            retry_count: 0,
            error_detail: String::new(),
            requesting_user: None,
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

    #[tokio::test]
    async fn test_requesting_user_filter_returns_only_matching_jobs() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = devbridge_server::JobQueue::new(storage).unwrap();

        let now = chrono::Utc::now();

        // Job A — alice
        queue
            .push(
                devbridge_core::job::JobMetadata {
                    job_id: "job-alice-1".into(),
                    document_name: "alice-doc.pdf".into(),
                    target_printer: "Printer".into(),
                    target_client_id: None,
                    copies: 1,
                    paper_size: "A4".into(),
                    duplex: false,
                    color: false,
                    payload_size: 100,
                    payload_sha256: "aabbcc".into(),
                    state: devbridge_core::job::JobState::Queued,
                    retry_count: 0,
                    error_detail: String::new(),
                    requesting_user: Some("alice".into()),
                    created_at: now,
                    updated_at: now,
                },
                "/tmp/alice.pdf".into(),
            )
            .unwrap();

        // Job B — bob
        queue
            .push(
                devbridge_core::job::JobMetadata {
                    job_id: "job-bob-1".into(),
                    document_name: "bob-doc.pdf".into(),
                    target_printer: "Printer".into(),
                    target_client_id: None,
                    copies: 1,
                    paper_size: "A4".into(),
                    duplex: false,
                    color: false,
                    payload_size: 200,
                    payload_sha256: "ddeeff".into(),
                    state: devbridge_core::job::JobState::Queued,
                    retry_count: 0,
                    error_detail: String::new(),
                    requesting_user: Some("bob".into()),
                    created_at: now,
                    updated_at: now,
                },
                "/tmp/bob.pdf".into(),
            )
            .unwrap();

        let state = AppState::new("server".into()).with_queue(Arc::new(queue));

        // Test 1: filter by alice → only alice's job
        {
            let app = crate::build_router(state.clone());
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/jobs?requesting_user=alice")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let arr = json.as_array().expect("expected array");
            assert_eq!(arr.len(), 1, "alice filter should return 1 job");
            assert_eq!(arr[0]["id"], "job-alice-1");
            assert_eq!(arr[0]["requesting_user"], "alice");
        }

        // Test 2: no filter → both jobs returned
        {
            let app = crate::build_router(state.clone());
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/jobs")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let arr = json.as_array().expect("expected array");
            assert_eq!(arr.len(), 2, "no filter should return both jobs");
        }

        // Test 3: filter by unknown user → empty array
        {
            let app = crate::build_router(state.clone());
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/jobs?requesting_user=nobody")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let arr = json.as_array().expect("expected array");
            assert!(
                arr.is_empty(),
                "unknown user filter should return empty array"
            );
        }
    }

    #[tokio::test]
    async fn test_job_events_include_verification_fields() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = devbridge_server::JobQueue::new(storage).unwrap();

        let event = devbridge_core::job_event::PrintJobEvent {
            job_id: "job-api-1".into(),
            requesting_user: None,
            stage: devbridge_core::job_event::PrintStage::Verified,
            success: true,
            detail: "EventID 307: Document 42".into(),
            verification_method: "eventid_307".into(),
            verification_evidence: "EventID 307: Document 42, eholla printer, USB002".into(),
            timestamp: chrono::Utc::now(),
        };
        queue.insert_job_event(&event).unwrap();

        let state = AppState::new("server".into()).with_queue(Arc::new(queue));
        let app = crate::build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/jobs/job-api-1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let events = json.as_array().unwrap();
        assert_eq!(events.len(), 1);

        let ev = &events[0];
        assert_eq!(ev["stage"], "verified");
        assert_eq!(ev["verification_method"], "eventid_307");
        assert!(
            ev["verification_evidence"]
                .as_str()
                .unwrap()
                .contains("EventID 307")
        );
    }
}
