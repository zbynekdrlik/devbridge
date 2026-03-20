use gloo_net::http::Request;
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
