//! `fiscal.paragon` (#115) against a REAL Portos eKasa API 7.0.4 —
//! InMemory storage + Playground ONLY (CI job `portos-live`), never a
//! store's Portos. A paragón is a hand-written paper receipt of a sale made
//! while the eKasa register could not work; the store registers it later
//! (zákon 384/2025 Z.z. § 11 ods. 3) with when it was written and its
//! number.
//!
//! What 7.0.4 answers for the open facts of #115 (a paragón written more
//! than 10 days ago, a number above int32) is PRINTED — the CI log is the
//! evidence recorded on the ticket — and checked for truthfulness: whatever
//! Portos decides, devbridge's outcome must match what Portos stored.

use std::time::Duration;

use chrono::{FixedOffset, Utc};
use serde_json::{Value, json};

use devbridge_client::device_jobs::executor::{DeviceExecutor, Outcome};
use devbridge_client::device_jobs::portos::outcome::{Lookup, classify_lookup};
use devbridge_client::device_jobs::portos::payload::external_id;

use super::portos_live::{
    done, executor, job, next_number, orp, portos_url, raw_lookup, scratch_journal, settle, unique,
};

/// The paragón's `issueDate`: `hours` hours ago, written with the Slovak
/// winter offset (+01:00), as Odoo writes a local time.
fn written_ago(hours: i64) -> String {
    let cet = FixedOffset::east_opt(3600).unwrap();
    (Utc::now() - chrono::Duration::hours(hours))
        .with_timezone(&cet)
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// A paragón number that differs between runs on the same InMemory Portos.
fn paragon_number() -> u64 {
    (Utc::now().timestamp_millis().unsigned_abs() % 1_000_000) + 1
}

fn paragon(orp: &str, issue_date: &str, number: u64) -> Value {
    json!({
        "data": {
            "cashRegisterCode": orp,
            "issueDate": issue_date,
            "paragonNumber": number,
            "items": [
                {"type": "Positive", "name": "Chlieb kváskový 500 g", "quantity": {"amount": 2, "unit": "ks"},
                 "unitPrice": 1.29, "price": 2.58, "vatRate": 19.0}
            ],
            "payments": [{"name": "Hotovosť", "amount": 2.6}],
            "roundingAmount": 0.02,
            "headerText": "devbridge CI #115 - InMemory/Playground"
        }
    })
}

/// A valid paragón is registered once; the same operation again (a new job,
/// the same `client_uuid`) is answered from the externalId lookup.
#[tokio::test]
async fn live_paragon_is_registered_exactly_once() {
    let orp = orp();
    let e = executor(&scratch_journal(), &portos_url(), Duration::from_secs(60));
    let uuid = unique("paragon");
    let number = paragon_number();
    let written = written_ago(2);
    let payload = paragon(&orp, &written, number);
    let first = settle(&e, &job(61, "fiscal.paragon", &uuid, payload.clone())).await;
    println!("paragon written {written} registered: {first}");
    assert_eq!(first["receipt_type"], "Paragon", "{first}");
    assert_eq!(first["paragon_number"], number, "{first}");
    assert_eq!(first["external_id"], external_id(3, &uuid));
    assert!(
        first["okp"].as_str().is_some_and(|o| !o.is_empty()),
        "{first}"
    );
    assert!(first["receipt_number"].is_number(), "{first}");
    let after_first = next_number(&orp).await;

    let again = done(e.execute(&job(62, "fiscal.paragon", &uuid, payload)).await);
    println!("the same paragon again: {again}");
    assert_eq!(again["via"], "lookup");
    assert_eq!(again["receipt_number"], first["receipt_number"]);
    assert_eq!(again["paragon_number"], number);
    assert_eq!(
        next_number(&orp).await,
        after_first,
        "the retry registered nothing"
    );
}

/// The open facts of #115 — Portos' own rules for a paragón written more
/// than 10 days ago and for a number above int32 (the 7.0.4 swagger says
/// `int32`, the vendor client 1 … 4 294 967 295): `done` only when Portos
/// stored it, `failed portos_validation` only when it did not.
#[tokio::test]
async fn live_paragon_portos_rules_are_reported_truthfully() {
    let orp = orp();
    let e = executor(&scratch_journal(), &portos_url(), Duration::from_secs(60));
    for (fact, id, issue_date, number) in [
        (
            "issueDate 15 days ago",
            63,
            written_ago(15 * 24),
            paragon_number(),
        ),
        (
            "paragonNumber 3000000000 (above int32)",
            64,
            written_ago(1),
            3_000_000_000,
        ),
    ] {
        let uuid = unique("pfact");
        let outcome = e
            .execute(&job(
                id,
                "fiscal.paragon",
                &uuid,
                paragon(&orp, &issue_date, number),
            ))
            .await;
        println!("#115 fact - {fact}: {outcome:?}");
        let stored = classify_lookup(raw_lookup(&orp, &external_id(3, &uuid)).await);
        println!("#115 fact - {fact}: Portos lookup {stored:?}");
        match outcome {
            Outcome::Done(result) => {
                assert!(matches!(stored, Lookup::Found(_)), "{fact}: {stored:?}");
                assert_eq!(result["paragon_number"], number, "{fact}");
            }
            Outcome::Failed { code, text } => {
                assert_eq!(code, "portos_validation", "{fact}: {text}");
                assert!(text.ends_with("nothing was registered"), "{fact}: {text}");
                assert_eq!(stored, Lookup::NotFound, "{fact}");
            }
            // a print error after the registration
            Outcome::Indeterminate(reason) => {
                assert!(
                    matches!(stored, Lookup::Found(_)),
                    "{fact}: {reason} / {stored:?}"
                );
            }
        }
    }
}
