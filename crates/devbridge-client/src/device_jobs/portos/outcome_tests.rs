//! Unit tests of the Portos answer → job outcome tables (#100).

use super::*;
use serde_json::json;

const EXT: &str = "dbj-3-uuid-1";

/// A Portos `RegisterReceiptResultDTO` (shape of the 7.0.4 swagger example).
fn portos_receipt(is_successful: Value, external_id: Value) -> Value {
    let response = if is_successful == json!(true) {
        json!({"data": {"id": "O-7DBCDA8A56EE426DBCDA8A56EE426D1A"}, "processDate": "2026-10-15T09:57:00+02:00"})
    } else {
        Value::Null
    };
    let error = if is_successful == json!(false) {
        json!({"message": "Chyba v podpise dátovej správy.", "code": -10})
    } else {
        Value::Null
    };
    json!({
        "request": {
            "data": {
                "receiptType": "CashRegister", "amount": 2.58, "roundingAmount": 0.02,
                "issueDate": "2026-10-15T09:56:59+02:00", "receiptNumber": 42,
                "okp": "04eacca4-6ff07dea-c2c85513-28181bb6-f46edcb0", "pkp": "GYTI",
                "items": [{"name": "Chlieb"}], "payments": [{"name": "Hotovosť", "amount": 2.6}],
                "cashRegisterCode": "88812345678900001"
            },
            "id": "2d419057-807c-41be-a5d6-aafea8eba13e",
            "externalId": external_id, "date": "2026-10-15T09:56:59+02:00", "sendingCount": 1
        },
        "response": response,
        "isSuccessful": is_successful,
        "error": error,
        "$type": "Receipt"
    })
}

fn answer(status: u16, body: Value) -> Reply {
    Reply::Answer { status, body }
}

fn validation_problem() -> Value {
    json!({"traceId": null, "code": -900, "title": "One or more validation errors occurred.", "status": 400,
           "errors": {"Request.Data.Items[0].Name": ["Názov nesmie byť prázdny."]}})
}

#[test]
fn test_via_names() {
    assert_eq!(Via::Register.as_str(), "register");
    assert_eq!(Via::Lookup.as_str(), "lookup");
    assert_eq!(Via::Reconcile.as_str(), "reconcile");
}

#[test]
fn test_receipt_result_online_offline_rejected() {
    let online =
        receipt_result(&portos_receipt(json!(true), json!(EXT)), EXT, Via::Register).unwrap();
    assert_eq!(
        online,
        json!({
            "device": "portos", "via": "register", "external_id": EXT,
            "state": "online", "is_offline": false,
            "uid": "O-7DBCDA8A56EE426DBCDA8A56EE426D1A",
            "okp": "04eacca4-6ff07dea-c2c85513-28181bb6-f46edcb0",
            "receipt_number": 42, "issue_date": "2026-10-15T09:56:59+02:00",
            "receipt_type": "CashRegister", "amount": 2.58, "rounding_amount": 0.02,
            "cash_register_code": "88812345678900001",
            "portos_request_id": "2d419057-807c-41be-a5d6-aafea8eba13e",
            "process_date": "2026-10-15T09:57:00+02:00", "ekasa_error": null
        })
    );
    assert!(
        online.get("items").is_none() && online.get("pkp").is_none(),
        "no item data"
    );

    let offline = receipt_result(
        &portos_receipt(Value::Null, json!(EXT)),
        EXT,
        Via::Reconcile,
    )
    .unwrap();
    assert_eq!(offline["state"], "offline");
    assert_eq!(offline["is_offline"], true);
    assert_eq!(offline["uid"], Value::Null);
    assert_eq!(offline["process_date"], Value::Null);
    assert_eq!(offline["via"], "reconcile");

    let mut missing = portos_receipt(Value::Null, json!(EXT));
    missing.as_object_mut().unwrap().remove("isSuccessful");
    assert_eq!(receipt_state(&missing), "offline");

    let rejected =
        receipt_result(&portos_receipt(json!(false), Value::Null), EXT, Via::Lookup).unwrap();
    assert_eq!(rejected["state"], "rejected");
    assert_eq!(rejected["is_offline"], false);
    assert_eq!(
        rejected["ekasa_error"],
        json!({"message": "Chyba v podpise dátovej správy.", "code": -10})
    );
    assert_eq!(
        rejected["external_id"], EXT,
        "a null externalId in the answer is ours"
    );
}

#[test]
fn test_receipt_result_refuses_foreign_or_unreadable_answers() {
    assert_eq!(
        receipt_result(
            &portos_receipt(json!(true), json!("dbj-3-other")),
            EXT,
            Via::Lookup
        ),
        None
    );
    for broken in [
        json!({}),
        json!({"request": {}}),
        json!({"request": {"data": {"receiptNumber": 1}}}),
        json!({"request": {"data": {"okp": "x"}}}),
        json!({"request": {"data": {"okp": "x", "receiptNumber": "1"}}}),
        json!({"request": {"data": {"okp": 5, "receiptNumber": 1}}}),
        json!([]),
    ] {
        assert_eq!(
            receipt_result(&broken, EXT, Via::Register),
            None,
            "{broken}"
        );
    }
    let bare = receipt_result(
        &json!({"request": {"data": {"okp": "x", "receiptNumber": 1}}}),
        EXT,
        Via::Register,
    )
    .unwrap();
    assert_eq!(bare["okp"], "x");
    assert_eq!(bare["portos_request_id"], Value::Null);
}

#[test]
fn test_problem_text() {
    assert_eq!(
        problem_text(400, &validation_problem()),
        "HTTP 400 | code -900 | One or more validation errors occurred. | Request.Data.Items[0].Name: Názov nesmie byť prázdny."
    );
    assert_eq!(problem_text(500, &Value::Null), "HTTP 500");
    assert_eq!(
        problem_text(
            500,
            &json!({"title": "", "detail": "Tlačiareň 'pdf12' nebola nájdená.", "errors": {"a": "x"}})
        ),
        "HTTP 500 | Tlačiareň 'pdf12' nebola nájdená. | a: "
    );
    let long = json!({"detail": "d".repeat(2000)});
    assert_eq!(problem_text(500, &long).chars().count(), 600);
}

#[test]
fn test_classify_lookup() {
    let receipt = portos_receipt(json!(true), json!(EXT));
    assert_eq!(
        classify_lookup(answer(200, receipt.clone())),
        Lookup::Found(receipt)
    );
    assert_eq!(classify_lookup(answer(404, Value::Null)), Lookup::NotFound);
    assert_eq!(classify_lookup(answer(204, Value::Null)), Lookup::NotFound);
    assert_eq!(
        classify_lookup(answer(
            403,
            json!({"code": -104, "title": "Doklad sa nenašiel"})
        )),
        Lookup::NotFound
    );
    assert_eq!(
        classify_lookup(answer(400, json!({"code": -104}))),
        Lookup::NotFound
    );
    assert_eq!(
        classify_lookup(answer(500, json!({"code": -104}))),
        Lookup::Failed("HTTP 500 | code -104".into()),
        "a 5xx never means not found"
    );
    assert_eq!(
        classify_lookup(answer(403, json!({"code": -101}))),
        Lookup::Failed("HTTP 403 | code -101".into())
    );
    assert_eq!(
        classify_lookup(answer(200, Value::Null)),
        Lookup::Failed("HTTP 200".into())
    );
    assert_eq!(
        classify_lookup(answer(201, json!({}))),
        Lookup::Failed("HTTP 201".into())
    );
    assert_eq!(
        classify_lookup(Reply::Unreachable("refused".into())),
        Lookup::Unreachable("refused".into())
    );
    assert_eq!(
        classify_lookup(Reply::Lost("timeout".into())),
        Lookup::Failed("no answer: timeout".into())
    );
}

fn failed_code(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Failed { code, .. } => code,
        other => panic!("not failed: {other:?}"),
    }
}

fn text_of(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Failed { text, .. } => text,
        Outcome::Indeterminate(reason) => reason,
        Outcome::Done(v) => panic!("done: {v}"),
    }
}

#[test]
fn test_register_outcome_table() {
    let kind = ReceiptKind::CashRegister;
    // registered: online (200) and offline (202)
    for (status, success) in [(200, json!(true)), (202, Value::Null)] {
        match register_outcome(
            answer(status, portos_receipt(success, json!(EXT))),
            kind,
            EXT,
        ) {
            Outcome::Done(result) => {
                assert_eq!(result["via"], "register");
                assert_eq!(result["receipt_number"], 42);
            }
            other => panic!("{status}: {other:?}"),
        }
    }
    // never sent
    let o = register_outcome(Reply::Unreachable("connection refused".into()), kind, EXT);
    assert_eq!(failed_code(&o), CODE_UNREACHABLE);
    assert!(
        text_of(&o).contains("connection refused") && text_of(&o).ends_with("nothing ran"),
        "{o:?}"
    );
    // validation: nothing stored
    let o = register_outcome(answer(400, validation_problem()), kind, EXT);
    assert_eq!(failed_code(&o), CODE_VALIDATION);
    assert!(
        text_of(&o).contains("CashRegister receipt as invalid"),
        "{o:?}"
    );
    assert!(text_of(&o).contains("Názov nesmie byť prázdny."), "{o:?}");
    assert!(text_of(&o).ends_with("nothing was registered"), "{o:?}");
    assert_eq!(
        failed_code(&register_outcome(answer(422, Value::Null), kind, EXT)),
        CODE_VALIDATION
    );
    let o = register_outcome(answer(401, json!({"code": -1000})), kind, EXT);
    assert_eq!(failed_code(&o), CODE_UNAUTHENTICATED);
    assert!(text_of(&o).contains("HTTP 401 | code -1000"), "{o:?}");
    let o = register_outcome(answer(404, Value::Null), ReceiptKind::Deposit, EXT);
    assert_eq!(failed_code(&o), CODE_ENDPOINT_MISSING);
    assert!(
        text_of(&o).contains("no /api/v1/requests/receipts/deposit"),
        "{o:?}"
    );
    assert!(
        text_of(&o).contains("cannot register Deposit receipts"),
        "{o:?}"
    );
    assert_eq!(
        failed_code(&register_outcome(answer(405, Value::Null), kind, EXT)),
        CODE_ENDPOINT_MISSING
    );

    // may be registered → indeterminate, reconciled by the externalId
    let maybe: Vec<Reply> = vec![
        Reply::Lost("operation timed out".into()),
        answer(
            500,
            json!({"code": -502, "title": "Tlač dokladu zlyhala", "status": 500}),
        ),
        answer(
            500,
            json!({"code": -403, "key": {"name": "RegistrationRequestExternalId"}}),
        ),
        answer(503, Value::Null),
        answer(400, json!({"code": -101})),
        answer(404, json!({"code": -201})),
        answer(403, Value::Null),
        answer(200, Value::Null),
        answer(
            200,
            portos_receipt(json!(true), json!("dbj-3-someone-else")),
        ),
    ];
    for reply in maybe {
        let o = register_outcome(reply.clone(), kind, EXT);
        match &o {
            Outcome::Indeterminate(reason) => {
                assert!(
                    reason.contains("reconciled by externalId dbj-3-uuid-1"),
                    "{reason}"
                );
                assert!(reason.contains("never registered again"), "{reason}");
            }
            other => panic!("{reply:?} → {other:?}"),
        }
    }
    let o = register_outcome(answer(500, json!({"code": -502})), kind, EXT);
    assert!(
        text_of(&o)
            .starts_with("Portos answered HTTP 500 | code -502 - the receipt may be registered"),
        "{o:?}"
    );
    let o = register_outcome(Reply::Lost("reset".into()), kind, EXT);
    assert!(
        text_of(&o).starts_with("no answer from Portos after the receipt was sent (reset)"),
        "{o:?}"
    );
}

#[test]
fn test_control_outcome_table() {
    let drawer = FiscalOp::OpenDrawer;
    let copy = FiscalOp::PrintCopy;
    assert_eq!(
        control_outcome(answer(200, json!({"opened": true})), drawer),
        Outcome::Done(json!({"device": "portos", "opened": true, "printer_ready": true}))
    );
    assert_eq!(
        control_outcome(answer(202, json!({"opened": false})), drawer),
        Outcome::Done(json!({"device": "portos", "opened": false, "printer_ready": false}))
    );
    assert_eq!(
        control_outcome(answer(200, json!({"printed": true})), copy),
        Outcome::Done(json!({"device": "portos", "printed": true, "printer_ready": true}))
    );
    assert_eq!(
        control_outcome(answer(200, Value::Null), copy),
        Outcome::Done(json!({"device": "portos", "printed": null, "printer_ready": true}))
    );
    let o = control_outcome(Reply::Unreachable("refused".into()), drawer);
    assert_eq!(failed_code(&o), CODE_UNREACHABLE);
    assert!(text_of(&o).ends_with("nothing ran"));
    let o = control_outcome(
        answer(403, json!({"code": -104, "title": "Doklad sa nenašiel"})),
        copy,
    );
    assert_eq!(failed_code(&o), CODE_RECEIPT_NOT_FOUND);
    assert!(
        text_of(&o).contains("Doklad sa nenašiel") && text_of(&o).ends_with("nothing was printed")
    );
    assert_eq!(
        failed_code(&control_outcome(answer(400, validation_problem()), copy)),
        CODE_VALIDATION
    );
    assert_eq!(
        failed_code(&control_outcome(answer(401, Value::Null), copy)),
        CODE_UNAUTHENTICATED
    );
    assert_eq!(
        failed_code(&control_outcome(answer(404, Value::Null), drawer)),
        CODE_ENDPOINT_MISSING
    );
    assert!(
        text_of(&control_outcome(answer(404, Value::Null), drawer))
            .contains("cannot open the drawer")
    );
    let o = control_outcome(answer(409, Value::Null), copy);
    assert_eq!(failed_code(&o), CODE_REFUSED);
    assert!(text_of(&o).contains("refused to print the receipt copy"));
    assert_eq!(
        failed_code(&control_outcome(answer(404, json!({"code": -5})), copy)),
        CODE_REFUSED
    );
    for reply in [
        Reply::Lost("timeout".into()),
        answer(500, json!({"code": -501})),
        answer(503, Value::Null),
    ] {
        assert!(
            matches!(
                control_outcome(reply.clone(), drawer),
                Outcome::Indeterminate(_)
            ),
            "{reply:?}"
        );
    }
    assert!(
        text_of(&control_outcome(Reply::Lost("t".into()), drawer)).contains("open the drawer (t)")
    );
    assert!(
        text_of(&control_outcome(answer(500, Value::Null), copy))
            .contains("print the receipt copy")
    );
}

#[test]
fn test_retry_safe_texts() {
    let drawer = retry_safe_text(FiscalOp::OpenDrawer);
    assert!(drawer.contains("whether the drawer opened"), "{drawer}");
    assert!(drawer.contains("the till may simply retry"), "{drawer}");
    let copy = retry_safe_text(FiscalOp::PrintCopy);
    assert!(
        copy.contains("whether the receipt copy was printed"),
        "{copy}"
    );
}

#[test]
fn test_classify_unprocessed() {
    let entry =
        |ext: &str| json!({"request": {"id": "x", "externalId": ext}, "isSuccessful": null});
    assert_eq!(
        classify_unprocessed(answer(204, Value::Null), EXT),
        Unprocessed::NotListed
    );
    assert_eq!(
        classify_unprocessed(answer(200, json!([])), EXT),
        Unprocessed::NotListed
    );
    assert_eq!(
        classify_unprocessed(answer(200, json!([entry("dbj-3-other"), entry(EXT)])), EXT),
        Unprocessed::Listed
    );
    assert_eq!(
        classify_unprocessed(answer(200, json!([entry("dbj-3-other")])), EXT),
        Unprocessed::NotListed,
        "only entries of THIS externalId count"
    );
    assert_eq!(
        classify_unprocessed(answer(200, json!({"a": 1})), EXT),
        Unprocessed::Failed("HTTP 200".into())
    );
    assert_eq!(
        classify_unprocessed(answer(500, json!({"code": -400})), EXT),
        Unprocessed::Failed("HTTP 500 | code -400".into())
    );
    assert_eq!(
        classify_unprocessed(Reply::Unreachable("refused".into()), EXT),
        Unprocessed::Failed("refused".into())
    );
    assert_eq!(
        classify_unprocessed(Reply::Lost("timeout".into()), EXT),
        Unprocessed::Failed("timeout".into())
    );
}

#[test]
fn test_sent_but_not_found_texts() {
    let listed = sent_but_not_found_text(Unprocessed::Listed, EXT);
    assert_eq!(
        listed,
        "Portos lists the request with externalId dbj-3-uuid-1 as not processed yet - waiting for Portos"
    );
    let unproven = sent_but_not_found_text(Unprocessed::NotListed, EXT);
    assert!(
        unproven.starts_with("devbridge sent the receipt but Portos has neither"),
        "{unproven}"
    );
    assert!(
        unproven.contains("a person must check Portos WebAdmin for externalId dbj-3-uuid-1"),
        "{unproven}"
    );
    assert!(
        unproven.ends_with("never register this sale again under another id"),
        "{unproven}"
    );
    assert!(
        !unproven.contains("  "),
        "no doubled spaces from the line breaks: {unproven}"
    );
    assert_eq!(
        sent_but_not_found_text(Unprocessed::Failed("HTTP 500".into()), EXT),
        "Portos has no receipt with externalId dbj-3-uuid-1; its unprocessed list could not be read (HTTP 500) - asked again later"
    );
}

#[test]
fn test_receipt_summary() {
    let done = Outcome::Done(json!({"receipt_number": 42, "state": "offline"}));
    assert_eq!(receipt_summary(&done), "receipt 42 (offline)");
    assert_eq!(
        receipt_summary(&Outcome::Done(json!({"receipt_number": 7}))),
        "receipt 7 (?)"
    );
    assert_eq!(receipt_summary(&Outcome::Done(json!({"opened": true}))), "");
    assert_eq!(
        receipt_summary(&Outcome::Done(json!({"receipt_number": "7"}))),
        ""
    );
    assert_eq!(receipt_summary(&Outcome::failed("a", "b")), "");
    assert_eq!(receipt_summary(&Outcome::Indeterminate("x".into())), "");
}
