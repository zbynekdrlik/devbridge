//! The HTTP calls to a Portos eKasa API (#100) — each one answers a
//! [`Reply`] that keeps the one distinction exactly-once needs: did the
//! request possibly reach Portos?
//!
//! - [`Reply::Unreachable`]: the TCP connection could not be opened — the
//!   request never left this PC (Portos not running, wrong address). Safe to
//!   call "nothing ran".
//! - [`Reply::Lost`]: the request was (or may have been) sent, but no
//!   complete answer came back (timeout, reset). Portos may have acted on it.
//! - [`Reply::Answer`]: an HTTP status and its JSON body (`Null` when the
//!   body is empty or not JSON).
//!
//! Connections are never reused (`pool_max_idle_per_host(0)`): a request on
//! a pooled connection the server already closed would fail AFTER the
//! connect step and read as `Lost`, not `Unreachable`. No proxy is ever used
//! (Portos runs on the store PC or its LAN).

use std::time::Duration;

use serde_json::Value;

/// TCP connect timeout of every call (Portos is local).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// What one call to Portos came back with.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// The connection could not be opened — nothing was sent.
    Unreachable(String),
    /// Sent (or possibly sent), no complete answer.
    Lost(String),
    /// An HTTP answer.
    Answer { status: u16, body: Value },
}

/// Parse a response body: JSON, or `Null` (empty / not JSON).
pub fn parse_body(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// One HTTP client for every Portos of this bridge.
#[derive(Debug, Clone)]
pub struct PortosHttp {
    http: reqwest::Client,
}

impl PortosHttp {
    pub fn new() -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_max_idle_per_host(0)
            .no_proxy()
            .build()?;
        Ok(Self { http })
    }

    /// `GET {base}{path}?{query}`, the whole call bounded by `timeout`.
    pub async fn get(
        &self,
        base: &str,
        path: &str,
        query: &[(String, String)],
        timeout: Duration,
    ) -> Reply {
        let request = self.http.get(format!("{base}{path}")).query(query);
        send(request, timeout).await
    }

    /// `POST {base}{path}?{query}` with an optional JSON body.
    pub async fn post(
        &self,
        base: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        timeout: Duration,
    ) -> Reply {
        let mut request = self.http.post(format!("{base}{path}")).query(query);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        send(request, timeout).await
    }
}

async fn send(request: reqwest::RequestBuilder, timeout: Duration) -> Reply {
    let response = match request
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(timeout)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) if e.is_connect() => return Reply::Unreachable(error_text(&e)),
        Err(e) => return Reply::Lost(error_text(&e)),
    };
    let status = response.status().as_u16();
    match response.text().await {
        Ok(text) => Reply::Answer {
            status,
            body: parse_body(&text),
        },
        Err(e) => Reply::Lost(format!(
            "HTTP {status}, reading the answer failed: {}",
            error_text(&e)
        )),
    }
}

/// The error and its causes (`reqwest` hides the useful part in `source`).
fn error_text(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_parse_body() {
        assert_eq!(parse_body(r#"{"a": 1}"#), json!({"a": 1}));
        assert_eq!(parse_body("[]"), json!([]));
        assert_eq!(parse_body(""), Value::Null);
        assert_eq!(parse_body("<html>"), Value::Null);
    }

    /// A one-shot server that answers `response` (raw HTTP) or, when `None`,
    /// reads the request and closes the connection without answering.
    async fn one_shot(response: Option<&'static str>) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut request = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let len = text[..head_end]
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            if let Some(response) = response {
                sock.write_all(response.as_bytes()).await.unwrap();
            }
            sock.shutdown().await.ok();
            String::from_utf8_lossy(&request).to_string()
        });
        (base, handle)
    }

    #[tokio::test]
    async fn test_answer_with_query_body_and_headers() {
        let (base, server) = one_shot(Some(
            "HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: 17\r\nconnection: close\r\n\r\n{\"printed\":false}",
        ))
        .await;
        let http = PortosHttp::new().unwrap();
        let query = vec![("CashRegisterCode".to_string(), "888 1".to_string())];
        let reply = http
            .post(
                &base,
                "/api/v1/x",
                &query,
                Some(&json!({"k": "v"})),
                Duration::from_secs(5),
            )
            .await;
        assert_eq!(
            reply,
            Reply::Answer {
                status: 202,
                body: json!({"printed": false})
            }
        );
        let request = server.await.unwrap();
        assert!(
            request.starts_with("POST /api/v1/x?CashRegisterCode=888+1 HTTP/1.1"),
            "{request}"
        );
        assert!(
            request.contains("content-type: application/json"),
            "{request}"
        );
        assert!(request.contains("accept: application/json"), "{request}");
        assert!(request.ends_with(r#"{"k":"v"}"#), "{request}");
    }

    #[tokio::test]
    async fn test_get_without_body_and_non_json_answer() {
        let (base, server) = one_shot(Some(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 4\r\nconnection: close\r\n\r\noops",
        ))
        .await;
        let reply = PortosHttp::new()
            .unwrap()
            .get(&base, "/api/v1/product/info", &[], Duration::from_secs(5))
            .await;
        assert_eq!(
            reply,
            Reply::Answer {
                status: 500,
                body: Value::Null
            }
        );
        let request = server.await.unwrap();
        assert!(
            request.starts_with("GET /api/v1/product/info HTTP/1.1"),
            "{request}"
        );
        assert!(!request.contains("content-type"), "{request}");
    }

    #[tokio::test]
    async fn test_closed_port_is_unreachable_and_a_dropped_answer_is_lost() {
        // a port nobody listens on: nothing was sent
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let http = PortosHttp::new().unwrap();
        match http
            .post(&base, "/x", &[], Some(&json!({})), Duration::from_secs(5))
            .await
        {
            // the cause chain is kept: "error sending request …: … (os error N)"
            Reply::Unreachable(e) => {
                assert!(e.starts_with("error sending request"), "{e}");
                assert!(e.contains("os error"), "{e}");
            }
            other => panic!("{other:?}"),
        }
        // the request arrives, the connection closes without an answer
        let (base, server) = one_shot(None).await;
        match http
            .post(&base, "/x", &[], Some(&json!({})), Duration::from_secs(5))
            .await
        {
            Reply::Lost(e) => assert!(e.starts_with("error sending request"), "{e}"),
            other => panic!("{other:?}"),
        }
        assert!(server.await.unwrap().starts_with("POST /x"));
    }

    #[tokio::test]
    async fn test_a_silent_server_is_lost_after_the_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(sock);
        });
        let started = std::time::Instant::now();
        let reply = PortosHttp::new()
            .unwrap()
            .get(&base, "/x", &[], Duration::from_millis(200))
            .await;
        assert!(matches!(reply, Reply::Lost(_)), "{reply:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
        server.abort();
    }
}
