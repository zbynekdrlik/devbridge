# Client Pairing & Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Single `irm | iex` deploys a new client. Admin approves on server dashboard. Virtual printer auto-provisioned.

**Architecture:** Add PairingState lifecycle to client registration. Server gates job delivery on approval. Approval auto-creates virtual printer + Windows IPP printer. Remove dead TLS code. Fix install.ps1 to run full chain.

**Tech Stack:** Rust (tonic gRPC, axum, rusqlite), Protobuf, Leptos WASM, PowerShell

---

## Task 1: Proto — Add `virtual_printer_name` to ClientIdentity

**Files:**
- `/home/newlevel/devel/devbridge/proto/devbridge.proto`

**Why:** Every downstream Rust crate depends on the generated proto types. This must land first so everything compiles.

### Steps

- [ ] Edit `proto/devbridge.proto` line 28 — add field 5 to `ClientIdentity`:

```protobuf
message ClientIdentity {
  string machine_id = 1;
  string hostname = 2;
  repeated string printer_names = 3;
  string client_version = 4;
  string virtual_printer_name = 5;
}
```

- [ ] Verify proto compiles by running:

```sh
cargo build -p devbridge-core 2>&1 | head -20
```

- [ ] Commit:

```sh
git add proto/devbridge.proto
git commit -m "proto: add virtual_printer_name to ClientIdentity

Clients will send their requested virtual printer display name during
SubscribeJobs registration. Used by server-side pairing approval to
auto-create virtual printers.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Core Types — PairingState enum + ClientRegistration fields + Config field

**Files:**
- `/home/newlevel/devel/devbridge/crates/devbridge-core/src/client_registration.rs`
- `/home/newlevel/devel/devbridge/crates/devbridge-core/src/config.rs`

**Why:** PairingState is the central type for the pairing lifecycle. ClientRegistration gains two fields. ClientConfig gains `virtual_printer_name`.

### Steps

- [ ] Add `PairingState` enum and update `ClientRegistration` in `client_registration.rs`:

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingState {
    Pending,
    Approved,
    Rejected,
}

impl PairingState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PairingState::Pending => "pending",
            PairingState::Approved => "approved",
            PairingState::Rejected => "rejected",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "approved" => PairingState::Approved,
            "rejected" => PairingState::Rejected,
            _ => PairingState::Pending,
        }
    }
}

impl std::fmt::Display for PairingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRegistration {
    pub machine_id: String,
    pub hostname: String,
    pub printer_names: Vec<String>,
    pub client_version: String,
    pub last_seen: DateTime<Utc>,
    pub is_online: bool,
    pub pairing_state: PairingState,
    pub virtual_printer_name: Option<String>,
}
```

- [ ] Update the existing test `test_client_registration_serde_roundtrip` to include the new fields:

```rust
#[test]
fn test_client_registration_serde_roundtrip() {
    let now = Utc::now();
    let reg = ClientRegistration {
        machine_id: "abc123".into(),
        hostname: "store-a-pc".into(),
        printer_names: vec!["EPSON L3270".into(), "Canon MG3600".into()],
        client_version: "0.1.0".into(),
        last_seen: now,
        is_online: true,
        pairing_state: PairingState::Pending,
        virtual_printer_name: Some("Store A".into()),
    };

    let json = serde_json::to_string(&reg).unwrap();
    let restored: ClientRegistration = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.machine_id, "abc123");
    assert_eq!(restored.hostname, "store-a-pc");
    assert_eq!(restored.printer_names.len(), 2);
    assert_eq!(restored.printer_names[0], "EPSON L3270");
    assert!(restored.is_online);
    assert_eq!(restored.pairing_state, PairingState::Pending);
    assert_eq!(restored.virtual_printer_name, Some("Store A".into()));
}

#[test]
fn test_pairing_state_roundtrip() {
    assert_eq!(PairingState::from_str("pending"), PairingState::Pending);
    assert_eq!(PairingState::from_str("approved"), PairingState::Approved);
    assert_eq!(PairingState::from_str("rejected"), PairingState::Rejected);
    assert_eq!(PairingState::from_str("unknown"), PairingState::Pending);

    assert_eq!(PairingState::Pending.as_str(), "pending");
    assert_eq!(PairingState::Approved.as_str(), "approved");
    assert_eq!(PairingState::Rejected.as_str(), "rejected");
}

#[test]
fn test_pairing_state_serde() {
    let json = serde_json::to_string(&PairingState::Approved).unwrap();
    assert_eq!(json, "\"approved\"");
    let restored: PairingState = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, PairingState::Approved);
}
```

- [ ] Add `virtual_printer_name` field to `ClientConfig` in `config.rs` (line 58, before `pub tls:`):

```rust
    /// Virtual printer name to request on server during pairing (e.g., "Store A")
    #[serde(default)]
    pub virtual_printer_name: Option<String>,
```

- [ ] Update the `test_config()` helper in `receiver.rs` tests (line 471-491) and any other test constructors of `ClientRegistration` in the workspace. Search with:

```sh
cargo build --workspace 2>&1 | grep "missing"
```

Fix every compilation error by adding the new fields with defaults (`pairing_state: PairingState::Pending`, `virtual_printer_name: None`).

- [ ] Run tests:

```sh
cargo test -p devbridge-core
```

- [ ] Commit:

```sh
git add crates/devbridge-core/src/client_registration.rs crates/devbridge-core/src/config.rs
git commit -m "core: add PairingState enum and virtual_printer_name to ClientRegistration/Config

PairingState tracks pending/approved/rejected lifecycle for client
authorization. virtual_printer_name lets clients request a display name
for auto-provisioned virtual printers on approval.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Storage Migration — DB schema + CRUD for pairing fields

**Files:**
- `/home/newlevel/devel/devbridge/crates/devbridge-server/src/storage.rs`
- `/home/newlevel/devel/devbridge/crates/devbridge-server/src/queue.rs`

**Why:** Database must store pairing_state and virtual_printer_name. Migration must set existing clients to `approved`. Need `get_client` and `update_pairing_state` methods.

### Steps

- [ ] Add migration in `Storage::new()` (after the existing `set_all_clients_offline` migration block, around line 85). Add two `ALTER TABLE` migrations:

```rust
// Migration: add pairing_state column (defaults existing clients to approved)
if conn
    .prepare("SELECT pairing_state FROM clients LIMIT 0")
    .is_err()
{
    let _ = conn.execute_batch(
        "ALTER TABLE clients ADD COLUMN pairing_state TEXT NOT NULL DEFAULT 'approved';",
    );
}

// Migration: add virtual_printer_name column
if conn
    .prepare("SELECT virtual_printer_name FROM clients LIMIT 0")
    .is_err()
{
    let _ = conn.execute_batch(
        "ALTER TABLE clients ADD COLUMN virtual_printer_name TEXT;",
    );
}
```

Note: DEFAULT 'approved' ensures existing deployed clients (pjsnvs, pjpos-client, holla-client) keep working without admin re-approval.

- [ ] Update `upsert_client` (line 428-453) to include new columns. The `ON CONFLICT DO UPDATE` must NOT overwrite `pairing_state` (so re-connecting doesn't reset an approved client to pending):

```rust
pub fn upsert_client(&self, reg: &ClientRegistration) -> Result<()> {
    let printer_names_json = serde_json::to_string(&reg.printer_names)
        .context("failed to serialize printer names")?;

    self.conn
        .execute(
            "INSERT INTO clients (machine_id, hostname, printer_names, client_version, last_seen, is_online, pairing_state, virtual_printer_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(machine_id) DO UPDATE SET
                hostname = excluded.hostname,
                printer_names = excluded.printer_names,
                client_version = excluded.client_version,
                last_seen = excluded.last_seen,
                is_online = excluded.is_online,
                virtual_printer_name = COALESCE(excluded.virtual_printer_name, clients.virtual_printer_name)",
            params![
                reg.machine_id,
                reg.hostname,
                printer_names_json,
                reg.client_version,
                reg.last_seen.to_rfc3339(),
                reg.is_online as i32,
                reg.pairing_state.as_str(),
                reg.virtual_printer_name,
            ],
        )
        .with_context(|| format!("failed to upsert client {}", reg.machine_id))?;
    Ok(())
}
```

Key: `pairing_state` is NOT in the `ON CONFLICT DO UPDATE` set. A reconnecting approved client stays approved. Only new clients get the value from the INSERT (which is `pending`).

- [ ] Update `row_to_client` (line 618-631) to read new columns:

```rust
fn row_to_client(row: &rusqlite::Row) -> rusqlite::Result<ClientRegistration> {
    use devbridge_core::client_registration::PairingState;

    let last_seen_str: String = row.get("last_seen")?;
    let printer_names_str: String = row.get("printer_names")?;
    let printer_names: Vec<String> = serde_json::from_str(&printer_names_str).unwrap_or_default();
    let pairing_str: String = row.get("pairing_state").unwrap_or_else(|_| "pending".into());

    Ok(ClientRegistration {
        machine_id: row.get("machine_id")?,
        hostname: row.get("hostname")?,
        printer_names,
        client_version: row.get("client_version")?,
        last_seen: last_seen_str.parse::<DateTime<Utc>>().unwrap_or_default(),
        is_online: row.get::<_, i32>("is_online")? != 0,
        pairing_state: PairingState::from_str(&pairing_str),
        virtual_printer_name: row.get::<_, Option<String>>("virtual_printer_name").unwrap_or(None),
    })
}
```

- [ ] Add new storage methods — `get_client` and `update_pairing_state`:

```rust
/// Get a single client by machine_id.
pub fn get_client(&self, machine_id: &str) -> Result<Option<ClientRegistration>> {
    let mut stmt = self
        .conn
        .prepare("SELECT * FROM clients WHERE machine_id = ?1")
        .context("failed to prepare get-client query")?;

    let mut rows = stmt
        .query_map(params![machine_id], row_to_client)
        .context("failed to query client")?;

    match rows.next() {
        Some(Ok(c)) => Ok(Some(c)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// Update only the pairing_state for a client.
pub fn update_pairing_state(
    &self,
    machine_id: &str,
    state: devbridge_core::client_registration::PairingState,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let rows = self
        .conn
        .execute(
            "UPDATE clients SET pairing_state = ?1, last_seen = ?2 WHERE machine_id = ?3",
            params![state.as_str(), now, machine_id],
        )
        .with_context(|| format!("failed to update pairing state for {machine_id}"))?;

    if rows == 0 {
        anyhow::bail!("client {machine_id} not found");
    }
    Ok(())
}
```

- [ ] Add delegation methods in `queue.rs`:

```rust
pub fn get_client(&self, machine_id: &str) -> Result<Option<ClientRegistration>> {
    let storage = self.storage.lock().unwrap();
    storage.get_client(machine_id)
}

pub fn update_pairing_state(
    &self,
    machine_id: &str,
    state: devbridge_core::client_registration::PairingState,
) -> Result<()> {
    let storage = self.storage.lock().unwrap();
    storage.update_pairing_state(machine_id, state)
}
```

- [ ] Write tests in `storage.rs` tests module:

```rust
#[test]
fn test_client_pairing_state_default() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(&dir.path().join("test.db")).unwrap();

    let reg = ClientRegistration {
        machine_id: "test-1".into(),
        hostname: "host-1".into(),
        printer_names: vec![],
        client_version: "0.1.0".into(),
        last_seen: Utc::now(),
        is_online: true,
        pairing_state: PairingState::Pending,
        virtual_printer_name: Some("Store A".into()),
    };
    storage.upsert_client(&reg).unwrap();

    let client = storage.get_client("test-1").unwrap().unwrap();
    assert_eq!(client.pairing_state, PairingState::Pending);
    assert_eq!(client.virtual_printer_name, Some("Store A".into()));
}

#[test]
fn test_update_pairing_state() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(&dir.path().join("test.db")).unwrap();

    let reg = ClientRegistration {
        machine_id: "test-2".into(),
        hostname: "host-2".into(),
        printer_names: vec![],
        client_version: "0.1.0".into(),
        last_seen: Utc::now(),
        is_online: true,
        pairing_state: PairingState::Pending,
        virtual_printer_name: None,
    };
    storage.upsert_client(&reg).unwrap();

    storage.update_pairing_state("test-2", PairingState::Approved).unwrap();
    let client = storage.get_client("test-2").unwrap().unwrap();
    assert_eq!(client.pairing_state, PairingState::Approved);
}

#[test]
fn test_upsert_does_not_overwrite_pairing_state() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(&dir.path().join("test.db")).unwrap();

    // First insert as pending
    let reg = ClientRegistration {
        machine_id: "test-3".into(),
        hostname: "host-3".into(),
        printer_names: vec![],
        client_version: "0.1.0".into(),
        last_seen: Utc::now(),
        is_online: true,
        pairing_state: PairingState::Pending,
        virtual_printer_name: None,
    };
    storage.upsert_client(&reg).unwrap();

    // Approve it
    storage.update_pairing_state("test-3", PairingState::Approved).unwrap();

    // Re-upsert (simulates client reconnect)
    let reg2 = ClientRegistration {
        machine_id: "test-3".into(),
        hostname: "host-3-updated".into(),
        printer_names: vec!["Printer1".into()],
        client_version: "0.2.0".into(),
        last_seen: Utc::now(),
        is_online: true,
        pairing_state: PairingState::Pending, // Client always sends pending
        virtual_printer_name: None,
    };
    storage.upsert_client(&reg2).unwrap();

    // pairing_state must still be approved
    let client = storage.get_client("test-3").unwrap().unwrap();
    assert_eq!(client.pairing_state, PairingState::Approved);
    assert_eq!(client.hostname, "host-3-updated"); // other fields updated
}
```

- [ ] Run tests:

```sh
cargo test -p devbridge-server
```

- [ ] Commit:

```sh
git add crates/devbridge-server/src/storage.rs crates/devbridge-server/src/queue.rs
git commit -m "storage: add pairing_state and virtual_printer_name to clients table

Migration sets existing clients to 'approved' so deployed machines keep
working. upsert_client preserves pairing_state on reconnect. New methods:
get_client, update_pairing_state.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Virtual Printer API Bug Fixes — create/delete IPP + Windows printer

**Files:**
- `/home/newlevel/devel/devbridge/crates/devbridge-dashboard/src/api/virtual_printers.rs`

**Why:** Independent of pairing. Three bugs: (1) create doesn't register in IPP, (2) create doesn't accept paired_client_id, (3) delete doesn't clean up IPP + Windows printer. Fix these first since approval depends on working VP create.

### Steps

- [ ] **Bug 1+2 fix:** Update `CreateRequest` and `create_virtual_printer` to accept `paired_client_id` and register with IPP service:

```rust
#[derive(Deserialize)]
struct CreateRequest {
    display_name: String,
    #[serde(default)]
    paired_client_id: Option<String>,
}
```

In `create_virtual_printer`, after `queue.insert_virtual_printer(&vp)`, add IPP registration (use the pattern from `update_virtual_printer` lines 128-133):

```rust
    let vp = VirtualPrinter {
        id: Uuid::new_v4().to_string(),
        ipp_name: slugify(&name),
        display_name: name.clone(),
        paired_client_id: body.paired_client_id,
        created_at: now,
        updated_at: now,
    };

    queue
        .insert_virtual_printer(&vp)
        .map_err(|_| StatusCode::CONFLICT)?;

    // Register in IPP service (fixes bug: previously required server restart)
    if let Some(ipp) = &state.ipp_server {
        let _ = ipp.add_printer(&vp).await;
    }

    // Register Windows IPP printer (server mode only)
    if cfg!(target_os = "windows") && state.mode == "server" {
        let printer_name = vp.display_name.clone();
        tokio::task::spawn_blocking(move || {
            let script = format!(
                r#"$port = 'http://127.0.0.1:631/ipp/print'; rundll32.exe printui.dll,PrintUIEntry /if /b "{}" /r "$port" /m "Microsoft IPP Class Driver" /q"#,
                printer_name
            );
            let _ = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .output();
        });
    }
```

- [ ] **Bug 3 fix:** Update `delete_virtual_printer` to clean up IPP service and Windows printer:

```rust
async fn delete_virtual_printer(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let Some(queue) = &state.queue else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    // Get VP details before deleting (need ipp_name and display_name for cleanup)
    let vp = queue
        .get_virtual_printer(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    queue
        .delete_virtual_printer(&id)
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // Remove from IPP service in-memory registry
    if let Some(ipp) = &state.ipp_server {
        ipp.remove_printer(&vp.ipp_name).await;
    }

    // Remove Windows IPP printer (server mode only)
    if cfg!(target_os = "windows") && state.mode == "server" {
        let printer_name = vp.display_name;
        tokio::task::spawn_blocking(move || {
            let script = format!(
                r#"Remove-Printer -Name '{}' -ErrorAction SilentlyContinue"#,
                printer_name
            );
            let _ = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .output();
        });
    }

    Ok(StatusCode::NO_CONTENT)
}
```

- [ ] Add test for create with `paired_client_id`:

```rust
#[tokio::test]
async fn test_create_virtual_printer_with_paired_client() {
    let state = test_state_with_queue();
    let app = crate::build_router(state);

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/virtual-printers")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"display_name": "Store B", "paired_client_id": "client-b"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(created["display_name"], "Store B");
    assert_eq!(created["paired_client_id"], "client-b");
}
```

- [ ] Run tests:

```sh
cargo test -p devbridge-dashboard
```

- [ ] Commit:

```sh
git add crates/devbridge-dashboard/src/api/virtual_printers.rs
git commit -m "fix: virtual printer create/delete now update IPP service in-memory

Bug 1: POST /virtual-printers now calls ipp.add_printer() after DB insert
Bug 2: POST /virtual-printers accepts optional paired_client_id
Bug 3: DELETE /virtual-printers now calls ipp.remove_printer() and removes
Windows IPP printer registration

Previously, create/delete required a server restart for IPP changes to
take effect.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Dispatch Pairing Logic — gate jobs on approval, remove auto-pair

**Files:**
- `/home/newlevel/devel/devbridge/crates/devbridge-server/src/dispatch.rs`

**Why:** The dispatch service must check pairing_state after client registration. Pending/rejected clients must not receive jobs. The auto-pairing block (lines 115-134) must be removed entirely.

### Steps

- [ ] In `subscribe_jobs`, after the `upsert_client` call (line 93-95), add a pairing state check. The `reg` construction (lines 85-92) must include the new fields:

```rust
        // Auto-register client in storage
        let reg = ClientRegistration {
            machine_id: identity.machine_id.clone(),
            hostname: identity.hostname.clone(),
            printer_names: identity.printer_names.clone(),
            client_version: identity.client_version.clone(),
            last_seen: Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Pending,
            virtual_printer_name: if identity.virtual_printer_name.is_empty() {
                None
            } else {
                Some(identity.virtual_printer_name.clone())
            },
        };
        if let Err(e) = self.queue.upsert_client(&reg) {
            error!(error = %e, "failed to register client");
        }

        // Check pairing state — only approved clients receive jobs
        let pairing_state = self
            .queue
            .get_client(&identity.machine_id)
            .ok()
            .flatten()
            .map(|c| c.pairing_state)
            .unwrap_or(devbridge_core::client_registration::PairingState::Pending);

        if pairing_state == devbridge_core::client_registration::PairingState::Rejected {
            return Err(Status::permission_denied("client has been rejected"));
        }
```

- [ ] DELETE the entire auto-pair block (lines 115-134):

```rust
        // Auto-pair: if any virtual printer has no paired client, pair with this one.
        // Runs AFTER register_client so the per-client channel exists for job routing.
        if let Ok(vps) = self.queue.list_virtual_printers() {
            // ... entire block ...
        }
```

- [ ] In the job delivery loop (inside the `tokio::spawn`), gate job sending on pairing_state. Pending clients should stay connected but not receive jobs. Add a check before sending:

After `let queue = Arc::clone(&self.queue);` (line 137), clone what we need:

```rust
        let pairing_machine_id = machine_id.clone();
        let pairing_queue = Arc::clone(&self.queue);
```

Then at the top of the loop body (inside `tokio::spawn`), before checking per-client channel:

```rust
            loop {
                // Pending clients stay connected but don't receive jobs
                // (re-check each iteration so approval takes effect immediately)
                let is_approved = pairing_queue
                    .get_client(&pairing_machine_id)
                    .ok()
                    .flatten()
                    .is_some_and(|c| {
                        c.pairing_state
                            == devbridge_core::client_registration::PairingState::Approved
                    });

                if !is_approved {
                    // Wait a bit and re-check (don't spin)
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }

                // Register for default queue notification BEFORE checking
                let notified = queue.notified();
                // ... rest of existing loop ...
```

- [ ] Run tests:

```sh
cargo test -p devbridge-server
```

- [ ] Commit:

```sh
git add crates/devbridge-server/src/dispatch.rs
git commit -m "dispatch: gate job delivery on pairing approval, remove auto-pair

Pending clients connect and stay connected but receive no jobs until
admin approves. Rejected clients get permission_denied. The auto-pair
block is removed — virtual printers are created explicitly on approval.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Client — send virtual_printer_name in ClientIdentity

**Files:**
- `/home/newlevel/devel/devbridge/crates/devbridge-client/src/receiver.rs`

**Why:** Client must send the new proto field so the server knows what virtual printer name to create on approval.

### Steps

- [ ] Add `virtual_printer_name` field to `Receiver` struct (line 34, after `printer_display_name`):

```rust
    virtual_printer_name: Option<String>,
```

- [ ] Read it in `new()` (line 62, after `printer_display_name`):

```rust
            virtual_printer_name: config.virtual_printer_name.clone(),
```

- [ ] Add it to `ClientIdentity` construction (line 118-123):

```rust
        let identity = ClientIdentity {
            machine_id: self.machine_id.clone(),
            hostname: self.hostname.clone(),
            printer_names,
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            virtual_printer_name: self.virtual_printer_name.clone().unwrap_or_default(),
        };
```

- [ ] Update `test_config()` in tests (line 471-491) to include `virtual_printer_name: None`:

```rust
    fn test_config() -> ClientConfig {
        ClientConfig {
            // ... existing fields ...
            printer_display_name: None,
            virtual_printer_name: None,
            tls: TlsConfig {
                cert_file: "".into(),
                key_file: "".into(),
                ca_file: "".into(),
            },
        }
    }
```

- [ ] Add test:

```rust
    #[test]
    fn test_virtual_printer_name_from_config() {
        let mut config = test_config();
        config.virtual_printer_name = Some("Store A".into());

        let receiver = Receiver::new(&config);
        assert_eq!(receiver.virtual_printer_name, Some("Store A".into()));
    }
```

- [ ] Run tests:

```sh
cargo test -p devbridge-client
```

- [ ] Commit:

```sh
git add crates/devbridge-client/src/receiver.rs
git commit -m "client: send virtual_printer_name in ClientIdentity

Reads from config.toml virtual_printer_name field and includes it in the
gRPC SubscribeJobs request so the server knows what to name the virtual
printer on approval.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Dashboard API — approve/reject endpoints

**Files:**
- `/home/newlevel/devel/devbridge/crates/devbridge-dashboard/src/api/clients.rs`

**Why:** Admin needs HTTP endpoints to approve or reject pending clients. Approve auto-creates virtual printer + IPP + Windows printer.

### Steps

- [ ] Add imports and new route handlers to `clients.rs`. The router gains two new POST routes:

```rust
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use devbridge_core::client_registration::PairingState;
use devbridge_core::virtual_printer::{VirtualPrinter, slugify};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/clients", get(list_clients))
        .route("/clients/{id}/approve", post(approve_client))
        .route("/clients/{id}/reject", post(reject_client))
}
```

- [ ] Update `list_clients` response to include `pairing_state` and `virtual_printer_name`:

```rust
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
                        "pairing_state": c.pairing_state.as_str(),
                        "virtual_printer_name": c.virtual_printer_name,
                    })
                })
                .collect();
            Json(json!(json_clients))
        }
        Err(_) => Json(json!([])),
    }
}
```

- [ ] Implement `approve_client` — the core of the pairing flow:

```rust
async fn approve_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let Some(queue) = &state.queue else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    // Get client details
    let client = queue
        .get_client(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if client.pairing_state == PairingState::Approved {
        return Ok(Json(json!({"status": "already_approved"})));
    }

    // Set approved
    queue
        .update_pairing_state(&id, PairingState::Approved)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Auto-create virtual printer if client requested one
    let mut vp_created = None;
    if let Some(ref vp_name) = client.virtual_printer_name {
        let now = Utc::now();
        let vp = VirtualPrinter {
            id: Uuid::new_v4().to_string(),
            ipp_name: slugify(vp_name),
            display_name: vp_name.clone(),
            paired_client_id: Some(id.clone()),
            created_at: now,
            updated_at: now,
        };

        if queue.insert_virtual_printer(&vp).is_ok() {
            // Register in IPP service
            if let Some(ipp) = &state.ipp_server {
                let _ = ipp.add_printer(&vp).await;
            }

            // Register Windows IPP printer
            if cfg!(target_os = "windows") && state.mode == "server" {
                let printer_name = vp.display_name.clone();
                tokio::task::spawn_blocking(move || {
                    let script = format!(
                        r#"$port = 'http://127.0.0.1:631/ipp/print'; rundll32.exe printui.dll,PrintUIEntry /if /b "{}" /r "$port" /m "Microsoft IPP Class Driver" /q"#,
                        printer_name
                    );
                    let _ = std::process::Command::new("powershell")
                        .args(["-NoProfile", "-Command", &script])
                        .output();
                });
            }

            vp_created = Some(json!({
                "id": vp.id,
                "display_name": vp.display_name,
                "ipp_name": vp.ipp_name,
            }));
        }
    }

    Ok(Json(json!({
        "status": "approved",
        "client_id": id,
        "virtual_printer": vp_created,
    })))
}
```

- [ ] Implement `reject_client`:

```rust
async fn reject_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let Some(queue) = &state.queue else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    queue
        .get_client(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    queue
        .update_pairing_state(&id, PairingState::Rejected)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(json!({
        "status": "rejected",
        "client_id": id,
    })))
}
```

- [ ] Update existing tests and add new ones:

```rust
#[tokio::test]
async fn test_list_clients_includes_pairing_state() {
    let state = test_state_with_queue();
    let queue = state.queue.as_ref().unwrap();

    let reg = devbridge_core::client_registration::ClientRegistration {
        machine_id: "test-mc".into(),
        hostname: "test-host".into(),
        printer_names: vec!["Printer1".into()],
        client_version: "0.1.0".into(),
        last_seen: chrono::Utc::now(),
        is_online: true,
        pairing_state: devbridge_core::client_registration::PairingState::Pending,
        virtual_printer_name: Some("Store A".into()),
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

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let client = json.as_array().unwrap()[0].as_object().unwrap();
    assert!(client.contains_key("pairing_state"));
    assert!(client.contains_key("virtual_printer_name"));
    assert_eq!(client["pairing_state"], "pending");
    assert_eq!(client["virtual_printer_name"], "Store A");
}

#[tokio::test]
async fn test_approve_client_creates_virtual_printer() {
    let state = test_state_with_queue();
    let queue = state.queue.as_ref().unwrap();

    let reg = devbridge_core::client_registration::ClientRegistration {
        machine_id: "approve-test".into(),
        hostname: "host-1".into(),
        printer_names: vec![],
        client_version: "0.1.0".into(),
        last_seen: chrono::Utc::now(),
        is_online: true,
        pairing_state: devbridge_core::client_registration::PairingState::Pending,
        virtual_printer_name: Some("My Printer".into()),
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
    assert!(json["virtual_printer"].is_object());
    assert_eq!(json["virtual_printer"]["display_name"], "My Printer");

    // Verify VP was created in storage
    let vps = queue.list_virtual_printers().unwrap();
    assert_eq!(vps.len(), 1);
    assert_eq!(vps[0].display_name, "My Printer");
    assert_eq!(vps[0].paired_client_id, Some("approve-test".into()));

    // Verify client is now approved
    let client = queue.get_client("approve-test").unwrap().unwrap();
    assert_eq!(client.pairing_state, devbridge_core::client_registration::PairingState::Approved);
}

#[tokio::test]
async fn test_reject_client() {
    let state = test_state_with_queue();
    let queue = state.queue.as_ref().unwrap();

    let reg = devbridge_core::client_registration::ClientRegistration {
        machine_id: "reject-test".into(),
        hostname: "host-2".into(),
        printer_names: vec![],
        client_version: "0.1.0".into(),
        last_seen: chrono::Utc::now(),
        is_online: true,
        pairing_state: devbridge_core::client_registration::PairingState::Pending,
        virtual_printer_name: None,
    };
    queue.upsert_client(&reg).unwrap();

    let app = crate::build_router(state);
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
}

#[tokio::test]
async fn test_approve_nonexistent_client_404() {
    let app = crate::build_router(test_state_with_queue());
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
}
```

- [ ] Update the existing `test_list_clients_with_registered_client` test to include new fields in the `ClientRegistration` construction.

- [ ] Run tests:

```sh
cargo test -p devbridge-dashboard
```

- [ ] Commit:

```sh
git add crates/devbridge-dashboard/src/api/clients.rs
git commit -m "api: add approve/reject endpoints for client pairing

POST /clients/{id}/approve sets pairing_state to approved and
auto-creates a virtual printer + IPP + Windows printer registration
if the client requested a virtual_printer_name.

POST /clients/{id}/reject sets pairing_state to rejected.

GET /clients now includes pairing_state and virtual_printer_name fields.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Dashboard UI — Pending Clients Section

**Files:**
- `/home/newlevel/devel/devbridge/crates/devbridge-ui/src/api.rs`
- `/home/newlevel/devel/devbridge/crates/devbridge-ui/src/pages/dashboard.rs`

**Why:** Admin needs to see pending clients and click Approve/Reject on the server dashboard.

### Steps

- [ ] Add API functions in `api.rs`:

```rust
pub async fn approve_client(id: &str) -> Result<Value, String> {
    Request::post(&format!("/api/clients/{id}/approve"))
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}

pub async fn reject_client(id: &str) -> Result<Value, String> {
    Request::post(&format!("/api/clients/{id}/reject"))
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("Parse failed: {e}"))
}
```

- [ ] Add `PendingClients` component in `dashboard.rs` (inside `ServerDashboardView`, after the stats bar and before the job list). Create a new `#[component]` function:

```rust
#[component]
fn PendingClients() -> impl IntoView {
    let refresh = RwSignal::new(0u32);
    let clients = LocalResource::new(move || {
        let _ = refresh.get();
        api::fetch_clients()
    });

    view! {
        {move || {
            clients.read().as_ref().map(|res| {
                match &**res {
                    Ok(all_clients) => {
                        let pending: Vec<_> = all_clients
                            .iter()
                            .filter(|c| {
                                c.get("pairing_state")
                                    .and_then(|s| s.as_str())
                                    == Some("pending")
                            })
                            .cloned()
                            .collect();

                        if pending.is_empty() {
                            return view! {}.into_any();
                        }

                        view! {
                            <div class="card" style="margin-bottom: 1rem; border-left: 4px solid #f59e0b">
                                <h3 style="margin: 0 0 0.75rem 0; font-size: 1em; color: #f59e0b">
                                    "⏳ Pending Clients (" {pending.len()} ")"
                                </h3>
                                {pending.into_iter().map(|c| {
                                    let machine_id = c.get("machine_id").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                                    let hostname = c.get("hostname").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                                    let vp_name = c.get("virtual_printer_name").and_then(|v| v.as_str()).unwrap_or("(none)").to_string();
                                    let mid_approve = machine_id.clone();
                                    let mid_reject = machine_id.clone();
                                    let refresh_approve = refresh;
                                    let refresh_reject = refresh;

                                    view! {
                                        <div style="display: flex; align-items: center; gap: 1rem; padding: 0.5rem 0; border-top: 1px solid var(--border)">
                                            <div style="flex: 1">
                                                <div style="font-weight: 600">{&machine_id}</div>
                                                <div style="font-size: 0.85em; color: var(--text-muted)">
                                                    {hostname} " · printer: " {vp_name}
                                                </div>
                                            </div>
                                            <button
                                                style="background: #22c55e; color: white; border: none; padding: 0.3rem 0.75rem; border-radius: 4px; cursor: pointer"
                                                on:click=move |_| {
                                                    let id = mid_approve.clone();
                                                    leptos::task::spawn_local(async move {
                                                        let _ = api::approve_client(&id).await;
                                                        refresh_approve.update(|n| *n += 1);
                                                    });
                                                }
                                            >
                                                "Approve"
                                            </button>
                                            <button
                                                style="background: #ef4444; color: white; border: none; padding: 0.3rem 0.75rem; border-radius: 4px; cursor: pointer"
                                                on:click=move |_| {
                                                    let id = mid_reject.clone();
                                                    leptos::task::spawn_local(async move {
                                                        let _ = api::reject_client(&id).await;
                                                        refresh_reject.update(|n| *n += 1);
                                                    });
                                                }
                                            >
                                                "Reject"
                                            </button>
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        }.into_any()
                    }
                    Err(_) => view! {}.into_any(),
                }
            })
        }}
    }
}
```

- [ ] Add `<PendingClients />` inside `ServerDashboardView`, after the stats bar `</div>` and before the job list section.

- [ ] Build UI to verify:

```sh
cd /home/newlevel/devel/devbridge/crates/devbridge-ui && trunk build 2>&1 | tail -5
```

- [ ] Commit:

```sh
git add crates/devbridge-ui/src/api.rs crates/devbridge-ui/src/pages/dashboard.rs
git commit -m "ui: add pending clients section to server dashboard

Shows pending clients with hostname, requested printer name, and
Approve/Reject buttons. Approving creates the virtual printer and
starts job delivery. Cards disappear after action.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Installer Fixes — install.ps1 calls post-install.ps1, add VirtualPrinterName

**Files:**
- `/home/newlevel/devel/devbridge/installer/install.ps1`
- `/home/newlevel/devel/devbridge/installer/post-install.ps1`

**Why:** The one-liner `irm | iex` must do the full chain: download NSIS, install, configure, start service. Currently install.ps1 never calls post-install.ps1.

### Steps

- [ ] In `install.ps1`, after the NSIS installer runs (line 75) and before "Verify service" (line 78), add the post-install call. Insert before `# --- Verify service ---`:

```powershell
# --- Run post-install configuration ---
$installDir = "C:\Program Files\DevBridge"
$postInstallScript = Join-Path $installDir "post-install.ps1"
if (-not (Test-Path $postInstallScript)) {
    # Also check the _up_\_up_ Tauri resource path
    $postInstallScript = Join-Path $installDir "_up_\_up_\installer\post-install.ps1"
}
if (Test-Path $postInstallScript) {
    Write-Host "Running post-install configuration..."
    # Pass through all parameters that were provided via environment variables
    # (irm | iex doesn't support script params, so we use env vars)
    $postArgs = @()
    if ($env:DEVBRIDGE_MODE) { $postArgs += "-Mode", $env:DEVBRIDGE_MODE }
    else { $postArgs += "-Mode", "client" }
    if ($env:DEVBRIDGE_SERVER_HOST) { $postArgs += "-ServerHost", $env:DEVBRIDGE_SERVER_HOST }
    if ($env:DEVBRIDGE_TARGET_PRINTER) { $postArgs += "-TargetPrinter", $env:DEVBRIDGE_TARGET_PRINTER }
    if ($env:DEVBRIDGE_CLIENT_ID) { $postArgs += "-ClientId", $env:DEVBRIDGE_CLIENT_ID }
    if ($env:DEVBRIDGE_VIRTUAL_PRINTER_NAME) { $postArgs += "-VirtualPrinterName", $env:DEVBRIDGE_VIRTUAL_PRINTER_NAME }
    if ($env:DEVBRIDGE_PRINT_BACKEND) { $postArgs += "-PrintBackend", $env:DEVBRIDGE_PRINT_BACKEND }
    if ($env:DEVBRIDGE_PRINTER_ADDRESS) { $postArgs += "-PrinterAddress", $env:DEVBRIDGE_PRINTER_ADDRESS }
    if ($env:DEVBRIDGE_DASHBOARD_PORT) { $postArgs += "-DashboardPort", $env:DEVBRIDGE_DASHBOARD_PORT }
    if ($env:DEVBRIDGE_GHOSTSCRIPT_DEVICE) { $postArgs += "-GhostscriptDevice", $env:DEVBRIDGE_GHOSTSCRIPT_DEVICE }
    if ($env:DEVBRIDGE_GHOSTSCRIPT_RESOLUTION) { $postArgs += "-GhostscriptResolution", $env:DEVBRIDGE_GHOSTSCRIPT_RESOLUTION }

    & $postInstallScript @postArgs
} else {
    Write-Warning "post-install.ps1 not found — manual configuration required."
}
```

- [ ] In `post-install.ps1`, add `-VirtualPrinterName` parameter (line 26, before `$PrintBackend`):

```powershell
    [string]$VirtualPrinterName = "",
```

- [ ] In `post-install.ps1`, remove the `-CertsSource` parameter (line 19) and the cert-copying block (lines 58-63):

Delete this parameter:
```powershell
    [string]$CertsSource = "",
```

Delete this block:
```powershell
# ── Copy TLS certificates ──────────────────────────────────────────────────
$certsDir = Join-Path $DataDir "certs"
if ($CertsSource -and (Test-Path $CertsSource)) {
    Write-Host "Copying certificates from $CertsSource"
    Copy-Item "$CertsSource\*" $certsDir -Force
}
```

- [ ] In `post-install.ps1`, add `virtual_printer_name` to the client config template (after the `printer_tls` line, around line 205):

```powershell
$(if ($VirtualPrinterName) { "virtual_printer_name = `"$VirtualPrinterName`"" })
```

- [ ] Commit:

```sh
git add installer/install.ps1 installer/post-install.ps1
git commit -m "installer: install.ps1 calls post-install.ps1, add VirtualPrinterName

install.ps1 now locates and calls post-install.ps1 after NSIS install,
passing through configuration via environment variables (since irm | iex
doesn't support script parameters).

post-install.ps1 gains -VirtualPrinterName param written to config.toml.
Removed dead -CertsSource param and cert-copying logic.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: TLS Cleanup — remove dead TLS from configs and templates

**Files:**
- `/home/newlevel/devel/devbridge/config/default.toml`
- `/home/newlevel/devel/devbridge/deploy/config-templates/server.toml`
- `/home/newlevel/devel/devbridge/deploy/config-templates/client.toml`
- `/home/newlevel/devel/devbridge/installer/post-install.ps1`

**Why:** TLS is dead code. gRPC runs plaintext over WireGuard. Config templates should not include `[*.tls]` sections. Keep the `TlsConfig` struct in Rust code so old deployed config.toml files still parse.

### Steps

- [ ] Remove `[server.tls]` and `[client.tls]` sections from `config/default.toml`. Add a comment about `virtual_printer_name`:

```toml
[general]
mode = "server"
log_level = "info"
data_dir = "C:\\ProgramData\\DevBridge"

[server]
ipp_port = 631
grpc_port = 50051
dashboard_port = 9120
printer_name = "DevBridge"
spool_dir = "C:\\ProgramData\\DevBridge\\spool"

[client]
server_address = "10.0.0.1:50051"
target_printer = "HP LaserJet"
dashboard_port = 9120
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60
# client_id = "my-unique-id"
# virtual_printer_name = "Store A"
# print_backend = "windows_spooler"
# printer_address = "10.78.5.9:9100"
# ghostscript_device = "ppmraw"
# ghostscript_resolution = 600
# printer_display_name = "Canon MG3600"

[jobs]
max_retries = 3
retry_delay_secs = 30
job_expiry_hours = 24
max_payload_size_mb = 100
```

IMPORTANT: Since the Rust `Config` struct still has `tls: TlsConfig` as a required field, we must make TlsConfig default-able. Update `config.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TlsConfig {
    #[serde(default)]
    pub cert_file: String,
    #[serde(default)]
    pub key_file: String,
    #[serde(default)]
    pub ca_file: String,
}
```

And add `#[serde(default)]` to both `ServerConfig.tls` and `ClientConfig.tls`:

```rust
    #[serde(default)]
    pub tls: TlsConfig,
```

- [ ] Remove `[server.tls]` section from `deploy/config-templates/server.toml`.

- [ ] Remove `[client.tls]` section from `deploy/config-templates/client.toml`.

- [ ] In `post-install.ps1`, remove the `[server.tls]` and `[client.tls]` blocks from both config templates (server template lines 152-156, client template lines 209-212).

- [ ] Update the `VALID_TOML` constant in `config.rs` tests to still include `[server.tls]` and `[client.tls]` sections — this proves backward compatibility (old configs with TLS sections still parse).

- [ ] Add a new test proving TLS-free configs parse:

```rust
#[test]
fn test_config_without_tls_sections() {
    let toml = r#"
[general]
mode = "server"
log_level = "info"
data_dir = "/tmp/devbridge"

[server]
ipp_port = 631
grpc_port = 50051
dashboard_port = 9090
printer_name = "TestPrinter"
spool_dir = "/tmp/spool"

[client]
server_address = "127.0.0.1:50051"
target_printer = "LocalPrinter"
dashboard_port = 9120
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60

[jobs]
max_retries = 3
retry_delay_secs = 10
job_expiry_hours = 24
max_payload_size_mb = 50
"#;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(toml.as_bytes()).unwrap();

    let config = Config::load(tmp.path()).unwrap();
    assert_eq!(config.server.tls.cert_file, "");
    assert_eq!(config.client.tls.cert_file, "");
}
```

- [ ] Run tests:

```sh
cargo test --workspace
```

- [ ] Commit:

```sh
git add config/default.toml deploy/config-templates/ crates/devbridge-core/src/config.rs installer/post-install.ps1
git commit -m "cleanup: remove dead TLS sections from config templates

gRPC runs plaintext over WireGuard. TLS config was parsed but never
used. Removed [*.tls] sections from default.toml and deploy templates.
TlsConfig struct kept with #[serde(default)] so old deployed configs
still parse.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Delete generate-certs.ps1

**Files:**
- `/home/newlevel/devel/devbridge/installer/generate-certs.ps1`

**Why:** Dead code. Certs were never used by gRPC. Misleads future developers.

### Steps

- [ ] Delete the file:

```sh
git rm installer/generate-certs.ps1
```

- [ ] Commit:

```sh
git commit -m "cleanup: delete unused generate-certs.ps1

TLS certificates were never consumed by the gRPC transport (plaintext
over WireGuard). Removing to avoid confusion.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: CLAUDE.md Updates — Production inventory + deployment rules

**Files:**
- `/home/newlevel/devel/devbridge/CLAUDE.md`

**Why:** CLAUDE.md needs production machine inventory and deployment rules so agents know the landscape.

### Steps

- [ ] Replace the "Certificates / TLS" section (lines 172-176) with:

```markdown
## Certificates / TLS

gRPC runs plaintext (`http://`) over WireGuard VPN tunnels. The `TlsConfig`
struct exists for backward compatibility with old config files but is never
used. `printer_tls` in ClientConfig is unrelated — it controls HTTPS/IPPS
for direct printer connections (e.g., Epson with self-signed certs).

## Production Machines

| Machine    | Hostname      | IP          | Client ID      | Target Printer          | Virtual Printer | MCP Server       |
|------------|---------------|-------------|----------------|-------------------------|-----------------|------------------|
| pz-server  | stagebox1-snv | 10.77.8.200 | —              | —                       | —               | win-pz-server    |
| pz-snv     | pz-snv        | 10.78.2.10  | pjsnvs         | EPSON L3270             | SNV Store       | win-pz-snv       |
| pjpos      | moderatori    | 10.77.9.235 | pjpos-client   | Microsoft Print to PDF  | PJPOS           | win-print-client |
| pz-holla   | pz-holla      | 10.88.1.100 | holla-client   | Canon MG3600            | Holla Store     | win-pz-holla     |

## Deployment Rules

1. **NEVER manually write config.toml or install prerequisites by hand.** Always use `irm | iex` with environment variables. If the installer doesn't handle something, fix the installer.
2. **New client deployment:** Set env vars, run one-liner, approve on server dashboard.
3. **Upgrades:** Same one-liner. NSIS installer upgrades in-place. post-install.ps1 is idempotent.
```

- [ ] Commit:

```sh
git add CLAUDE.md
git commit -m "docs: add production machine inventory and deployment rules to CLAUDE.md

Documents all four production machines with IPs, client IDs, printers,
and MCP server names. Adds rule: never manually write config, always
use the installer.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: Fix All Compilation Errors Across Workspace

**Files:** Various — every file that constructs `ClientRegistration` or `ClientConfig`.

**Why:** Tasks 2-6 add fields to shared types. Some files outside the task scope may also construct these types (e.g., test helpers in `clients.rs`, `virtual_printers.rs`).

### Steps

- [ ] Build the full workspace and fix every error:

```sh
cargo build --workspace 2>&1 | grep -E "error\[" | head -30
```

Common fixes needed:
- `crates/devbridge-dashboard/src/api/clients.rs` tests: add `pairing_state` and `virtual_printer_name` to `ClientRegistration` construction
- `crates/devbridge-client/src/receiver.rs` tests: add `virtual_printer_name` to `test_config()`
- Any other test that constructs `ClientRegistration` or `ClientConfig`

- [ ] Run full test suite:

```sh
cargo test --workspace
```

- [ ] Run clippy:

```sh
cargo clippy --workspace --all-targets -- -D warnings
```

- [ ] Run fmt:

```sh
cargo fmt --all -- --check
```

- [ ] Commit any remaining fixes:

```sh
git add -A
git commit -m "fix: resolve compilation errors from new ClientRegistration/Config fields

Update all test helpers and constructors across the workspace to include
pairing_state and virtual_printer_name fields.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Dependency Graph

```
Task 1 (proto)
  └─→ Task 2 (core types)
        ├─→ Task 3 (storage)
        │     ├─→ Task 5 (dispatch)
        │     └─→ Task 7 (API approve/reject)
        │           └─→ Task 8 (UI pending clients)
        └─→ Task 6 (client sending VP name)
Task 4 (VP bug fixes) — independent, can land any time
Task 9 (installer) — independent of Rust code
Task 10 (TLS cleanup) — independent, low risk
Task 11 (delete generate-certs) — independent
Task 12 (CLAUDE.md) — independent
Task 13 (fix compilation) — after all code tasks
```

## Post-Deploy Verification

After CI deploys all changes:

1. **Existing clients still work:** Check that pjsnvs, pjpos-client, holla-client are online on http://10.77.8.200:9120 dashboard. Their pairing_state should be `approved` from the migration.

2. **Server dashboard shows pending section:** Navigate to http://10.77.8.200:9120, verify no pending clients shown (all existing are approved).

3. **Virtual printer create/delete works without restart:** Create a test VP via dashboard, verify it appears in IPP service. Delete it, verify cleanup.

4. **New client E2E test:** On a test machine:
   ```powershell
   $env:DEVBRIDGE_MODE = "client"
   $env:DEVBRIDGE_SERVER_HOST = "10.77.8.200"
   $env:DEVBRIDGE_CLIENT_ID = "test-pairing"
   $env:DEVBRIDGE_TARGET_PRINTER = "Microsoft Print to PDF"
   $env:DEVBRIDGE_VIRTUAL_PRINTER_NAME = "Test Pairing"
   irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
   ```
   Verify client appears as pending on dashboard. Approve. Verify VP created. Send test print.
