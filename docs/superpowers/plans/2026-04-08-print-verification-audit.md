# Print Verification & Audit Evidence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add physical delivery verification to all print backends and enrich the audit trail with machine-verifiable evidence, so "completed" actually means paper came out.

**Architecture:** Backends emit a new `Verified` stage event with evidence (EventID 307, IPP job-state, CUPS lpstat). The proto `JobCompletion` gains `verification_method`, `verification_evidence`, and `client_id` fields. The server validates the completing client matches the paired client and stores verification evidence in the event audit trail. DB migration adds two columns to `job_events`.

**Tech Stack:** Rust, tonic/prost (gRPC), SQLite (rusqlite), PowerShell (EventID 307 on Windows)

**Spec:** `docs/superpowers/specs/2026-04-08-print-verification-audit-design.md`

---

## File Structure

### Modified Files
| File | Changes |
|------|---------|
| `proto/devbridge.proto:75-82` | Add `verification_method`, `verification_evidence`, `client_id` fields to `JobCompletion` |
| `crates/devbridge-core/src/job_event.rs:7-19` | Add `Verified` variant to `PrintStage`; add `verification_method` and `verification_evidence` fields to `PrintJobEvent` |
| `crates/devbridge-server/src/storage.rs:87-98` | DB migration: add `verification_method` and `verification_evidence` columns to `job_events` |
| `crates/devbridge-server/src/storage.rs:573-663` | Update `insert_job_event`, `get_job_events`, `get_all_job_events` to handle new columns |
| `crates/devbridge-server/src/dispatch.rs:307-355` | Validate `client_id` against `target_client_id`; store verification evidence in events |
| `crates/devbridge-client/src/receiver.rs:310-322` | Populate `verification_method`, `verification_evidence`, `client_id` in `JobCompletion` |
| `crates/devbridge-client/src/backend_windows_spooler.rs:25-94` | Add EventID 307 verification after spooler submission |
| `crates/devbridge-client/src/backend_direct_ipp.rs:170-216` | Emit `Verified` event with IPP job-state evidence |
| `crates/devbridge-client/src/backend_cups.rs:55-60` | Emit `Verified` event with CUPS evidence |
| `crates/devbridge-client/src/backend_print_proxy.rs:98-104` | Emit `Verified` with `none` method |
| `crates/devbridge-dashboard/src/api/jobs.rs:31-43` | Include `verification_method` and `verification_evidence` in event JSON responses |

---

## Task 1: Extend Proto and Core Types

**Files:**
- Modify: `proto/devbridge.proto:75-82`
- Modify: `crates/devbridge-core/src/job_event.rs:7-19,24-29`

- [ ] **Step 1: Write failing tests for `PrintStage::Verified` serialization**

Add to `crates/devbridge-core/src/job_event.rs` tests section (after line 124):

```rust
    #[test]
    fn test_verified_stage_serde_roundtrip() {
        let json = serde_json::to_string(&PrintStage::Verified).expect("serialize verified");
        assert_eq!(json, "\"verified\"");
        let roundtripped: PrintStage = serde_json::from_str(&json).expect("deserialize verified");
        assert_eq!(roundtripped, PrintStage::Verified);
    }

    #[test]
    fn test_print_job_event_with_verification_fields() {
        let event = PrintJobEvent {
            job_id: "job-v1".into(),
            stage: PrintStage::Verified,
            success: true,
            detail: "EventID 307: Document 42, eholla printer, USB002, 245KB".into(),
            verification_method: "eventid_307".into(),
            verification_evidence: "EventID 307: Document 42, eholla printer, USB002, 245KB".into(),
            timestamp: Utc::now(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let restored: PrintJobEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.verification_method, "eventid_307");
        assert_eq!(restored.verification_evidence, "EventID 307: Document 42, eholla printer, USB002, 245KB");
        assert_eq!(restored.stage, PrintStage::Verified);
    }

    #[test]
    fn test_print_job_event_default_verification_empty() {
        let event = PrintJobEvent::ok("job-old", PrintStage::Completed, "done");
        assert_eq!(event.verification_method, "");
        assert_eq!(event.verification_evidence, "");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p devbridge-core test_verified_stage`
Expected: FAIL — `PrintStage::Verified` variant does not exist, `verification_method` field does not exist.

- [ ] **Step 3: Add `Verified` variant to `PrintStage`**

In `crates/devbridge-core/src/job_event.rs`, add `Verified` between `Sent` and `Acknowledged` (line 15):

```rust
pub enum PrintStage {
    Received,
    Routed,
    Downloading,
    Downloaded,
    Rendering,
    Rendered,
    Sending,
    Sent,
    Verified,
    Acknowledged,
    Completed,
    Failed,
    Retrying,
}
```

- [ ] **Step 4: Add verification fields to `PrintJobEvent`**

In `crates/devbridge-core/src/job_event.rs`, modify the struct (line 24):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrintJobEvent {
    pub job_id: String,
    pub stage: PrintStage,
    pub success: bool,
    pub detail: String,
    #[serde(default)]
    pub verification_method: String,
    #[serde(default)]
    pub verification_evidence: String,
    pub timestamp: DateTime<Utc>,
}
```

Update `PrintJobEvent::new` (line 34) to initialize the new fields:

```rust
    pub fn new(
        job_id: impl Into<String>,
        stage: PrintStage,
        success: bool,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            job_id: job_id.into(),
            stage,
            success,
            detail: detail.into(),
            verification_method: String::new(),
            verification_evidence: String::new(),
            timestamp: Utc::now(),
        }
    }
```

Add a new constructor for verified events:

```rust
    /// Create a verified event with physical delivery evidence.
    pub fn verified(
        job_id: impl Into<String>,
        method: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        let evidence_str = evidence.into();
        Self {
            job_id: job_id.into(),
            stage: PrintStage::Verified,
            success: true,
            detail: evidence_str.clone(),
            verification_method: method.into(),
            verification_evidence: evidence_str,
            timestamp: Utc::now(),
        }
    }
```

- [ ] **Step 5: Update the existing serde roundtrip test to include `Verified`**

In the `test_print_stage_serde_roundtrip` test, add `PrintStage::Verified` to the stages array.

- [ ] **Step 6: Add `emit_verified` to `EventEmitter`**

After the `emit_fail` method (line 89):

```rust
    /// Emit a verified event with physical delivery evidence.
    pub fn emit_verified(
        &self,
        job_id: impl Into<String>,
        method: impl Into<String>,
        evidence: impl Into<String>,
    ) {
        self.emit(PrintJobEvent::verified(job_id, method, evidence));
    }
```

- [ ] **Step 7: Extend proto `JobCompletion`**

In `proto/devbridge.proto`, replace lines 75-82:

```proto
message JobCompletion {
  string job_id = 1;
  bool success = 2;
  string error_detail = 3;
  uint32 pages_printed = 4;
  string printer_status = 5;
  string spooler_status = 6;
  string verification_method = 7;
  string verification_evidence = 8;
  string client_id = 9;
}
```

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p devbridge-core`
Expected: PASS — all tests including new verification tests.

- [ ] **Step 9: Update `print_stage_to_proto_state` in `receiver.rs`**

In `crates/devbridge-client/src/receiver.rs`, add the `Verified` mapping in `print_stage_to_proto_state` (around line 431):

```rust
fn print_stage_to_proto_state(stage: PrintStage) -> i32 {
    match stage {
        PrintStage::Received | PrintStage::Routed => 1,
        PrintStage::Downloading | PrintStage::Downloaded => 2,
        PrintStage::Rendering | PrintStage::Rendered => 7,
        PrintStage::Sending | PrintStage::Sent | PrintStage::Acknowledged | PrintStage::Verified => 8,
        PrintStage::Completed => 4,
        PrintStage::Failed => 5,
        PrintStage::Retrying => 1,
    }
}
```

Update the test `test_print_stage_to_proto_state_mapping` to add:

```rust
        // Verified maps to SENDING = 8
        assert_eq!(print_stage_to_proto_state(PrintStage::Verified), 8);
```

- [ ] **Step 10: Run full workspace tests**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 11: Commit**

```bash
git add proto/devbridge.proto crates/devbridge-core/src/job_event.rs crates/devbridge-client/src/receiver.rs
git commit -m "Add Verified stage, verification fields to PrintJobEvent and JobCompletion proto"
```

---

## Task 2: Database Migration and Storage Layer

**Files:**
- Modify: `crates/devbridge-server/src/storage.rs:87-98,573-663`

- [ ] **Step 1: Write failing test for verification fields in job events**

Add to `crates/devbridge-server/src/storage.rs` tests section:

```rust
    #[test]
    fn test_insert_and_get_event_with_verification_fields() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(&dir.path().join("test.db")).unwrap();

        let event = devbridge_core::job_event::PrintJobEvent {
            job_id: "job-verify-1".into(),
            stage: devbridge_core::job_event::PrintStage::Verified,
            success: true,
            detail: "EventID 307: Document 42, eholla printer, USB002, 245KB".into(),
            verification_method: "eventid_307".into(),
            verification_evidence: "EventID 307: Document 42, eholla printer, USB002, 245KB".into(),
            timestamp: chrono::Utc::now(),
        };

        storage.insert_job_event(&event).unwrap();
        let events = storage.get_job_events("job-verify-1").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].verification_method, "eventid_307");
        assert_eq!(events[0].verification_evidence, "EventID 307: Document 42, eholla printer, USB002, 245KB");
        assert_eq!(events[0].stage, devbridge_core::job_event::PrintStage::Verified);
    }

    #[test]
    fn test_old_events_have_empty_verification_fields() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(&dir.path().join("test.db")).unwrap();

        // Insert event without verification fields (simulates pre-migration data)
        let event = devbridge_core::job_event::PrintJobEvent::ok(
            "job-old-1",
            devbridge_core::job_event::PrintStage::Completed,
            "printed",
        );
        storage.insert_job_event(&event).unwrap();

        let events = storage.get_job_events("job-old-1").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].verification_method, "");
        assert_eq!(events[0].verification_evidence, "");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p devbridge-server test_insert_and_get_event_with_verification`
Expected: FAIL — `verification_method` column does not exist.

- [ ] **Step 3: Add DB migration for verification columns**

In `crates/devbridge-server/src/storage.rs`, after the `job_events` table creation (line 98), add migration:

```rust
        // Migration: add verification columns to job_events
        if conn
            .prepare("SELECT verification_method FROM job_events LIMIT 0")
            .is_err()
        {
            let _ = conn.execute_batch(
                "ALTER TABLE job_events ADD COLUMN verification_method TEXT NOT NULL DEFAULT '';
                 ALTER TABLE job_events ADD COLUMN verification_evidence TEXT NOT NULL DEFAULT '';",
            );
        }
```

- [ ] **Step 4: Update `insert_job_event` to store verification fields**

Replace the `insert_job_event` method (line 573):

```rust
    pub fn insert_job_event(&self, event: &devbridge_core::job_event::PrintJobEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO job_events (job_id, stage, success, detail, verification_method, verification_evidence, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                event.job_id,
                serde_json::to_value(event.stage)
                    .unwrap()
                    .as_str()
                    .unwrap_or("unknown"),
                event.success as i32,
                event.detail,
                event.verification_method,
                event.verification_evidence,
                event.timestamp.to_rfc3339(),
            ],
        )?;
        Ok(())
    }
```

- [ ] **Step 5: Update `get_job_events` to read verification fields**

Replace the `get_job_events` method (line 592):

```rust
    pub fn get_job_events(
        &self,
        job_id: &str,
    ) -> Result<Vec<devbridge_core::job_event::PrintJobEvent>> {
        use devbridge_core::job_event::{PrintJobEvent, PrintStage};

        let mut stmt = self.conn.prepare(
            "SELECT job_id, stage, success, detail, verification_method, verification_evidence, timestamp
             FROM job_events WHERE job_id = ?1 ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map(params![job_id], |row| {
                let stage_str: String = row.get(1)?;
                let stage: PrintStage = serde_json::from_str(&format!("\"{}\"", stage_str))
                    .unwrap_or(PrintStage::Failed);
                let ts_str: String = row.get(6)?;
                let timestamp = DateTime::parse_from_rfc3339(&ts_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());

                Ok(PrintJobEvent {
                    job_id: row.get(0)?,
                    stage,
                    success: row.get::<_, i32>(2)? != 0,
                    detail: row.get(3)?,
                    verification_method: row.get(4)?,
                    verification_evidence: row.get(5)?,
                    timestamp,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(events)
    }
```

- [ ] **Step 6: Update `get_all_job_events` similarly**

Replace the `get_all_job_events` method (line 626):

```rust
    pub fn get_all_job_events(
        &self,
    ) -> Result<std::collections::HashMap<String, Vec<devbridge_core::job_event::PrintJobEvent>>>
    {
        use devbridge_core::job_event::{PrintJobEvent, PrintStage};

        let mut stmt = self.conn.prepare(
            "SELECT job_id, stage, success, detail, verification_method, verification_evidence, timestamp
             FROM job_events ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map([], |row| {
                let stage_str: String = row.get(1)?;
                let stage: PrintStage = serde_json::from_str(&format!("\"{}\"", stage_str))
                    .unwrap_or(PrintStage::Failed);
                let ts_str: String = row.get(6)?;
                let timestamp = DateTime::parse_from_rfc3339(&ts_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());

                Ok(PrintJobEvent {
                    job_id: row.get(0)?,
                    stage,
                    success: row.get::<_, i32>(2)? != 0,
                    detail: row.get(3)?,
                    verification_method: row.get(4)?,
                    verification_evidence: row.get(5)?,
                    timestamp,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut map: std::collections::HashMap<String, Vec<PrintJobEvent>> =
            std::collections::HashMap::new();
        for event in events {
            map.entry(event.job_id.clone()).or_default().push(event);
        }
        Ok(map)
    }
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p devbridge-server`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/devbridge-server/src/storage.rs
git commit -m "Add verification_method and verification_evidence columns to job_events"
```

---

## Task 3: Server-Side Client Validation

**Files:**
- Modify: `crates/devbridge-server/src/dispatch.rs:307-355`

- [ ] **Step 1: Write failing tests for client validation**

Add to `crates/devbridge-server/src/dispatch.rs` tests section:

```rust
    #[tokio::test]
    async fn test_complete_job_stores_verification_evidence() {
        let (_dir, queue, service) = temp_dispatch(3);
        queue
            .push(test_job("job-verified"), "/tmp/verified.pdf".into())
            .unwrap();

        let completion = JobCompletion {
            job_id: "job-verified".into(),
            success: true,
            pages_printed: 1,
            error_detail: String::new(),
            printer_status: "delivered".into(),
            spooler_status: "windows_spooler".into(),
            verification_method: "eventid_307".into(),
            verification_evidence: "EventID 307: Document 42, eholla printer, USB002, 245KB".into(),
            client_id: "holla-client".into(),
        };
        service
            .complete_job(Request::new(completion))
            .await
            .unwrap();

        let job = queue.get_job("job-verified").unwrap().unwrap();
        assert_eq!(job.state, JobState::Completed);

        // Verify the completion event was stored with evidence
        let events = queue.get_job_events("job-verified").unwrap();
        let completed_event = events.iter().find(|e| e.stage == devbridge_core::job_event::PrintStage::Completed);
        assert!(completed_event.is_some(), "should have a Completed event");
        let ev = completed_event.unwrap();
        assert_eq!(ev.verification_method, "eventid_307");
        assert!(ev.verification_evidence.contains("EventID 307"));
    }

    #[tokio::test]
    async fn test_complete_job_client_mismatch_emits_warning() {
        let (_dir, queue, service) = temp_dispatch(3);

        // Create a paired job targeting holla-client
        let mut job = test_job("job-mismatch");
        job.target_client_id = Some("holla-client".into());
        queue
            .push(job, "/tmp/mismatch.pdf".into())
            .unwrap();

        // A different client completes it
        let completion = JobCompletion {
            job_id: "job-mismatch".into(),
            success: true,
            pages_printed: 1,
            error_detail: String::new(),
            printer_status: "delivered".into(),
            spooler_status: "direct_ipp".into(),
            verification_method: "ipp_job_state".into(),
            verification_evidence: "IPP job-state=9 (completed)".into(),
            client_id: "pjpos-client".into(),
        };
        service
            .complete_job(Request::new(completion))
            .await
            .unwrap();

        let events = queue.get_job_events("job-mismatch").unwrap();
        // Should have a client_mismatch warning event
        let mismatch_event = events.iter().find(|e| e.verification_method == "client_mismatch");
        assert!(mismatch_event.is_some(), "should have a client_mismatch warning");
        let ev = mismatch_event.unwrap();
        assert!(ev.detail.contains("pjpos-client"));
        assert!(ev.detail.contains("holla-client"));
    }

    #[tokio::test]
    async fn test_complete_job_unpaired_no_mismatch_warning() {
        let (_dir, queue, service) = temp_dispatch(3);
        // Unpaired job (target_client_id is None)
        queue
            .push(test_job("job-unpaired"), "/tmp/unpaired.pdf".into())
            .unwrap();

        let completion = JobCompletion {
            job_id: "job-unpaired".into(),
            success: true,
            pages_printed: 1,
            error_detail: String::new(),
            printer_status: "delivered".into(),
            spooler_status: "direct_ipp".into(),
            verification_method: "ipp_job_state".into(),
            verification_evidence: "IPP job-state=9 (completed)".into(),
            client_id: "any-client".into(),
        };
        service
            .complete_job(Request::new(completion))
            .await
            .unwrap();

        let events = queue.get_job_events("job-unpaired").unwrap();
        let mismatch = events.iter().any(|e| e.verification_method == "client_mismatch");
        assert!(!mismatch, "unpaired jobs should not get mismatch warnings");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p devbridge-server test_complete_job_stores_verification`
Expected: FAIL — `JobCompletion` struct does not have `verification_method` field yet (proto not regenerated), and `complete_job` does not emit events.

- [ ] **Step 3: Implement client validation and event emission in `complete_job`**

Replace the `complete_job` method in `crates/devbridge-server/src/dispatch.rs` (lines 307-355):

```rust
    async fn complete_job(
        &self,
        request: Request<JobCompletion>,
    ) -> Result<Response<CompletionAck>, Status> {
        let completion = request.into_inner();

        // Check for client mismatch on paired jobs
        if let Ok(Some(job)) = self.queue.get_job(&completion.job_id) {
            if let Some(ref expected_client) = job.target_client_id {
                if !completion.client_id.is_empty() && completion.client_id != *expected_client {
                    info!(
                        job_id = %completion.job_id,
                        expected = %expected_client,
                        actual = %completion.client_id,
                        "job completed by different client than routed"
                    );
                    let warning = devbridge_core::job_event::PrintJobEvent {
                        job_id: completion.job_id.clone(),
                        stage: devbridge_core::job_event::PrintStage::Completed,
                        success: true,
                        detail: format!(
                            "Completed by {} (originally routed to {})",
                            completion.client_id, expected_client
                        ),
                        verification_method: "client_mismatch".into(),
                        verification_evidence: format!(
                            "expected={}, actual={}",
                            expected_client, completion.client_id
                        ),
                        timestamp: chrono::Utc::now(),
                    };
                    let _ = self.queue.insert_job_event(&warning);
                }
            }
        }

        if completion.success {
            info!(
                job_id = %completion.job_id,
                pages = completion.pages_printed,
                verification = %completion.verification_method,
                "job completed successfully"
            );

            // Store completion event with verification evidence
            let event = devbridge_core::job_event::PrintJobEvent {
                job_id: completion.job_id.clone(),
                stage: devbridge_core::job_event::PrintStage::Completed,
                success: true,
                detail: format!(
                    "Completed ({})",
                    if completion.verification_method.is_empty() {
                        "no verification"
                    } else {
                        &completion.verification_method
                    }
                ),
                verification_method: completion.verification_method,
                verification_evidence: completion.verification_evidence,
                timestamp: chrono::Utc::now(),
            };
            let _ = self.queue.insert_job_event(&event);

            self.queue
                .update_state(&completion.job_id, JobState::Completed)
                .map_err(|e| Status::internal(format!("failed to complete job: {e}")))?;
            return Ok(Response::new(CompletionAck {}));
        }

        // Failed: store failure event with evidence
        let event = devbridge_core::job_event::PrintJobEvent {
            job_id: completion.job_id.clone(),
            stage: devbridge_core::job_event::PrintStage::Failed,
            success: false,
            detail: completion.error_detail.clone(),
            verification_method: completion.verification_method,
            verification_evidence: completion.verification_evidence,
            timestamp: chrono::Utc::now(),
        };
        let _ = self.queue.insert_job_event(&event);

        // Check if we should retry
        let should_retry = self
            .queue
            .get_job(&completion.job_id)
            .ok()
            .flatten()
            .is_some_and(|job| job.retry_count < self.max_retries);

        if should_retry {
            info!(
                job_id = %completion.job_id,
                error = %completion.error_detail,
                max_retries = self.max_retries,
                "job failed, requeuing for retry"
            );
            self.queue
                .requeue_job(&completion.job_id, &completion.error_detail)
                .map_err(|e| Status::internal(format!("failed to requeue job: {e}")))?;
        } else {
            info!(
                job_id = %completion.job_id,
                error = %completion.error_detail,
                "job failed permanently (retry limit reached)"
            );
            self.queue
                .update_state(&completion.job_id, JobState::Failed)
                .map_err(|e| Status::internal(format!("failed to mark job failed: {e}")))?;
        }

        Ok(Response::new(CompletionAck {}))
    }
```

- [ ] **Step 4: Update existing dispatch tests to include new proto fields**

All existing `JobCompletion` constructions in tests need the three new fields. Add to each:

```rust
            verification_method: String::new(),
            verification_evidence: String::new(),
            client_id: String::new(),
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p devbridge-server`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/devbridge-server/src/dispatch.rs
git commit -m "Add client validation and verification evidence to job completion"
```

---

## Task 4: Client Completion Report with Verification

**Files:**
- Modify: `crates/devbridge-client/src/receiver.rs:310-322`
- Modify: `crates/devbridge-core/src/job_event.rs` (add `last_verification` to `EventEmitter`)

- [ ] **Step 1: Write failing test for EventEmitter tracking verification state**

Add to `crates/devbridge-core/src/job_event.rs` tests:

```rust
    #[tokio::test]
    async fn test_event_emitter_tracks_last_verification() {
        let (tx, _rx) = tokio::sync::broadcast::channel::<PrintJobEvent>(16);
        let emitter = EventEmitter::new(tx);

        // No verification yet
        let (method, evidence) = emitter.last_verification();
        assert_eq!(method, "");
        assert_eq!(evidence, "");

        // Emit a verified event
        emitter.emit_verified("job-1", "eventid_307", "EventID 307: Document 42");

        let (method, evidence) = emitter.last_verification();
        assert_eq!(method, "eventid_307");
        assert_eq!(evidence, "EventID 307: Document 42");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p devbridge-core test_event_emitter_tracks`
Expected: FAIL — `last_verification` method does not exist.

- [ ] **Step 3: Add verification tracking to `EventEmitter`**

In `crates/devbridge-core/src/job_event.rs`, modify `EventEmitter` to track verification state:

```rust
use std::sync::Mutex;

#[derive(Clone)]
pub struct EventEmitter {
    sender: tokio::sync::broadcast::Sender<PrintJobEvent>,
    verification: Arc<Mutex<(String, String)>>,
}

impl EventEmitter {
    pub fn new(sender: tokio::sync::broadcast::Sender<PrintJobEvent>) -> Self {
        Self {
            sender,
            verification: Arc::new(Mutex::new((String::new(), String::new()))),
        }
    }

    pub fn emit(&self, event: PrintJobEvent) {
        // Track verification evidence
        if event.stage == PrintStage::Verified || !event.verification_method.is_empty() {
            if let Ok(mut v) = self.verification.lock() {
                *v = (
                    event.verification_method.clone(),
                    event.verification_evidence.clone(),
                );
            }
        }
        let _ = self.sender.send(event);
    }

    // ... existing emit_ok, emit_fail methods unchanged ...

    pub fn emit_verified(
        &self,
        job_id: impl Into<String>,
        method: impl Into<String>,
        evidence: impl Into<String>,
    ) {
        self.emit(PrintJobEvent::verified(job_id, method, evidence));
    }

    /// Return the last verification (method, evidence) emitted.
    pub fn last_verification(&self) -> (String, String) {
        self.verification
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PrintJobEvent> {
        self.sender.subscribe()
    }
}
```

Add `use std::sync::Arc;` at the top of the file if not already present.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p devbridge-core test_event_emitter_tracks`
Expected: PASS

- [ ] **Step 5: Update receiver to populate verification in `JobCompletion`**

In `crates/devbridge-client/src/receiver.rs`, replace the success completion block (lines 310-322):

```rust
                    // Get verification evidence from the event emitter
                    let (ver_method, ver_evidence) = event_emitter.last_verification();

                    // Report completion with backend info and verification
                    let completion = JobCompletion {
                        job_id: job.job_id.clone(),
                        success,
                        error_detail,
                        pages_printed: if success { job.copies } else { 0 },
                        printer_status: if success {
                            "delivered".into()
                        } else {
                            "error".into()
                        },
                        spooler_status: self.print_backend.clone(),
                        verification_method: ver_method,
                        verification_evidence: ver_evidence,
                        client_id: self.machine_id.clone(),
                    };
```

Also update the download failure completion block (lines 347-355) to include the new fields:

```rust
                    let completion = JobCompletion {
                        job_id: job.job_id.clone(),
                        success: false,
                        error_detail: e.to_string(),
                        pages_printed: 0,
                        printer_status: String::new(),
                        spooler_status: "download_failed".into(),
                        verification_method: String::new(),
                        verification_evidence: String::new(),
                        client_id: self.machine_id.clone(),
                    };
```

- [ ] **Step 6: Run workspace tests**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/devbridge-core/src/job_event.rs crates/devbridge-client/src/receiver.rs
git commit -m "Track verification evidence in EventEmitter and populate in JobCompletion"
```

---

## Task 5: Direct IPP Backend — Emit Verified Events

**Files:**
- Modify: `crates/devbridge-client/src/backend_direct_ipp.rs:170-216`

- [ ] **Step 1: Replace `Completed` events with `Verified` in `poll_job_completion`**

In `crates/devbridge-client/src/backend_direct_ipp.rs`, modify the `poll_job_completion` method.

On job-state 9 (completed), line 172, change:

```rust
                9 => {
                    let evidence = format!(
                        "IPP job-state=9 (completed), job-id={}",
                        printer_job_id
                    );
                    events.emit_verified(
                        job_id,
                        "ipp_job_state",
                        &evidence,
                    );
                    events.emit_ok(
                        job_id,
                        PrintStage::Completed,
                        format!("{} confirmed printed", display),
                    );
                    return Ok(());
                }
```

On job-state 7/8 (canceled/aborted), line 180, update the `emit_fail` to include verification info:

```rust
                7 | 8 => {
                    let state_name = if job_state == 7 { "canceled" } else { "aborted" };
                    let evidence = format!(
                        "IPP job-state={} ({}), job-id={}, reasons={}",
                        job_state, state_name, printer_job_id, state_reasons
                    );
                    let mut fail_event = PrintJobEvent::fail(
                        job_id,
                        PrintStage::Failed,
                        &evidence,
                    );
                    fail_event.verification_method = "ipp_job_state".into();
                    fail_event.verification_evidence = evidence.clone();
                    events.emit(fail_event);
                    anyhow::bail!("{}", evidence);
                }
```

On timeout, line 194, change the `emit_ok` (which was wrong — marking as completed on timeout) to `emit_fail`:

```rust
                _ => {
                    if std::time::Instant::now() > deadline {
                        let evidence = format!(
                            "IPP job-state polling timeout after 60s, job-id={}, last state={}",
                            printer_job_id, job_state
                        );
                        warn!("{}", evidence);
                        let mut fail_event = PrintJobEvent::fail(
                            job_id,
                            PrintStage::Failed,
                            &evidence,
                        );
                        fail_event.verification_method = "ipp_job_state".into();
                        fail_event.verification_evidence = evidence.clone();
                        events.emit(fail_event);
                        anyhow::bail!("{}", evidence);
                    }
                }
```

**Important:** The previous code accepted timeout as success (`emit_ok` + `return Ok(())`). The spec says verification failure = retry. This is the correct behavioral change — if we can't confirm the printer completed, we fail and retry.

- [ ] **Step 2: Add import for `PrintJobEvent`**

At the top of `backend_direct_ipp.rs`, add:

```rust
use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};
```

(Replace the existing `use devbridge_core::job_event::{EventEmitter, PrintStage};`)

- [ ] **Step 3: Run tests**

Run: `cargo test -p devbridge-client`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-client/src/backend_direct_ipp.rs
git commit -m "Emit Verified events with IPP job-state evidence in direct_ipp backend"
```

---

## Task 6: Windows Spooler Backend — EventID 307 Verification

**Files:**
- Modify: `crates/devbridge-client/src/backend_windows_spooler.rs`

- [ ] **Step 1: Replace spooler-only verification with EventID 307**

Replace the entire `print` method in `crates/devbridge-client/src/backend_windows_spooler.rs`:

```rust
impl PrintBackend for WindowsSpooler {
    fn name(&self) -> &str {
        "windows_spooler"
    }

    fn print(&self, job: &PrintJobInfo, pdf_path: &Path, events: &EventEmitter) -> Result<()> {
        let printer = &job.printer_name;
        let display = job.printer_display_name.as_deref().unwrap_or(printer);

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!("Windows spooler → {}", display),
        );

        if let Err(e) = crate::printer::check_printer_ready(printer) {
            warn!(printer, error = %e, "printer readiness check failed, attempting print anyway");
        }

        crate::printer::print_pdf(printer, pdf_path)?;

        events.emit_ok(
            &job.job_id,
            PrintStage::Sent,
            format!("Submitted to Windows spooler for {}", display),
        );

        let is_virtual = printer.to_lowercase().contains("pdf")
            || printer.to_lowercase().contains("xps")
            || printer.to_lowercase().contains("onenote")
            || printer.to_lowercase().contains("fax");

        if is_virtual {
            events.emit_verified(
                &job.job_id,
                "virtual_printer",
                format!("Virtual printer {} — no physical delivery", display),
            );
            events.emit_ok(
                &job.job_id,
                PrintStage::Completed,
                format!("Virtual printer {}", display),
            );
            return Ok(());
        }

        // Physical printer: verify with EventID 307
        self.verify_eventid_307(printer, display, &job.job_id, events)
    }
}
```

- [ ] **Step 2: Add `verify_eventid_307` method**

Add a new method to `impl WindowsSpooler`:

```rust
impl WindowsSpooler {
    pub fn new(target_printer: String) -> Self {
        Self { target_printer }
    }

    /// Verify physical delivery via Windows Print Service EventID 307.
    /// Only runs on Windows; on other platforms, falls back to spooler verification.
    #[cfg(target_os = "windows")]
    fn verify_eventid_307(
        &self,
        printer: &str,
        display: &str,
        job_id: &str,
        events: &EventEmitter,
    ) -> Result<()> {
        use std::process::Command;
        use std::time::{Duration, Instant};

        // Ensure the PrintService Operational log is enabled (idempotent)
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command",
                "wevtutil sl 'Microsoft-Windows-PrintService/Operational' /e:true"])
            .output();

        let deadline = Instant::now() + Duration::from_secs(60);
        let poll_interval = Duration::from_secs(2);

        // Record the current time to only match events after our print submission
        let start_time = chrono::Utc::now();

        loop {
            let ps_script = format!(
                r#"Get-WinEvent -LogName 'Microsoft-Windows-PrintService/Operational' -MaxEvents 20 -ErrorAction SilentlyContinue | Where-Object {{ $_.Id -eq 307 -and $_.TimeCreated -ge '{start}' -and $_.Message -match '{printer}' }} | Select-Object -First 1 -ExpandProperty Message"#,
                start = start_time.format("%Y-%m-%dT%H:%M:%S"),
                printer = printer.replace('\'', "''"),
            );

            let output = Command::new("powershell")
                .args(["-NoProfile", "-Command", &ps_script])
                .output()?;

            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if !stdout.is_empty() && stdout.contains("307") || stdout.contains(printer) {
                // EventID 307 found
                let evidence = format!("EventID 307: {}", stdout.chars().take(200).collect::<String>());
                tracing::info!(job_id, printer, "physical delivery confirmed via EventID 307");
                events.emit_verified(job_id, "eventid_307", &evidence);
                events.emit_ok(
                    job_id,
                    PrintStage::Completed,
                    format!("Printed on {} (EventID 307 confirmed)", display),
                );
                return Ok(());
            }

            if Instant::now() > deadline {
                // Check for error events before failing
                let error_ps = format!(
                    r#"Get-WinEvent -LogName 'Microsoft-Windows-PrintService/Operational' -MaxEvents 10 -ErrorAction SilentlyContinue | Where-Object {{ ($_.Id -eq 372 -or $_.Id -eq 842) -and $_.TimeCreated -ge '{start}' -and $_.Message -match '{printer}' }} | Select-Object -First 1 -ExpandProperty Message"#,
                    start = start_time.format("%Y-%m-%dT%H:%M:%S"),
                    printer = printer.replace('\'', "''"),
                );
                let error_output = Command::new("powershell")
                    .args(["-NoProfile", "-Command", &error_ps])
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();

                let error_detail = if error_output.is_empty() {
                    format!("No EventID 307 within 60s for {}", printer)
                } else {
                    format!("No EventID 307 within 60s for {}. Error: {}", printer,
                        error_output.chars().take(200).collect::<String>())
                };

                let mut fail_event = devbridge_core::job_event::PrintJobEvent::fail(
                    job_id, PrintStage::Failed, &error_detail,
                );
                fail_event.verification_method = "eventid_307".into();
                fail_event.verification_evidence = error_detail.clone();
                events.emit(fail_event);
                anyhow::bail!("{}", error_detail);
            }

            std::thread::sleep(poll_interval);
        }
    }

    /// Non-Windows fallback: use spooler queue verification.
    #[cfg(not(target_os = "windows"))]
    fn verify_eventid_307(
        &self,
        printer: &str,
        display: &str,
        job_id: &str,
        events: &EventEmitter,
    ) -> Result<()> {
        let verification = crate::printer::verify_print_completion(printer, 60)?;
        if verification.success {
            events.emit_verified(
                job_id,
                "spooler_queue",
                format!("Spooler queue cleared for {} (non-Windows fallback)", display),
            );
            events.emit_ok(
                job_id,
                PrintStage::Completed,
                format!("Printed via spooler on {}", display),
            );
            Ok(())
        } else {
            let error_detail = format!(
                "spooler {}: {} (printer: {})",
                verification.spooler_status, verification.detail, printer
            );
            let mut fail_event = devbridge_core::job_event::PrintJobEvent::fail(
                job_id, PrintStage::Failed, &error_detail,
            );
            fail_event.verification_method = "spooler_queue".into();
            fail_event.verification_evidence = error_detail.clone();
            events.emit(fail_event);
            anyhow::bail!("{}", error_detail);
        }
    }
}
```

- [ ] **Step 3: Add needed imports**

At the top of `backend_windows_spooler.rs`:

```rust
use std::path::Path;

use anyhow::Result;
use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};
use tracing::warn;

use crate::print_backend::{PrintBackend, PrintJobInfo};
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p devbridge-client`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-client/src/backend_windows_spooler.rs
git commit -m "Add EventID 307 physical delivery verification to windows_spooler backend"
```

---

## Task 7: CUPS and Print Proxy Backends — Emit Verified Events

**Files:**
- Modify: `crates/devbridge-client/src/backend_cups.rs:55-60`
- Modify: `crates/devbridge-client/src/backend_print_proxy.rs:98-104`

- [ ] **Step 1: Update CUPS backend to emit Verified events**

In `crates/devbridge-client/src/backend_cups.rs`, replace the verification block (lines 53-79):

```rust
        let verification = crate::printer::verify_print_completion(printer, 180)?;

        if verification.success {
            events.emit_verified(
                &job.job_id,
                "cups_lpstat",
                format!("CUPS job completed on {}", display),
            );
            events.emit_ok(
                &job.job_id,
                PrintStage::Completed,
                format!("Printed via CUPS on {}", display),
            );
        } else {
            let evidence = format!(
                "CUPS spooler {}: {} (printer: {})",
                verification.spooler_status, verification.detail, printer
            );
            let mut fail_event = devbridge_core::job_event::PrintJobEvent::fail(
                &job.job_id,
                PrintStage::Failed,
                &evidence,
            );
            fail_event.verification_method = "cups_lpstat".into();
            fail_event.verification_evidence = evidence.clone();
            events.emit(fail_event);
            anyhow::bail!("{}", evidence);
        }
```

Add import at top: `use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};`
(Replace the existing import.)

- [ ] **Step 2: Update print proxy backend to emit Verified with "none"**

In `crates/devbridge-client/src/backend_print_proxy.rs`, replace the success block (line 98-104):

```rust
        if http_code == 200 {
            info!(proxy_url = %self.proxy_url, "print proxy accepted job");
            events.emit_verified(
                &job.job_id,
                "none",
                format!("Proxied to {} — no local verification", self.proxy_url),
            );
            events.emit_ok(
                &job.job_id,
                PrintStage::Completed,
                format!("Printed via proxy for {}", display),
            );
            Ok(())
```

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-client/src/backend_cups.rs crates/devbridge-client/src/backend_print_proxy.rs
git commit -m "Emit Verified events in CUPS and print_proxy backends"
```

---

## Task 8: Dashboard API — Include Verification Fields

**Files:**
- Modify: `crates/devbridge-dashboard/src/api/jobs.rs:31-43,57-66`

- [ ] **Step 1: Write failing test for verification fields in API response**

Add to `crates/devbridge-dashboard/src/api/jobs.rs` tests:

```rust
    #[tokio::test]
    async fn test_job_events_include_verification_fields() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = devbridge_server::JobQueue::new(storage).unwrap();

        // Insert a verified event
        let event = devbridge_core::job_event::PrintJobEvent {
            job_id: "job-api-1".into(),
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
        assert!(ev["verification_evidence"].as_str().unwrap().contains("EventID 307"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p devbridge-dashboard test_job_events_include_verification`
Expected: FAIL — `verification_method` not in JSON response.

- [ ] **Step 3: Add verification fields to event JSON serialization**

In `crates/devbridge-dashboard/src/api/jobs.rs`, update the `get_job_events` function (line 31):

```rust
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
```

Update the same in `get_all_events_batch` (line 57):

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p devbridge-dashboard`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-dashboard/src/api/jobs.rs
git commit -m "Include verification_method and verification_evidence in event API responses"
```

---

## Task 9: Update Mutation Testing Exclusions

**Files:**
- Modify: `.cargo/mutants.toml`

- [ ] **Step 1: Add EventID 307 Windows-only code to exclusions**

The `verify_eventid_307` Windows implementation cannot be tested on CI (ubuntu). Add to `.cargo/mutants.toml`:

```toml
# EventID 307 — Windows-only PowerShell code, not exercisable on ubuntu CI
"verify_eventid_307.*target_os.*windows"
```

Verify the exact regex matches by reading the current exclusions and appending. The pattern should match the `#[cfg(target_os = "windows")]` variant of the function.

- [ ] **Step 2: Commit**

```bash
git add .cargo/mutants.toml
git commit -m "Exclude Windows-only EventID 307 verification from mutation testing"
```

---

## Task 10: Format Check, Push, and Monitor CI

**Files:** None (CI verification)

- [ ] **Step 1: Run format check**

```bash
cargo fmt --all --check
```

Fix any formatting issues with `cargo fmt --all`.

- [ ] **Step 2: Push to dev**

```bash
git push origin dev
```

- [ ] **Step 3: Monitor CI**

```bash
gh run list --branch dev --limit 3
```

Wait for all jobs to complete. Use `gh run view <run-id>` to monitor. If any job fails, download logs with `gh run view <run-id> --log-failed`, fix, and push again.

- [ ] **Step 4: Verify all Tier 1 jobs pass**

All jobs including Mutation Testing and Playwright E2E must be green.

---

## Task 11: Create PR

- [ ] **Step 1: Create PR from dev to main**

```bash
gh pr create --title "Add physical delivery verification to all print backends" --body "$(cat <<'EOF'
## Summary
- Add `Verified` stage to print audit trail with machine-verifiable evidence
- Windows spooler: verify via EventID 307 (physical USB/network delivery proof)
- Direct IPP: emit `Verified` with IPP job-state=9 confirmation
- CUPS: emit `Verified` with lpstat completion evidence
- Server validates completing client matches paired client (mismatch warning)
- DB migration adds `verification_method` and `verification_evidence` to job_events
- Proto `JobCompletion` gains `verification_method`, `verification_evidence`, `client_id`
- IPP timeout now fails + retries instead of silently marking "completed"

Closes #19, closes #20

## Test plan
- [ ] `PrintStage::Verified` serialization roundtrip
- [ ] Verification fields round-trip through DB
- [ ] Server client mismatch detection (paired job, wrong client)
- [ ] Server no-mismatch for unpaired jobs
- [ ] Completion event stores verification evidence
- [ ] EventEmitter tracks last verification
- [ ] Dashboard API returns verification fields
- [ ] All existing tests still pass
- [ ] Mutation testing: zero survivors
- [ ] Playwright E2E: all pages pass

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 2: Monitor PR CI until all jobs pass**

- [ ] **Step 3: Verify PR is mergeable**

```bash
gh api repos/zbynekdrlik/devbridge/pulls/<PR_NUMBER> --jq '{mergeable: .mergeable, mergeable_state: .mergeable_state}'
```

- [ ] **Step 4: Report PR URL and wait for user merge approval**

---

## Verification

After merge and main CI:

1. **Proto regeneration:** `JobCompletion` has `verification_method`, `verification_evidence`, `client_id` fields
2. **DB migration:** `job_events` table has the two new columns, existing events have empty strings
3. **Backend verification:** Each backend emits `Verified` stage events with appropriate method/evidence
4. **Client validation:** Paired job completed by wrong client gets `client_mismatch` warning event
5. **Dashboard API:** `/api/jobs/{id}/events` returns `verification_method` and `verification_evidence`
6. **IPP timeout:** Now fails and retries instead of silently accepting timeout as success
7. **All CI jobs green** including mutation testing and Playwright E2E
