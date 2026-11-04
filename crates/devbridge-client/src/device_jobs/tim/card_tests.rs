//! Pure card-payment decisions (#102 D2) — no library, no terminal.

use chrono::TimeZone;
use serde_json::json;

use super::*;

fn job(payload: Value) -> DeviceJob {
    DeviceJob {
        id: 7,
        job_type: JOB_PURCHASE.into(),
        client_uuid: "u-7".into(),
        kasa_id: Some(3),
        kasa_name: String::new(),
        pos_order: Value::Null,
        payload,
        created_at: String::new(),
        claimed_at: String::new(),
    }
}

/// The simulator's authorisation (eft-probe/probe.out, 4.11.2026).
pub(crate) fn sim_facts(trans_seq: i64, amount_minor: i64) -> TxFacts {
    TxFacts {
        transaction_type: Some(1),
        amount_minor: Some(amount_minor),
        currency_eur: Some(true),
        trans_seq: Some(trans_seq),
        trans_ref: Some(200),
        trm_trans_ref: Some("0012345678100".into()),
        auth_code: Some("300".into()),
        pos_entry_mode: Some(18),
        cvm: Some(1),
        brand_name: Some("VISA".into()),
        card_number_printable: Some("XXXXXXXXXXXX5240".into()),
        merchant_receipt: Some("MERCHANT\nslip".into()),
        cardholder_receipt: Some("CARDHOLDER\nslip".into()),
    }
}

#[test]
fn test_transaction_error_classes() {
    for (rc, code) in [
        (2, CODE_CANCELLED),
        (53, CODE_CANCELLED),
        (54, CODE_CANCELLED),
        (55, CODE_CANCELLED),
        (32, CODE_CARD_ERROR),
        (48, CODE_CARD_ERROR),
        (74, CODE_CARD_ERROR),
        (76, CODE_CARD_ERROR),
        (75, CODE_DECLINED),
        (77, CODE_DECLINED),
        (82, CODE_DECLINED),
        (88, CODE_DECLINED),
        (91, CODE_DECLINED),
        (93, CODE_DECLINED),
        (97, CODE_DECLINED),
        (118, CODE_DECLINED),
        (123, CODE_DECLINED),
        (138, CODE_DECLINED),
        (152, CODE_DECLINED),
        (154, CODE_DECLINED),
        (156, CODE_DECLINED),
        (158, CODE_DECLINED),
        (161, CODE_DECLINED),
        (69, CODE_TERMINAL_BUSY),
        (70, CODE_TERMINAL_BUSY),
        (162, CODE_TERMINAL_BUSY),
    ] {
        assert_eq!(
            classify_transaction_error(rc),
            TxFailure::NothingCharged(code),
            "rc {rc}"
        );
    }
    // a lost connection, a timeout, an unknown host outcome, anything else
    for rc in [
        0, 1, 3, 9, 10, 11, 13, 14, 31, 49, 56, 58, 60, 71, 89, 90, 94, 119, 122, 139, 151, 153,
        157, 163, 999,
    ] {
        assert_eq!(
            classify_transaction_error(rc),
            TxFailure::Unknown,
            "rc {rc}"
        );
    }
}

#[test]
fn test_amount_minor_and_currency() {
    assert_eq!(amount_minor(&job(json!({"amount": 2.5}))), Some(250));
    assert_eq!(amount_minor(&job(json!({"amount": 0.01}))), Some(1));
    assert_eq!(amount_minor(&job(json!({"amount": 19.99}))), Some(1999));
    assert_eq!(
        amount_minor(&job(json!({"amount": 1.005}))),
        None,
        "not whole cents"
    );
    assert_eq!(amount_minor(&job(json!({"amount": 0.004}))), None);
    assert_eq!(amount_minor(&job(json!({"amount": 0}))), None);
    assert_eq!(amount_minor(&job(json!({"amount": -1}))), None);
    assert_eq!(amount_minor(&job(json!({"amount": "1"}))), None);
    assert_eq!(amount_minor(&job(json!({"amount": 1e12}))), None, "absurd");
    assert_eq!(
        amount_minor(&job(json!({"amount": 99_999_999_999.0}))),
        Some(9_999_999_999_900)
    );
    assert_eq!(amount_minor(&job(json!({}))), None);
    assert!(currency_ok(&job(json!({}))));
    assert!(currency_ok(&job(json!({"currency": null}))));
    assert!(currency_ok(&job(json!({"currency": "EUR"}))));
    assert!(!currency_ok(&job(json!({"currency": "eur"}))));
    assert!(!currency_ok(&job(json!({"currency": 978}))));
}

#[test]
fn test_settle_target_and_mismatch() {
    assert_eq!(
        settle_target(&job(json!({"card_job_id": 4711}))),
        Some(4711)
    );
    assert_eq!(settle_target(&job(json!({"card_job_id": 0}))), None);
    assert_eq!(settle_target(&job(json!({"card_job_id": "4711"}))), None);
    assert_eq!(settle_target(&job(json!({}))), None);
    let auth = card_result("authorized", Some("12345678"), &sim_facts(100, 250));
    assert_eq!(
        settle_mismatch(&job(json!({"card_job_id": 7})), &auth),
        None
    );
    assert_eq!(
        settle_mismatch(
            &job(json!({"card_job_id": 7, "trans_seq": 100, "amount": 2.5})),
            &auth
        ),
        None
    );
    assert_eq!(
        settle_mismatch(&job(json!({"trans_seq": null, "amount": null})), &auth),
        None
    );
    let seq = settle_mismatch(&job(json!({"trans_seq": 101})), &auth).unwrap();
    assert!(
        seq.contains("trans_seq 101 is not the authorised 100"),
        "{seq}"
    );
    let amount = settle_mismatch(&job(json!({"amount": 2.51})), &auth).unwrap();
    assert!(
        amount.contains("amount 2.51 is not the authorised 2.5"),
        "{amount}"
    );
    assert!(settle_mismatch(&job(json!({"trans_seq": "100"})), &auth).is_some());
    assert_eq!(
        settle_mismatch(&job(json!({"trans_seq": 1})), &json!({})),
        None,
        "no authorisation stored"
    );
}

#[test]
fn test_card_number_masking() {
    assert_eq!(mask_card_number("XXXXXXXXXXXX5240"), "XXXXXXXXXXXX5240");
    assert_eq!(mask_card_number("4193581767935240"), "XXXXXXXXXXXX5240");
    assert_eq!(
        mask_card_number("4193 5817 6793 5240"),
        "XXXX XXXX XXXX 5240"
    );
    assert_eq!(mask_card_number("524"), "524");
    assert_eq!(last4("XXXXXXXXXXXX5240").as_deref(), Some("5240"));
    assert_eq!(last4("4193581767935240").as_deref(), Some("5240"));
    assert_eq!(last4("X24"), None);
    assert_eq!(last4("1234").as_deref(), Some("1234"));
}

#[test]
fn test_cvm_and_signature() {
    let names: Vec<&str> = (0..=7).map(|c| cvm_name(Some(c))).collect();
    assert_eq!(
        names,
        [
            "undefined",
            "pin",
            "pin_signature",
            "signature",
            "on_device",
            "no_cvm",
            "other",
            "undefined"
        ]
    );
    assert_eq!(cvm_name(None), "undefined");
    assert!(!needs_signature(Some(1)));
    assert!(needs_signature(Some(2)));
    assert!(needs_signature(Some(3)));
    assert!(!needs_signature(Some(4)));
    assert!(!needs_signature(None));
}

#[test]
fn test_card_result_is_odoos_shape_without_a_pan() {
    let mut facts = sim_facts(100, 100);
    facts.card_number_printable = Some("4193581767935240".into());
    let result = card_result("authorized", Some("12345678"), &facts);
    assert_eq!(
        result,
        json!({
            "state": "authorized",
            "terminal": {"tid": "12345678"},
            "transaction": {"trans_seq": 100, "trm_trans_ref": "0012345678100", "trans_ref": "200",
                            "auth_code": "300", "amount": 1.0, "currency": "EUR",
                            "pos_entry_mode": "18", "cvm": "pin"},
            "card": {"brand_name": "VISA", "card_number_printable": "XXXXXXXXXXXX5240", "last4": "5240"},
            "receipts": {"merchant": "MERCHANT\nslip", "cardholder": "CARDHOLDER\nslip"},
        })
    );
    assert!(!result.to_string().contains("4193581767935240"));
    let bare = card_result("committed", None, &TxFacts::default());
    assert_eq!(bare["transaction"]["currency"], Value::Null);
    assert_eq!(bare["card"]["last4"], Value::Null);
    let mut other = sim_facts(1, 1);
    other.currency_eur = Some(false);
    assert_eq!(
        card_result("authorized", None, &other)["transaction"]["currency"],
        Value::Null
    );
}

#[test]
fn test_authorisation_problem() {
    assert_eq!(authorisation_problem(&sim_facts(100, 250), 250), None);
    assert!(
        authorisation_problem(&sim_facts(100, 249), 250)
            .unwrap()
            .contains("Some(249) cents, the job asked 250")
    );
    let mut no_seq = sim_facts(100, 250);
    no_seq.trans_seq = None;
    assert!(
        authorisation_problem(&no_seq, 250)
            .unwrap()
            .contains("trans_seq")
    );
    let mut chf = sim_facts(100, 250);
    chf.currency_eur = Some(false);
    assert!(authorisation_problem(&chf, 250).unwrap().contains("EUR"));
    chf.currency_eur = None;
    assert!(authorisation_problem(&chf, 250).is_some());
}

fn last(transaction_type: i32, outcome: i32, facts: Option<TxFacts>) -> LastTransaction {
    LastTransaction {
        transaction_type,
        outcome,
        facts,
    }
}

#[test]
fn test_baseline() {
    // nothing on the terminal yet (the simulator's fresh answer)
    assert_eq!(baseline(Some(&last(0, 0, None)), None), Some(0));
    assert_eq!(baseline(Some(&last(0, 0, None)), Some(7)), Some(7));
    // the last transaction's seq wins
    assert_eq!(
        baseline(Some(&last(1, 1, Some(sim_facts(100, 1)))), Some(90)),
        Some(100)
    );
    // a failed last one (no response): the journal's highest
    assert_eq!(baseline(Some(&last(1, 2, None)), Some(90)), Some(90));
    assert_eq!(baseline(Some(&last(1, 2, None)), None), None);
    let mut no_seq = sim_facts(1, 1);
    no_seq.trans_seq = None;
    assert_eq!(baseline(Some(&last(1, 1, Some(no_seq))), Some(5)), Some(5));
    // the terminal could not be asked
    assert_eq!(baseline(None, Some(5)), Some(5));
    assert_eq!(baseline(None, None), None);
}

#[test]
fn test_correlate() {
    let ours = || last(1, 1, Some(sim_facts(101, 250)));
    assert_eq!(
        correlate(Some(100), 250, &ours()),
        Correlation::Charged(sim_facts(101, 250))
    );
    let mut failed = ours();
    failed.outcome = 2;
    assert_eq!(correlate(Some(100), 250, &failed), Correlation::NotCharged);
    let undecidable = |c: Correlation| matches!(c, Correlation::Undecidable(_));
    let mut partial = ours();
    partial.outcome = 3;
    assert!(undecidable(correlate(Some(100), 250, &partial)));
    assert!(undecidable(correlate(None, 250, &ours())), "no baseline");
    assert!(
        undecidable(correlate(Some(101), 250, &ours())),
        "not above the baseline"
    );
    assert!(
        !undecidable(correlate(Some(100), 250, &ours())),
        "exactly one above"
    );
    assert!(
        undecidable(correlate(Some(100), 251, &ours())),
        "another amount"
    );
    let mut reversal = ours();
    reversal.transaction_type = 3;
    assert!(undecidable(correlate(Some(100), 250, &reversal)));
    assert!(
        undecidable(correlate(Some(100), 250, &last(1, 2, None))),
        "no response"
    );
    match correlate(Some(100), 251, &ours()) {
        Correlation::Undecidable(why) => {
            assert!(why.contains("after trans_seq 100, 251 cents"), "{why}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_settle_after() {
    let t = |s| Utc.with_ymd_and_hms(2026, 11, 4, 15, 0, s).unwrap();
    assert_eq!(settle_after(&[], 60), None);
    assert_eq!(
        settle_after(&[t(0), t(30), t(10)], 60),
        Some(t(30) + ChronoDuration::seconds(70))
    );
    assert_eq!(
        settle_after(&[t(0)], 50),
        Some(t(0) + ChronoDuration::seconds(60))
    );
}

#[test]
fn test_slip_lines() {
    assert_eq!(slip_lines("A\n  B  \n\n"), ["A", "  B"]);
    assert_eq!(slip_lines("A\n\nB"), ["A", "", "B"]);
    let wide = "x".repeat(45);
    assert_eq!(slip_lines(&wide), ["x".repeat(40), "x".repeat(5)]);
    assert_eq!(slip_lines(&"y".repeat(40)), ["y".repeat(40)]);
    assert_eq!(slip_lines("a\tb\u{1a}c\u{200b}d"), ["a b c d"]);
    assert_eq!(slip_lines("\r\nA\r\n"), ["", "A"]);
    let long: String = (0..100).map(|i| format!("{i}\n")).collect();
    let lines = slip_lines(&long);
    assert_eq!(lines.len(), SLIP_MAX_LINES);
    assert_eq!(lines[79], "79");
    assert!(slip_lines("").is_empty());
    // every line fits the Portos checker
    assert!(
        slip_lines(&format!("{}\n\u{7}", "é".repeat(41)))
            .iter()
            .all(|l| l.chars().count() <= SLIP_WIDTH)
    );
}

#[test]
fn test_slip_titles_fit_one_line() {
    assert!(CARDHOLDER_SLIP_TITLE.chars().count() <= SLIP_WIDTH);
    assert!(MERCHANT_SLIP_TITLE.chars().count() <= SLIP_WIDTH);
}
