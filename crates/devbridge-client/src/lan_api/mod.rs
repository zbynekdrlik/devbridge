//! The store's local HTTPS API for the Android pokladňa (#101) — eKasa
//! receipts while the store's internet (and so Odoo) is down.
//!
//! `[client.pokladna.lan_api]` starts it next to the device-job source, on
//! its own port (default 9443), every interface (the installer allows the
//! port on the Private / Domain firewall profiles and BLOCKS it on Public —
//! the service binary's own allow rule would otherwise open it there):
//!
//! - `POST /v1/jobs` — run a `fiscal.*` job of a till (wire format in
//!   [`request`]); answered once it is settled or after
//!   `Timing::lan_answer_wait` (202 → poll);
//! - `GET /v1/jobs/{kasa_id}/{client_uuid}` — the job's state and result;
//! - `GET /v1/health` — version, the Portos state, unreported jobs.
//!
//! Every call needs `Authorization: Bearer <lan_key>` of a till
//! ([`auth`]): 401 without a valid key (the Odoo bridge key is never one),
//! 403 when the key belongs to another till than `kasa_id`. `card.*` (and
//! anything not `fiscal.*`) is refused `unsupported` — card payments need
//! the bank online anyway.
//!
//! The jobs run through [`crate::device_jobs::lan::LanJobs`] — the SAME
//! exactly-once path as Odoo's (journal, per-Portos serial queue,
//! externalId) — and are reported to Odoo afterwards. TLS: the certificate
//! in `<data_dir>/lan-api/` ([`cert`]); the listener starts once it is
//! usable and reloads it after every renewal.

pub mod auth;
pub mod cert;
pub mod request;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_server::tls_rustls::RustlsConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use devbridge_core::config::PokladnaClientConfig;

use crate::device_jobs::lan::{
    CODE_BRIDGE_ERROR, CODE_CLIENT_UUID_CONFLICT, CODE_STATE_UNKNOWN, LanAnswer, LanJobs,
};
use auth::{KeyTable, bearer_token};
use cert::{CertPaths, CertSource};
use request::{error_json, job_json, job_status, parse_path, parse_request};

/// How often the listener looks for its certificate while it has none (a
/// new certificate from pz-server wakes it at once).
const CERT_WAIT: Duration = Duration::from_secs(10);

/// The rustls server config of a PEM chain (leaf first) and its key.
/// aws-lc-rs is named explicitly: the workspace's test build enables two
/// rustls crypto providers, so the process default would be ambiguous.
pub(crate) fn tls_config(chain_pem: &[u8], key_pem: &[u8]) -> Result<rustls::ServerConfig> {
    let chain = CertificateDer::pem_slice_iter(chain_pem)
        .collect::<Result<Vec<_>, _>>()
        .context("the certificate file is not PEM")?;
    ensure!(
        !chain.is_empty(),
        "the certificate file holds no certificate"
    );
    let key = PrivateKeyDer::from_pem_slice(key_pem).context("the key file holds no PEM key")?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("TLS protocol versions")?
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .context("the certificate and the key do not belong together")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

fn load_tls(paths: &CertPaths) -> Result<rustls::ServerConfig> {
    let chain =
        std::fs::read(&paths.cert).with_context(|| format!("read {}", paths.cert.display()))?;
    let key = std::fs::read(&paths.key).with_context(|| format!("read {}", paths.key.display()))?;
    tls_config(&chain, &key)
}

#[derive(Clone)]
struct ApiState {
    jobs: LanJobs,
    keys: Arc<KeyTable>,
}

/// The LAN API's routes (TLS is the listener's business).
pub fn router(jobs: LanJobs, keys: KeyTable) -> Router {
    Router::new()
        .route("/v1/jobs", post(post_job))
        .route("/v1/jobs/{kasa_id}/{client_uuid}", get(get_job))
        .route("/v1/health", get(health))
        .with_state(ApiState {
            jobs,
            keys: Arc::new(keys),
        })
}

fn reply(status: u16, body: Value) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(body)).into_response()
}

fn authorization(headers: &HeaderMap) -> Option<&str> {
    headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())
}

/// The HTTP status and body of a submit answer.
pub fn answer_reply(answer: LanAnswer) -> (u16, Value) {
    match answer {
        LanAnswer::Job { job, replay } => (job_status(job.state()), job_json(&job, replay)),
        LanAnswer::Refused { code, text } => (422, error_json(&code, &text)),
        LanAnswer::Conflict { job } => {
            let text = format!(
                "this client_uuid already names another operation on this till ({}) - nothing ran",
                job.entry.job.job_type
            );
            let mut body = error_json(CODE_CLIENT_UUID_CONFLICT, &text);
            body["job"] = job_json(&job, true);
            (409, body)
        }
        LanAnswer::BridgeError(text) => (503, error_json(CODE_BRIDGE_ERROR, &text)),
        LanAnswer::StateUnknown(text) => (503, error_json(CODE_STATE_UNKNOWN, &text)),
    }
}

const UNAUTHORIZED_TEXT: &str = "missing or unknown LAN key of a till";
const WRONG_KASA_TEXT: &str = "this LAN key belongs to another till";

async fn post_job(State(st): State<ApiState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(till) = st.keys.till_of(bearer_token(authorization(&headers))) else {
        warn!("LAN API: POST /v1/jobs without a valid till key - 401");
        return reply(401, error_json("unauthorized", UNAUTHORIZED_TEXT));
    };
    let request = match parse_request(&body) {
        Ok(request) => request,
        Err(text) => {
            warn!(till, reason = %text, "LAN API: bad job request - 400");
            return reply(400, error_json("bad_request", &text));
        }
    };
    if request.kasa_id != till {
        warn!(
            till,
            kasa_id = request.kasa_id,
            "LAN API: the key of another till - 403"
        );
        return reply(403, error_json("wrong_kasa", WRONG_KASA_TEXT));
    }
    let (status, body) = answer_reply(st.jobs.submit(request).await);
    reply(status, body)
}

async fn get_job(
    State(st): State<ApiState>,
    UrlPath((kasa, client_uuid)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(till) = st.keys.till_of(bearer_token(authorization(&headers))) else {
        warn!("LAN API: GET /v1/jobs without a valid till key - 401");
        return reply(401, error_json("unauthorized", UNAUTHORIZED_TEXT));
    };
    let Some(kasa_id) = parse_path(&kasa, &client_uuid) else {
        return reply(
            400,
            error_json(
                "bad_request",
                "kasa_id must be a positive integer and client_uuid [A-Za-z0-9_-]{1,64}",
            ),
        );
    };
    if kasa_id != till {
        warn!(till, kasa_id, "LAN API: the key of another till - 403");
        return reply(403, error_json("wrong_kasa", WRONG_KASA_TEXT));
    }
    match st.jobs.get(kasa_id, &client_uuid) {
        Ok(Some(job)) => reply(job_status(job.state()), job_json(&job, false)),
        Ok(None) => reply(
            404,
            error_json("not_found", "no job with this client_uuid on this till"),
        ),
        Err(e) => {
            // the job may exist and run - the till polls again
            error!(kasa_id, client_uuid = %client_uuid, error = %format!("{e:#}"), "LAN API: journal read failed");
            reply(503, error_json(CODE_STATE_UNKNOWN, &format!("{e:#}")))
        }
    }
}

async fn health(State(st): State<ApiState>, headers: HeaderMap) -> Response {
    if st
        .keys
        .till_of(bearer_token(authorization(&headers)))
        .is_none()
    {
        return reply(401, error_json("unauthorized", UNAUTHORIZED_TEXT));
    }
    match st.jobs.health() {
        Ok(h) => reply(
            200,
            json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "portos": h.portos,
                "unreported": h.unreported,
            }),
        ),
        Err(e) => reply(503, error_json(CODE_BRIDGE_ERROR, &format!("{e:#}"))),
    }
}

/// Everything the LAN API needs besides `[client.pokladna]`.
pub struct LanApiDeps {
    /// The client data dir — the certificate lives in `<data_dir>/lan-api/`.
    pub data_dir: PathBuf,
    /// pz-server's gRPC address (`[client] server_address`), the certificate
    /// source when `acme = true`.
    pub server_address: String,
    /// This client's id (`[client] client_id`).
    pub client_id: String,
    /// A listener the caller bound (tests: `127.0.0.1:0`); `None` = bind
    /// `0.0.0.0:<port>` once the certificate is usable.
    pub listener: Option<std::net::TcpListener>,
}

/// Wait until the pair on disk is usable (polled every [`CERT_WAIT`], or
/// woken by a new certificate).
async fn wait_for_certificate(paths: &CertPaths, reload: &Notify) -> rustls::ServerConfig {
    loop {
        match load_tls(paths) {
            Ok(config) => return config,
            Err(e) => {
                debug!(error = %format!("{e:#}"), "LAN API certificate not usable yet");
                tokio::select! {
                    _ = tokio::time::sleep(CERT_WAIT) => {}
                    _ = reload.notified() => {}
                }
            }
        }
    }
}

/// Load the pair again every time `reload` is notified (a new certificate);
/// an unusable one is logged and the served one stays.
async fn reload_on_notify(tls: &RustlsConfig, paths: &CertPaths, reload: &Notify) {
    loop {
        reload.notified().await;
        match load_tls(paths) {
            Ok(config) => {
                tls.reload_from_config(Arc::new(config));
                info!("LAN API certificate reloaded");
            }
            Err(e) => {
                error!(error = %format!("{e:#}"), "LAN API: the new certificate is not usable - the old one stays");
            }
        }
    }
}

/// Serve HTTPS once the certificate is usable; reload it on `reload`.
async fn serve(
    listener: Option<std::net::TcpListener>,
    port: u16,
    app: Router,
    paths: &CertPaths,
    reload: &Notify,
) -> Result<()> {
    info!(cert = %paths.cert.display(), "LAN API: listening once its certificate is usable");
    let tls = RustlsConfig::from_config(Arc::new(wait_for_certificate(paths, reload).await));
    let listener = match listener {
        Some(listener) => listener,
        None => std::net::TcpListener::bind(("0.0.0.0", port))
            .with_context(|| format!("bind the LAN API port {port}"))?,
    };
    listener
        .set_nonblocking(true)
        .context("LAN API listener: non-blocking")?;
    info!(addr = ?listener.local_addr().ok(), "LAN API listening (HTTPS)");
    let server = axum_server::from_tcp_rustls(listener, tls.clone())
        .context("LAN API listener")?
        .serve(app.into_make_service());
    tokio::select! {
        result = server => result.context("LAN API server stopped"),
        _ = reload_on_notify(&tls, paths, reload) => Ok(()),
    }
}

/// Run the LAN API until it fails (a bind error) or the source stops.
pub async fn run(cfg: &PokladnaClientConfig, jobs: LanJobs, deps: LanApiDeps) -> Result<()> {
    let lan = &cfg.lan_api;
    let keys = KeyTable::new(&cfg.kasy, &cfg.api_key);
    let paths = CertPaths::in_data_dir(&deps.data_dir);
    info!(
        port = lan.port,
        hostname = %lan.hostname,
        acme = lan.acme,
        tills = ?keys.tills(),
        cert = %paths.cert.display(),
        "LAN API starting (fiscal.* jobs of the tills, exactly once, reported to Odoo)"
    );
    // a replacement interrupted by a crash is installed before the listener
    // looks (with `acme = false` nothing else would)
    match cert::finish_pending(&paths) {
        Ok(installed) => info!(
            installed,
            "LAN API certificate: interrupted replacement checked"
        ),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "LAN API certificate: an interrupted replacement could not be finished");
        }
    }
    let reload = Notify::new();
    let source = CertSource {
        server_address: deps.server_address,
        client_id: deps.client_id,
        hostname: lan.hostname.clone(),
    };
    let certificates = async {
        if lan.acme {
            cert::maintain(source, paths.clone(), &reload).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        result = serve(deps.listener, lan.port, router(jobs, keys), &paths, &reload) => result,
        _ = certificates => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_answer_reply_of_answers_without_a_job() {
        assert_eq!(
            answer_reply(LanAnswer::BridgeError("disk full".into())),
            (
                503,
                json!({"error": {"code": "bridge_error", "text": "disk full"}})
            ),
            "nothing journaled: send the same client_uuid again"
        );
        assert_eq!(
            answer_reply(LanAnswer::StateUnknown("journal locked".into())),
            (
                503,
                json!({"error": {"code": "state_unknown", "text": "journal locked"}})
            ),
            "journaled, state unknown: poll"
        );
        assert_eq!(
            answer_reply(LanAnswer::Refused {
                code: "unsupported".into(),
                text: "card".into()
            }),
            (
                422,
                json!({"error": {"code": "unsupported", "text": "card"}})
            )
        );
    }
}
