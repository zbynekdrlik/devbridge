//! Unit tests of the `fiscal.paragon` payload (#115): a written paper
//! paragón registered later in eKasa (zákon 384/2025 Z.z. § 11 ods. 3).

use super::*;
use chrono::TimeZone;
use serde_json::json;

const ORP: &str = "88812345678900001";
const PARAGON: FiscalOp = FiscalOp::Register(ReceiptKind::Paragon);
const CASH: FiscalOp = FiscalOp::Register(ReceiptKind::CashRegister);
const INVALID: FiscalOp = FiscalOp::Register(ReceiptKind::Invalid);

/// "Now" on the store PC in these tests: 2026-10-25 12:00:00 UTC.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap()
}

fn paragon() -> Value {
    json!({
        "data": {
            "cashRegisterCode": ORP,
            "issueDate": "2026-10-23T19:00:22+02:00",
            "paragonNumber": 17,
            "items": [
                {"type": "Positive", "name": "Chlieb kváskový 500 g",
                 "quantity": {"amount": 2, "unit": "ks"},
                 "unitPrice": 1.29, "price": 2.58, "vatRate": 19.0}
            ],
            "payments": [{"name": "Hotovosť", "amount": 2.6}],
            "roundingAmount": 0.02,
            "receiptType": "Paragon",
            "headerText": "Paragón č. 17",
            "footerText": null
        }
    })
}

/// The paragón with `data.<key>` set to `value` (or removed).
fn with_data(key: &str, value: Option<Value>) -> Value {
    let mut p = paragon();
    let data = p["data"].as_object_mut().unwrap();
    match value {
        Some(v) => {
            data.insert(key.to_string(), v);
        }
        None => {
            data.remove(key);
        }
    }
    p
}

fn problems(op: FiscalOp, payload: &Value) -> String {
    check_payload_at(op, payload, now()).expect_err("a refused payload")
}

#[test]
fn test_paragon_job_type_endpoint_and_receipt_type() {
    assert_eq!(FiscalOp::for_job_type("fiscal.paragon"), Some(PARAGON));
    assert_eq!(
        ReceiptKind::Paragon.path(),
        "/api/v1/requests/receipts/paragon"
    );
    assert_eq!(ReceiptKind::Paragon.receipt_type(), "Paragon");
    assert_eq!(cash_register_code(PARAGON, &paragon()), Some(ORP));
    assert_eq!(MAX_PARAGON_NUMBER, u64::from(u32::MAX));
}

#[test]
fn test_a_valid_paragon_and_its_register_body() {
    check_payload_at(PARAGON, &paragon(), now()).expect("a full paragón");
    // the public check reads this PC's clock: a paragón of 2026 is past
    check_payload(PARAGON, &paragon()).expect("the real clock");
    let mut minimal = paragon();
    let data = minimal["data"].as_object_mut().unwrap();
    for key in [
        "payments",
        "roundingAmount",
        "receiptType",
        "headerText",
        "footerText",
    ] {
        data.remove(key);
    }
    check_payload_at(PARAGON, &minimal, now()).expect("a minimal paragón");
    for receipt_type in [json!(null), json!("paragon")] {
        check_payload_at(
            PARAGON,
            &with_data("receiptType", Some(receipt_type.clone())),
            now(),
        )
        .unwrap_or_else(|e| panic!("receiptType {receipt_type}: {e}"));
    }
    // written exactly now (the equal case), in UTC or another offset, long ago
    for written in [
        "2026-10-25T12:00:00Z",
        "2026-10-25T13:00:00+01:00",
        "2026-10-25T11:59:59.999Z",
        "2026-10-01T08:00:00+02:00",
        "2019-01-01T00:00:00Z",
    ] {
        check_payload_at(
            PARAGON,
            &with_data("issueDate", Some(json!(written))),
            now(),
        )
        .unwrap_or_else(|e| panic!("issueDate {written}: {e}"));
    }
    for number in [json!(1), json!(4_294_967_295_u64)] {
        check_payload_at(
            PARAGON,
            &with_data("paragonNumber", Some(number.clone())),
            now(),
        )
        .unwrap_or_else(|e| panic!("paragonNumber {number}: {e}"));
    }
    // Portos gets Odoo's data as is, no print context, devbridge's externalId
    let ext = external_id(3, "paragon-17");
    assert_eq!(
        register_body(&paragon(), &ext),
        json!({"request": {"data": paragon()["data"], "externalId": ext}})
    );
}

#[test]
fn test_paragon_issue_date_problems() {
    let field = "payload.data.issueDate";
    let format = format!(
        "{field} must be a date and time with its UTC offset (RFC 3339, e.g. 2026-10-23T19:00:22+02:00)"
    );
    let later = format!("{field} must not be later than now (the store PC's clock)");
    let cases: Vec<(Option<Value>, String)> = vec![
        (None, format!("{field} is required")),
        (Some(Value::Null), format!("{field} is required")),
        (Some(json!(1_761_400_000)), format.clone()),
        (Some(json!("")), format.clone()),
        (Some(json!("2026-10-23T19:00:22")), format.clone()),
        (Some(json!("2026-10-23")), format.clone()),
        (Some(json!("23.10.2026 19:00")), format.clone()),
        (Some(json!("2026-10-25T12:00:01Z")), later.clone()),
        (Some(json!("2026-10-25T13:00:00.001+01:00")), later.clone()),
        (Some(json!("2027-01-01T00:00:00+02:00")), later),
    ];
    for (value, expected) in cases {
        assert_eq!(
            problems(PARAGON, &with_data("issueDate", value.clone())),
            expected,
            "{value:?}"
        );
    }
    // the pure rule on its own
    assert_eq!(
        issue_date_problem(Some(&json!("2026-10-25T12:00:00Z")), now()),
        None
    );
    assert!(issue_date_problem(Some(&json!("2026-10-25T12:00:00.000000001Z")), now()).is_some());
}

#[test]
fn test_paragon_number_problems() {
    let field = "payload.data.paragonNumber";
    let range = format!("{field} must be a whole number between 1 and 4294967295");
    let cases: Vec<(Option<Value>, String)> = vec![
        (None, format!("{field} is required")),
        (Some(Value::Null), format!("{field} is required")),
        (Some(json!(0)), range.clone()),
        (Some(json!(4_294_967_296_u64)), range.clone()),
        (Some(json!(-1)), range.clone()),
        (Some(json!(1.5)), range.clone()),
        (Some(json!(17.0)), range.clone()),
        (Some(json!("17")), range),
    ];
    for (value, expected) in cases {
        assert_eq!(
            problems(PARAGON, &with_data("paragonNumber", value.clone())),
            expected,
            "{value:?}"
        );
    }
}

#[test]
fn test_paragon_refuses_customer_print_and_foreign_keys() {
    let d = "payload.data";
    assert_eq!(
        problems(
            PARAGON,
            &with_data("customer", Some(json!({"id": "12345678", "type": "ICO"})))
        ),
        format!("{d}.customer is not allowed")
    );
    for print in [json!({"printerName": "pos"}), json!({}), json!("pdf")] {
        let mut p = paragon();
        p["print"] = print.clone();
        assert_eq!(
            problems(PARAGON, &p),
            "payload.print is not allowed for a paragon (a paragón takes no print context)",
            "{print}"
        );
    }
    let mut no_print = paragon();
    no_print["print"] = Value::Null;
    check_payload_at(PARAGON, &no_print, now()).expect("a null print is left out");
    for (key, value) in [
        ("amount", json!(2.6)),
        ("invoiceNumber", json!("201801001")),
        ("externalId", json!("mine")),
    ] {
        assert_eq!(
            problems(PARAGON, &with_data(key, Some(value))),
            format!("{d}.{key} is not allowed")
        );
    }
    assert_eq!(
        problems(
            PARAGON,
            &with_data("receiptType", Some(json!("CashRegister")))
        ),
        format!("{d}.receiptType must be Paragon (or left out)")
    );
    // the items and payments follow the sale's rules
    assert_eq!(
        problems(PARAGON, &with_data("items", Some(json!([])))),
        format!("{d}.items must not be empty")
    );
    assert_eq!(
        problems(PARAGON, &with_data("items", None)),
        format!("{d}.items is required")
    );
    assert_eq!(
        problems(
            PARAGON,
            &with_data(
                "payments",
                Some(json!([{"name": "Hotovosť", "amount": 2.605}]))
            )
        ),
        format!("{d}.payments[0].amount must have at most 2 decimal places")
    );
    assert_eq!(
        problems(PARAGON, &with_data("cashRegisterCode", None)),
        format!("{d}.cashRegisterCode is required")
    );
    // several problems, all named, no value echoed
    let mut bad = with_data("paragonNumber", Some(json!(0)));
    bad["print"] = json!({"printerName": "pos"});
    bad["data"]["customer"] = json!({"id": "TAJNE-ICO", "type": "ICO"});
    let text = problems(PARAGON, &bad);
    assert_eq!(
        text,
        "payload.print is not allowed for a paragon (a paragón takes no print context); \
         payload.data.paragonNumber must be a whole number between 1 and 4294967295; \
         payload.data.customer is not allowed"
    );
    assert!(!text.contains("TAJNE"), "{text}");
}

/// Only a paragón takes `issueDate` / `paragonNumber`; a sale keeps its print
/// context and customer.
#[test]
fn test_only_a_paragon_takes_its_own_keys() {
    let sale = json!({"data": {"cashRegisterCode": ORP, "items": [
        {"type": "Positive", "name": "Rožok", "quantity": {"amount": 1}, "unitPrice": 0.12, "price": 0.12}
    ]}, "print": {"printerName": "pos"}});
    check_payload_at(CASH, &sale, now()).expect("a sale with print");
    for op in [CASH, INVALID] {
        for (key, value) in [
            ("issueDate", json!("2026-10-23T19:00:22+02:00")),
            ("paragonNumber", json!(17)),
        ] {
            let mut p = sale.clone();
            p["data"]["receiptType"] = Value::Null;
            p["data"][key] = value;
            assert_eq!(
                problems(op, &p),
                format!("payload.data.{key} is not allowed"),
                "{op:?}"
            );
        }
    }
    let mut with_customer = sale.clone();
    with_customer["data"]["customer"] = json!({"id": "12345678", "type": "ICO"});
    check_payload_at(CASH, &with_customer, now()).expect("a sale may name its customer");
}
