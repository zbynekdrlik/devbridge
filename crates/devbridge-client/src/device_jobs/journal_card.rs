//! Card payments of the device journal (#102 lanes D2 + D3) — table
//! `card_payments`, one row per `card.purchase` / `card.credit` /
//! `card.reversal` that reached its terminal (`tx_type` = the
//! `ta_e_transaction_type_t` it sent; a D2 journal is migrated with 1 =
//! purchase, the only type D2 sent).
//!
//! The generic row (`device_jobs`) stays `executing` while the payment is
//! authorised and waits for its commit / rollback (the purchase's execute
//! holds the terminal session), so restart recovery is unchanged: the row
//! becomes `indeterminate` and the TIM executor reconciles it with what is
//! stored here — the terminal's last `trans_seq` BEFORE the transaction
//! (`baseline_seq`), the amount, when the terminal was asked / authorised /
//! the execute ended, and the authorisation itself.
//!
//! `auth_ack` is the `phase: "authorized"` ack: 0 none, 1 owed, 2 accepted,
//! 3 refused (`too_late` — the payment is rolled back at once). It is owed
//! only while the generic row is still `executing`: a payment decided
//! without it (Odoo unreachable) is acked with its final outcome alone.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::{Journal, now};
use crate::device_jobs::api::AckRequest;

/// `auth_ack` values.
const AUTH_OWED: i64 = 1;
const AUTH_ACCEPTED: i64 = 2;
const AUTH_REFUSED: i64 = 3;

pub(super) fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS card_payments (
             job_id         INTEGER PRIMARY KEY,
             terminal_ip    TEXT NOT NULL,
             amount_minor   INTEGER NOT NULL,
             baseline_seq   INTEGER,
             contact_at     TEXT NOT NULL,
             authorized_at  TEXT,
             trans_seq      INTEGER,
             auth_result    TEXT,
             auth_ack       INTEGER NOT NULL DEFAULT 0,
             auth_ack_note  TEXT,
             ended_at       TEXT,
             person_reason  TEXT,
             tx_type        INTEGER NOT NULL DEFAULT 1,
             target_seq     INTEGER
         );",
    )
    .context("create card_payments table")?;
    // a D2 journal: every row was a purchase, none targets another
    for (column, ddl) in [
        ("tx_type", "tx_type INTEGER NOT NULL DEFAULT 1"),
        ("target_seq", "target_seq INTEGER"),
    ] {
        let has = conn
            .prepare("SELECT 1 FROM pragma_table_info('card_payments') WHERE name = ?1")?
            .exists([column])?;
        if !has {
            conn.execute_batch(&format!("ALTER TABLE card_payments ADD COLUMN {ddl};"))
                .with_context(|| format!("add {column} to card_payments"))?;
        }
    }
    Ok(())
}

/// One `card_payments` row.
#[derive(Debug, Clone, PartialEq)]
pub struct CardPayment {
    pub job_id: i64,
    pub terminal_ip: String,
    pub amount_minor: i64,
    /// The terminal's last `trans_seq` before this payment (`None` = unknown).
    pub baseline_seq: Option<i64>,
    pub contact_at: String,
    pub authorized_at: Option<String>,
    pub trans_seq: Option<i64>,
    /// The `phase: "authorized"` result (card_result `authorized`).
    pub auth_result: Option<Value>,
    pub auth_refused: bool,
    /// When the purchase's execute ended without knowing the outcome.
    pub ended_at: Option<String>,
    /// Set = the terminal cannot prove the outcome: a person decides, the
    /// terminal is not asked again.
    pub person_reason: Option<String>,
    /// `ta_e_transaction_type_t` sent (1 purchase, 2 credit, 3 reversal).
    pub tx_type: i32,
    /// The `trans_seq` a reversal reverses (`None` for a payment).
    pub target_seq: Option<i64>,
}

const CARD_COLUMNS: &str = "job_id, terminal_ip, amount_minor, baseline_seq, contact_at, \
     authorized_at, trans_seq, auth_result, auth_ack, ended_at, person_reason, tx_type, \
     target_seq";

fn row_to_card(row: &rusqlite::Row<'_>) -> rusqlite::Result<CardPayment> {
    let result: Option<String> = row.get(7)?;
    Ok(CardPayment {
        job_id: row.get(0)?,
        terminal_ip: row.get(1)?,
        amount_minor: row.get(2)?,
        baseline_seq: row.get(3)?,
        contact_at: row.get(4)?,
        authorized_at: row.get(5)?,
        trans_seq: row.get(6)?,
        auth_result: result.and_then(|t| serde_json::from_str(&t).ok()),
        auth_refused: row.get::<_, i64>(8)? == AUTH_REFUSED,
        ended_at: row.get(9)?,
        person_reason: row.get(10)?,
        tx_type: row.get(11)?,
        target_seq: row.get(12)?,
    })
}

impl Journal {
    /// The purchase is about to be sent to its terminal: store the card row
    /// (terminal, amount, baseline) AND `device_contact_at`, in ONE
    /// committed transaction. `false` = the job is not journaled or its
    /// terminal was already asked — it must NOT be transacted (again).
    pub fn card_contact(
        &self,
        job_id: i64,
        terminal_ip: &str,
        amount_minor: i64,
        baseline_seq: Option<i64>,
    ) -> Result<bool> {
        self.card_contact_of(job_id, terminal_ip, amount_minor, baseline_seq, 1, None)
    }

    /// [`Self::card_contact`] of a transaction of type `tx_type`.
    pub fn card_contact_of(
        &self,
        job_id: i64,
        terminal_ip: &str,
        amount_minor: i64,
        baseline_seq: Option<i64>,
        tx_type: i32,
        target_seq: Option<i64>,
    ) -> Result<bool> {
        let ts = now();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let marked = tx.execute(
            "UPDATE device_jobs SET device_contact_at = ?2, updated_at = ?2
             WHERE job_id = ?1 AND device_contact_at IS NULL AND state = 'executing'",
            params![job_id, ts],
        )?;
        if marked != 1 {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO card_payments (job_id, terminal_ip, amount_minor, baseline_seq, contact_at,
                 tx_type, target_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![job_id, terminal_ip, amount_minor, baseline_seq, ts, tx_type, target_seq],
        )?;
        tx.commit()
            .with_context(|| format!("journal: card contact of job {job_id}"))?;
        Ok(true)
    }

    /// The terminal authorised the payment: store it and owe the
    /// `phase: "authorized"` ack (wakes the acker). `false` = no card row.
    pub fn card_authorized(&self, job_id: i64, trans_seq: i64, result: &Value) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE card_payments SET authorized_at = ?2, trans_seq = ?3, auth_result = ?4,
                     auth_ack = ?5
                 WHERE job_id = ?1 AND authorized_at IS NULL",
                params![job_id, now(), trans_seq, result.to_string(), AUTH_OWED],
            )
            .with_context(|| format!("journal: card authorisation of job {job_id}"))?;
        self.card_events.notify_one();
        Ok(changed == 1)
    }

    /// A one-phase transaction (a reversal) answered with `trans_seq`: stored
    /// for the baseline of later payments and for `card.print_copy` — no
    /// authorised ack is owed. `false` = no card row.
    pub fn card_transacted(&self, job_id: i64, trans_seq: i64) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE card_payments SET trans_seq = ?2 WHERE job_id = ?1 AND trans_seq IS NULL",
                params![job_id, trans_seq],
            )
            .with_context(|| format!("journal: card transaction of job {job_id}"))?;
        Ok(changed == 1)
    }

    /// Another reversal of `target_seq` on `terminal_ip` (not `job_id`)
    /// whose job did not fail: its id and state. A TransSeq is reversed at
    /// most once — never sent again while one is done or unresolved.
    pub fn card_other_reversal(
        &self,
        terminal_ip: &str,
        target_seq: i64,
        job_id: i64,
    ) -> Result<Option<(i64, super::JobState)>> {
        let found: Option<(i64, String)> = self
            .conn()
            .query_row(
                "SELECT c.job_id, d.state FROM card_payments c JOIN device_jobs d ON d.job_id = c.job_id
                 WHERE c.terminal_ip = ?1 AND c.tx_type = 3 AND c.target_seq = ?2
                   AND c.job_id <> ?3 AND d.state <> 'failed'
                 ORDER BY c.job_id LIMIT 1",
                params![terminal_ip, target_seq, job_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .context("journal: other reversal of a trans_seq")?;
        Ok(found.map(|(id, state)| {
            (
                id,
                super::JobState::parse(&state).unwrap_or(super::JobState::Indeterminate),
            )
        }))
    }

    /// Whether a balance ended `done` on `terminal_ip` after this bridge
    /// sent the payment with `trans_seq` there (the NEWEST such payment: a
    /// reset sequence never lets an old one prove it) — only then a terminal that
    /// cannot reverse it is proven to have closed its period.
    pub fn card_balance_since_payment(&self, terminal_ip: &str, trans_seq: i64) -> Result<bool> {
        self.conn()
            .prepare(
                "SELECT 1 FROM card_payments p, card_balances b
                 WHERE p.job_id = (SELECT MAX(job_id) FROM card_payments
                                   WHERE terminal_ip = ?1 AND trans_seq = ?2
                                     AND tx_type IN (1, 2))
                   AND b.terminal_ip = p.terminal_ip AND b.outcome = 'done'
                   AND b.contact_at >= p.contact_at",
            )?
            .exists(params![terminal_ip, trans_seq])
            .context("journal: balance since a payment")
    }

    /// The purchase's execute ended without knowing the outcome (now).
    pub fn card_ended(&self, job_id: i64) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE card_payments SET ended_at = ?2 WHERE job_id = ?1",
                params![job_id, now()],
            )
            .with_context(|| format!("journal: card end of job {job_id}"))?;
        Ok(changed == 1)
    }

    /// The terminal cannot prove the outcome: leave it for a person (the
    /// first reason is kept).
    pub fn card_for_person(&self, job_id: i64, reason: &str) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE card_payments SET person_reason = ?2
                 WHERE job_id = ?1 AND person_reason IS NULL",
                params![job_id, reason],
            )
            .with_context(|| format!("journal: card job {job_id} for a person"))?;
        Ok(changed == 1)
    }

    /// The card row of `job_id`.
    pub fn card_payment(&self, job_id: i64) -> Result<Option<CardPayment>> {
        self.conn()
            .query_row(
                &format!("SELECT {CARD_COLUMNS} FROM card_payments WHERE job_id = ?1"),
                params![job_id],
                row_to_card,
            )
            .optional()
            .with_context(|| format!("journal: read card job {job_id}"))
    }

    /// The newest Odoo card job a TILL submits (purchase / credit / reversal /
    /// balance / print copy — never the Odoo-composed commit / rollback /
    /// shift jobs) of till `kasa_id` with `client_uuid`, and its card row
    /// (`GET /v1/card-jobs/…` of the LAN API, #102 D3).
    pub fn card_job_by_uuid(
        &self,
        kasa_id: i64,
        client_uuid: &str,
    ) -> Result<Option<(super::JournalEntry, Option<CardPayment>)>> {
        let job_id: Option<i64> = self
            .conn()
            .query_row(
                "SELECT job_id FROM device_jobs
                 WHERE kasa_id = ?1 AND client_uuid = ?2 AND job_type LIKE 'card.%'
                   AND job_type NOT IN ('card.commit', 'card.rollback', 'card.activate',
                                        'card.deactivate')
                 ORDER BY job_id DESC LIMIT 1",
                params![kasa_id, client_uuid],
                |r| r.get(0),
            )
            .optional()
            .with_context(|| format!("journal: card job (till {kasa_id}, {client_uuid})"))?;
        let Some(job_id) = job_id else {
            return Ok(None);
        };
        let Some(entry) = self.get(job_id)? else {
            return Ok(None);
        };
        Ok(Some((entry, self.card_payment(job_id)?)))
    }

    /// The highest authorised `trans_seq` stored for a terminal.
    pub fn card_last_seq(&self, terminal_ip: &str) -> Result<Option<i64>> {
        self.conn()
            .query_row(
                "SELECT MAX(trans_seq) FROM card_payments WHERE terminal_ip = ?1",
                params![terminal_ip],
                |r| r.get(0),
            )
            .context("journal: last card trans_seq")
    }

    /// Whether another payment was sent to the same terminal after this
    /// one (its card row came later).
    /// An unreadable time counts as later (a person decides).
    pub fn card_later_on_terminal(&self, job_id: i64) -> Result<bool> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT o.contact_at, c.contact_at FROM card_payments o, card_payments c
             WHERE c.job_id = ?1 AND o.terminal_ip = c.terminal_ip AND o.job_id <> c.job_id",
        )?;
        let pairs = stmt
            .query_map(params![job_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("journal: later payment than job {job_id}"))?;
        let at = |t: &str| chrono::DateTime::parse_from_rfc3339(t).ok();
        Ok(pairs.iter().any(|(other, own)| match (at(other), at(own)) {
            (Some(other), Some(own)) => other >= own,
            _ => true,
        }))
    }

    /// Owed `phase: "authorized"` acks — only of payments still held
    /// (generic row `executing`), oldest first.
    pub(super) fn owed_card_auth_acks(&self) -> Result<Vec<AckRequest>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT c.job_id, c.auth_result FROM card_payments c
             JOIN device_jobs d ON d.job_id = c.job_id
             WHERE c.auth_ack = ?1 AND d.state = 'executing' AND c.auth_result IS NOT NULL
             ORDER BY c.job_id",
        )?;
        let rows = stmt
            .query_map(params![AUTH_OWED], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|(job_id, text)| AckRequest::Authorized {
                job_id,
                result: serde_json::from_str(&text).unwrap_or(Value::Null),
            })
            .collect())
    }

    /// Odoo answered the `phase: "authorized"` ack (`note`: `ok`,
    /// `duplicate` or `refused: …` — a refusal rolls the payment back).
    pub fn mark_card_auth_acked(&self, job_id: i64, note: &str) -> Result<bool> {
        let state = if note.starts_with("refused:") {
            AUTH_REFUSED
        } else {
            AUTH_ACCEPTED
        };
        let changed = self
            .conn()
            .execute(
                "UPDATE card_payments SET auth_ack = ?2, auth_ack_note = ?3
                 WHERE job_id = ?1 AND auth_ack = ?4",
                params![job_id, state, note, AUTH_OWED],
            )
            .with_context(|| format!("journal: card auth ack of job {job_id}"))?;
        Ok(changed == 1)
    }

    /// Test helper: move a card row's moments `secs` into the past.
    #[cfg(test)]
    pub fn card_backdate(&self, job_id: i64, secs: i64) {
        let at = (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339();
        self.conn()
            .execute(
                "UPDATE card_payments SET contact_at = ?2,
                     authorized_at = CASE WHEN authorized_at IS NULL THEN NULL ELSE ?2 END,
                     ended_at = CASE WHEN ended_at IS NULL THEN NULL ELSE ?2 END
                 WHERE job_id = ?1",
                params![job_id, at],
            )
            .unwrap();
    }

    /// Card rows whose job row was pruned.
    pub(super) fn prune_card_payments(conn: &Connection) -> Result<usize> {
        Ok(conn.execute(
            "DELETE FROM card_payments
             WHERE job_id NOT IN (SELECT job_id FROM device_jobs)",
            [],
        )?)
    }
}

#[cfg(test)]
#[path = "journal_card_tests.rs"]
mod tests;
