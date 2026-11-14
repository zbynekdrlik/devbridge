//! E2E coverage for the `file.write` device job (issue #130) in its REAL
//! topology: the isolated E2E SERVER instance on pz-server (server mode, data
//! dir `C:\ProgramData\DevBridge-E2E`) runs the device-job source with the
//! file executor only — exactly what the Codex server will run.
//!
//! `deploy/e2e-setup-server.ps1` writes that instance's `[client.pokladna]`
//! (url = this fake Odoo on loopback `127.0.0.1:9233`, the E2E file-bridge
//! key, `wait_s` 10, `fiscal = false`) and `[client.pokladna.file_write]`
//! (`target_dir` = a fresh `file-write-in` folder in the E2E data dir) with
//! the REAL installer function `Get-DevBridgePokladnaRequest -Mode server`.
//! This binary runs on pz-server too, so it reads the written file back from
//! disk.
//!
//! Asserts:
//! 1. an EDI-shaped file (the contract's cp1250 golden content plus a run
//!    line) lands under its name byte-for-byte, acked `done` with its size +
//!    sha256;
//! 2. a second job with the SAME name is `failed file_exists` and the first
//!    file is unchanged (never overwritten);
//! 3. a name with a path (`a\..\`) and a content whose sha256 does not match
//!    are `failed bad_payload` "nothing ran" — nothing lands anywhere;
//! 4. a `fiscal.*` job is `failed unsupported` (no Portos on a server);
//! 5. the folder holds exactly that file and the staging folder in the data
//!    dir (`file-write-staging`, the installer's ACL) is empty;
//! 6. the heartbeat carries the server version and
//!    `portos=none; terminal=none; file=ok`; every `next` asked `wait_s` 10;
//!    every request had the right Bearer key; `pending` never overlapped a
//!    `next`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use axum::Router;
use axum::routing::post;
use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::device_jobs::{Fake, Shared, check_refused, final_acks, run_base_id, wait_until};

/// Must match `deploy/e2e-setup-server.ps1`.
pub const FAKE_FILE_PORT: u16 = 9233;
pub const E2E_FILE_KEY: &str = "e2e-fake-file-bridge-key-devbridge-130";
pub const E2E_FILE_WAIT_S: u64 = 10;
pub const E2E_FILE_DIR: &str = r"C:\ProgramData\DevBridge-E2E\file-write-in";
/// The executor's staging folder in the E2E server's data dir
/// (`devbridge-client`'s `staging::STAGING_DIR`), made by the installer's
/// `Initialize-DevBridgeFileWriteStaging`.
pub const E2E_STAGING_DIR: &str = r"C:\ProgramData\DevBridge-E2E\file-write-staging";
/// Heartbeat of a server-mode bridge: the file executor only.
pub const FILE_DEVICE_STATUS: &str = "portos=none; terminal=none; file=ok";

/// The server's source backs off up to 30 s while this fake is not up yet.
const FIRST_CONTACT_WAIT: Duration = Duration::from_secs(240);
const HEARTBEAT_WAIT: Duration = Duration::from_secs(90);

/// The contract's golden content: "HDR objednávka Žilina\r\n" in cp1250.
pub fn golden_content() -> Vec<u8> {
    let mut bytes = b"HDR objedn".to_vec();
    bytes.push(0xE1); // á
    bytes.extend_from_slice(b"vka ");
    bytes.push(0x8E); // Ž
    bytes.extend_from_slice(b"ilina\r\n");
    bytes
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A `file.write` payload as Odoo composes it.
pub fn file_payload(filename: &str, bytes: &[u8]) -> Value {
    json!({
        "filename": filename,
        "encoding": "cp1250",
        "content_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        "size": bytes.len(),
        "sha256": sha256_hex(bytes),
    })
}

/// The job was acked `done` once with exactly the file's name / size / sha256.
pub fn check_written(acks: &[Value], id: i64, filename: &str, bytes: &[u8]) -> Result<()> {
    let finals = final_acks(acks, id);
    let want = json!({"job_id": id, "result": {
        "filename": filename, "size": bytes.len(), "sha256": sha256_hex(bytes)}});
    ensure!(!finals.is_empty(), "job {id}: no final ack");
    ensure!(
        finals.iter().all(|a| *a == want),
        "job {id}: every final ack must be {want}, got {finals:?}"
    );
    Ok(())
}

/// The reader's folder holds exactly `filename` (no temp name, no staging
/// folder) and the staging folder is EMPTY.
pub fn check_folder(dir: &Path, staging: &Path, filename: &str) -> Result<()> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<_>>()?;
    names.sort();
    ensure!(
        names == [filename],
        "{}: expected only {filename}, found {names:?}",
        dir.display()
    );
    let staged = std::fs::read_dir(staging)
        .with_context(|| format!("read {}", staging.display()))?
        .count();
    ensure!(
        staged == 0,
        "{staged} file(s) left in {}",
        staging.display()
    );
    Ok(())
}

async fn server_version(client: &reqwest::Client, server_base: &str) -> Result<String> {
    let url = format!("{server_base}/api/status");
    let status: Value = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .json()
        .await
        .with_context(|| format!("GET {url}: not JSON"))?;
    status["version"]
        .as_str()
        .map(str::to_string)
        .context("server /api/status without version")
}

/// Step body — see the module docs.
pub async fn test_file_write(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let base = run_base_id();
    let name = format!("ORDER_E2E_{base}.txt");
    let evil = format!("evil_{base}.txt");
    let mut content = golden_content();
    content.extend_from_slice(format!("RUN {base}\r\n").as_bytes());
    let state: Shared = Arc::new(Mutex::new(Fake::with_key(E2E_FILE_KEY)));
    {
        let mut s = state.lock().unwrap();
        s.queue(base + 1, "file.write", file_payload(&name, &content));
        s.queue(base + 2, "file.write", file_payload(&name, b"SECOND\r\n"));
        s.queue(
            base + 3,
            "file.write",
            file_payload(&format!("a\\..\\{evil}"), b"x"),
        );
        let mut bad_sha = file_payload(&format!("BADSHA_{base}.txt"), b"abc");
        bad_sha["sha256"] = json!(sha256_hex(b"other"));
        s.queue(base + 4, "file.write", bad_sha);
        s.queue(
            base + 5,
            "fiscal.cash_register",
            json!({"amount": 1.5, "currency": "EUR"}),
        );
    }
    let app = Router::new()
        .route("/pokladna/bridge/next", post(crate::device_jobs::next))
        .route(
            "/pokladna/bridge/pending",
            post(crate::device_jobs::pending),
        )
        .route("/pokladna/bridge/ack", post(crate::device_jobs::ack))
        .route(
            "/pokladna/bridge/heartbeat",
            post(crate::device_jobs::heartbeat),
        )
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", FAKE_FILE_PORT))
        .await
        .with_context(|| format!("bind fake Odoo (file bridge) on 127.0.0.1:{FAKE_FILE_PORT}"))?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let outcome = verify(client, server_base, &state, base, &name, &evil, &content).await;
    server.abort();
    outcome?;
    println!("PASS");
    Ok(())
}

async fn verify(
    client: &reqwest::Client,
    server_base: &str,
    state: &Shared,
    base: i64,
    name: &str,
    evil: &str,
    content: &[u8],
) -> Result<()> {
    wait_until(
        state,
        FIRST_CONTACT_WAIT,
        "five file-bridge jobs answered",
        |s| (1..=5).all(|n| !final_acks(&s.acks, base + n).is_empty()),
    )
    .await?;
    let dir = Path::new(E2E_FILE_DIR);
    {
        let s = state.lock().unwrap();
        check_written(&s.acks, base + 1, name, content)?;
        check_refused(&s.acks, base + 2, "file_exists")?;
        check_refused(&s.acks, base + 3, "bad_payload")?;
        check_refused(&s.acks, base + 4, "bad_payload")?;
        check_refused(&s.acks, base + 5, "unsupported")?;
    }
    let on_disk = std::fs::read(dir.join(name)).with_context(|| format!("read {name}"))?;
    ensure!(
        on_disk == content,
        "{name}: {} bytes on disk (sha256 {}), expected {} bytes (sha256 {}) - the second job must never overwrite it",
        on_disk.len(),
        sha256_hex(&on_disk),
        content.len(),
        sha256_hex(content)
    );
    check_folder(dir, Path::new(E2E_STAGING_DIR), name)?;
    let parent = dir.parent().context("the E2E folder has a parent")?;
    ensure!(
        !parent.join(evil).exists(),
        "a '..' name escaped the folder"
    );
    println!(
        "  {name} written ({} bytes, sha256 {}), same name refused file_exists, '..' + bad sha256 refused bad_payload, fiscal job unsupported, staging empty",
        content.len(),
        sha256_hex(content)
    );

    let version = server_version(client, server_base).await?;
    wait_until(
        state,
        HEARTBEAT_WAIT,
        "heartbeat with the file executor",
        |s| {
            s.heartbeats.iter().any(|hb| {
                hb["bridge_version"] == version.as_str()
                    && hb["device_status"] == FILE_DEVICE_STATUS
            })
        },
    )
    .await?;
    let s = state.lock().unwrap();
    ensure!(
        !s.next_waits.is_empty() && s.next_waits.iter().all(|w| *w == E2E_FILE_WAIT_S),
        "next must ask wait_s {E2E_FILE_WAIT_S} (installer value): {:?}",
        s.next_waits
    );
    ensure!(
        s.unauthorized == 0,
        "{} request(s) without the file-bridge key",
        s.unauthorized
    );
    ensure!(
        s.overlap == 0,
        "pending ran while a next was in flight ({}x)",
        s.overlap
    );
    println!(
        "  heartbeat {FILE_DEVICE_STATUS} v{version}, {} next calls all wait_s {E2E_FILE_WAIT_S}",
        s.next_waits.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_golden_content_matches_the_contract() {
        let bytes = golden_content();
        assert_eq!(bytes.len(), 23);
        assert_eq!(
            sha256_hex(&bytes),
            "66f5904aa2710637d0125a58ebbda61ca52783af75ee92816db4aedafc12cff6"
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(&bytes),
            "SERSIG9iamVkbuF2a2EgjmlsaW5hDQo="
        );
    }

    #[test]
    fn test_file_payload_shape() {
        let p = file_payload("A.txt", b"abc");
        assert_eq!(p["size"], 3);
        assert_eq!(p["content_b64"], "YWJj");
        assert_eq!(p["encoding"], "cp1250");
        assert_eq!(p["sha256"], sha256_hex(b"abc"));
    }

    #[test]
    fn test_check_written() {
        let ok = vec![
            json!({"job_id": 5, "result": {"filename": "A.txt", "size": 3, "sha256": sha256_hex(b"abc")}}),
        ];
        assert!(check_written(&ok, 5, "A.txt", b"abc").is_ok());
        assert!(check_written(&ok, 5, "A.txt", b"abd").is_err());
        assert!(check_written(&ok, 5, "B.txt", b"abc").is_err());
        assert!(check_written(&[], 5, "A.txt", b"abc").is_err());
        let failed = vec![json!({"job_id": 5, "error_code": "file_exists", "error": "x"})];
        assert!(check_written(&failed, 5, "A.txt", b"abc").is_err());
    }

    #[test]
    fn test_check_folder() {
        let root = std::env::temp_dir().join(format!("e2e-file-{}", run_base_id()));
        let dir = root.join("in");
        let staging = root.join("staging");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(dir.join("A.txt"), b"x").unwrap();
        assert!(check_folder(&dir, &staging, "A.txt").is_ok());
        assert!(check_folder(&dir, &staging, "B.txt").is_err());
        std::fs::write(staging.join("job-1.part"), b"x").unwrap();
        assert!(
            check_folder(&dir, &staging, "A.txt").is_err(),
            "a staging leftover fails"
        );
        std::fs::remove_file(staging.join("job-1.part")).unwrap();
        std::fs::write(dir.join("extra.txt"), b"x").unwrap();
        assert!(
            check_folder(&dir, &staging, "A.txt").is_err(),
            "an extra file fails"
        );
        assert!(check_folder(&dir, &root.join("gone"), "A.txt").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_file_jobs_carry_no_till_in_the_fake() {
        let mut f = Fake::with_key(E2E_FILE_KEY);
        f.queue(1, "file.write", json!({}));
        f.queue(2, "fiscal.cash_register", json!({}));
        let file = crate::device_jobs::job_json_for_tests(&f, 1);
        assert_eq!(file["kasa"], Value::Null);
        let fiscal = crate::device_jobs::job_json_for_tests(&f, 2);
        assert_eq!(fiscal["kasa"]["id"], 3);
    }
}
