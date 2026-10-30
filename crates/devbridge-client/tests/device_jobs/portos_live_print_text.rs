//! `fiscal.print_text` (#120) against a REAL Portos eKasa API 7.0.4 —
//! InMemory storage + Playground ONLY (CI job `portos-live`), never a
//! store's Portos. What this Portos' printer answers is PRINTED (the CI log
//! is the evidence on the ticket); whatever it is, Portos must take the
//! request as we shape it and no receipt may appear.

use std::time::Duration;

use serde_json::json;

use devbridge_client::device_jobs::executor::{DeviceExecutor, Outcome};

use super::portos_live::{executor, job, next_number, orp, portos_url, scratch_journal, unique};

#[tokio::test]
async fn live_print_text_never_registers_a_receipt() {
    let orp = orp();
    let e = executor(&scratch_journal(), &portos_url(), Duration::from_secs(60));
    let before = next_number(&orp).await;
    let payload = json!({
        "title": "PRIEBEŽNÁ UZÁVIERKA (X)",
        "lines": ["NEFIŠKÁLNY DOKLAD –", "NIE JE DOKLAD O PREDAJI", "", "Hotovosť: 12,40 €"],
        "cut": true
    });
    let print = job(71, "fiscal.print_text", &unique("x"), payload);
    e.check(&print).expect("Odoo's payload shape is accepted");
    let outcome = e.execute(&print).await;
    println!("print_text: {outcome:?}");
    match &outcome {
        Outcome::Done(result) => assert_eq!(result["printed"], true, "{result}"),
        Outcome::Failed { code, .. } => assert_eq!(code, "outcome_unknown_retry_safe"),
        Outcome::Indeterminate(text) => panic!("Portos did not answer: {text}"),
    }
    assert_eq!(next_number(&orp).await, before, "no receipt was registered");
}
