use gloo_net::http::Request;
use gloo_net::websocket::futures::WebSocket;
use serde_json::Value;

pub async fn fetch_status() -> Result<Value, String> {
    Request::get("/api/status")
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn fetch_jobs() -> Result<Vec<Value>, String> {
    Request::get("/api/jobs")
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Vec<Value>>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn fetch_printers() -> Result<Vec<Value>, String> {
    Request::get("/api/printers")
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Vec<Value>>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn fetch_config() -> Result<Value, String> {
    Request::get("/api/config")
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn set_target_printer(name: &str) -> Result<Value, String> {
    Request::put("/api/printers/target")
        .json(&serde_json::json!({"name": name}))
        .map_err(|e| format!("Serialize failed: {e}"))?
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn fetch_virtual_printers() -> Result<Vec<Value>, String> {
    Request::get("/api/virtual-printers")
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Vec<Value>>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn create_virtual_printer(display_name: &str) -> Result<Value, String> {
    Request::post("/api/virtual-printers")
        .json(&serde_json::json!({
            "display_name": display_name,
        }))
        .map_err(|e| format!("Serialize failed: {e}"))?
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn update_virtual_printer(
    id: &str,
    display_name: Option<&str>,
    paired_client_id: Option<Option<&str>>,
) -> Result<Value, String> {
    let mut body = serde_json::Map::new();
    if let Some(name) = display_name {
        body.insert("display_name".into(), serde_json::json!(name));
    }
    if let Some(client_id) = paired_client_id {
        body.insert("paired_client_id".into(), serde_json::json!(client_id));
    }

    Request::put(&format!("/api/virtual-printers/{id}"))
        .json(&Value::Object(body))
        .map_err(|e| format!("Serialize failed: {e}"))?
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn delete_virtual_printer(id: &str) -> Result<(), String> {
    let resp = Request::delete(&format!("/api/virtual-printers/{id}"))
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    if resp.status() == 204 || resp.status() == 200 {
        Ok(())
    } else {
        Err(format!("Delete failed with status {}", resp.status()))
    }
}

pub async fn reprint_job(id: &str) -> Result<Value, String> {
    let resp = Request::post(&format!("/api/jobs/{id}/reprint"))
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    if resp.status() == 201 {
        resp.json::<Value>()
            .await
            .map_err(|e| format!("Parse failed: {e}"))
    } else {
        let body = resp
            .json::<Value>()
            .await
            .unwrap_or(serde_json::json!({"error": "unknown error"}));
        Err(body
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("reprint failed")
            .to_string())
    }
}

/// Connect to the WebSocket endpoint for real-time job events.
/// Returns the WebSocket connection for reading events.
pub fn connect_ws() -> Result<WebSocket, String> {
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let host = location.host().map_err(|_| "no host".to_string())?;
    let protocol = location.protocol().map_err(|_| "no protocol".to_string())?;
    let ws_protocol = if protocol == "https:" { "wss:" } else { "ws:" };
    let url = format!("{ws_protocol}//{host}/api/ws");
    WebSocket::open(&url).map_err(|e| format!("WebSocket error: {e}"))
}

pub async fn fetch_clients() -> Result<Vec<Value>, String> {
    Request::get("/api/clients")
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Vec<Value>>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn approve_client(id: &str) -> Result<serde_json::Value, String> {
    Request::post(&format!("/api/clients/{id}/approve"))
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn reject_client(id: &str) -> Result<serde_json::Value, String> {
    Request::post(&format!("/api/clients/{id}/reject"))
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn clear_jobs() -> Result<(), String> {
    Request::delete("/api/jobs")
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;
    Ok(())
}

pub async fn fetch_job_events(job_id: &str) -> Result<Vec<Value>, String> {
    Request::get(&format!("/api/jobs/{job_id}/events"))
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Vec<Value>>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

/// Fetch all jobs with their audit events in parallel.
/// Returns Vec<(job, events)> tuples.
pub async fn fetch_jobs_with_events() -> Result<Vec<(Value, Vec<Value>)>, String> {
    let jobs = fetch_jobs().await?;
    let mut results = Vec::new();
    for job in jobs {
        let job_id = job.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if job_id.is_empty() {
            results.push((job, vec![]));
            continue;
        }
        let events = fetch_job_events(job_id).await.unwrap_or_default();
        results.push((job, events));
    }
    Ok(results)
}
