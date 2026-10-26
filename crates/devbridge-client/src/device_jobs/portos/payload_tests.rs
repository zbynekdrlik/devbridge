//! Unit tests of the `fiscal.*` payload contract (#100).

use super::*;
use serde_json::json;

const ORP: &str = "88812345678900001";

fn sale() -> Value {
    json!({
        "data": {
            "cashRegisterCode": ORP,
            "items": [
                {"type": "positive", "name": "Chlieb kváskový 500 g",
                 "quantity": {"amount": 2.0, "unit": "ks"},
                 "unitPrice": 1.29, "price": 2.58, "vatRate": 19.0},
                {"type": "Discount", "name": "Zľava", "quantity": {"amount": 1},
                 "unitPrice": -0.5, "price": -0.5, "vatRate": 19.0,
                 "description": "\u{1a}text\u{1e}content\u{1f}vernostná zľava\u{1a}"},
                {"type": "ReturnedContainer", "name": "Fľaša", "quantity": {"amount": 1, "unit": null},
                 "unitPrice": -0.15, "price": -0.15, "vatRate": null,
                 "referenceReceiptId": null, "specialRegulation": null, "voucherNumber": null,
                 "seller": null}
            ],
            "payments": [{"name": "Hotovosť", "amount": 2.0}, {"name": "Platobná karta", "amount": -0.07}],
            "roundingAmount": -0.02,
            "customer": {"id": "12345678", "type": "ICO"},
            "receiptType": "CashRegister",
            "headerText": "Pekáreň Bastion",
            "footerText": null
        },
        "print": {"printerName": "pos", "options": {"OpenDrawer": true}}
    })
}

fn problems(op: FiscalOp, payload: &Value) -> String {
    check_payload(op, payload).expect_err("a refused payload")
}

const CASH: FiscalOp = FiscalOp::Register(ReceiptKind::CashRegister);
const INVALID: FiscalOp = FiscalOp::Register(ReceiptKind::Invalid);
const DEPOSIT: FiscalOp = FiscalOp::Register(ReceiptKind::Deposit);
const WITHDRAW: FiscalOp = FiscalOp::Register(ReceiptKind::Withdraw);

#[test]
fn test_job_types_and_receipt_kinds() {
    for (t, op) in [
        ("fiscal.cash_register", CASH),
        ("fiscal.invalid", INVALID),
        ("fiscal.deposit", DEPOSIT),
        ("fiscal.withdraw", WITHDRAW),
        ("fiscal.paragon", FiscalOp::Register(ReceiptKind::Paragon)),
        ("fiscal.print_copy", FiscalOp::PrintCopy),
        ("fiscal.open_drawer", FiscalOp::OpenDrawer),
    ] {
        assert_eq!(FiscalOp::for_job_type(t), Some(op), "{t}");
    }
    for t in [
        "fiscal.invoice",
        "fiscal.invoice_paragon",
        "fiscal",
        "card.purchase",
        "",
        "Fiscal.cash_register",
    ] {
        assert_eq!(FiscalOp::for_job_type(t), None, "{t}");
    }
    let kinds = [
        (
            ReceiptKind::CashRegister,
            "/api/v1/requests/receipts/cash_register",
            "CashRegister",
        ),
        (
            ReceiptKind::Invalid,
            "/api/v1/requests/receipts/invalid",
            "Invalid",
        ),
        (
            ReceiptKind::Deposit,
            "/api/v1/requests/receipts/deposit",
            "Deposit",
        ),
        (
            ReceiptKind::Withdraw,
            "/api/v1/requests/receipts/withdraw",
            "Withdraw",
        ),
        (
            ReceiptKind::Paragon,
            "/api/v1/requests/receipts/paragon",
            "Paragon",
        ),
    ];
    for (kind, path, receipt_type) in kinds {
        assert_eq!(kind.path(), path);
        assert_eq!(kind.receipt_type(), receipt_type);
    }
}

#[test]
fn test_external_id_and_client_uuid() {
    // "dbj-" + the first 32 hex of SHA-256("<kasa_id>:<client_uuid>")
    // (vectors from Python hashlib, so #101 can check its own implementation)
    let uuid = "6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a";
    assert_eq!(external_id(3, uuid), "dbj-b254a45f6c31a2b6b7815da564522aec");
    assert_eq!(external_id(4, uuid), "dbj-7f4a84ac850eafcdf72d54f836ca18f5");
    assert_eq!(external_id(12, "a"), "dbj-4278b9eba36bd346856170cca2665a57");
    assert_eq!(external_id(3, uuid), external_id(3, uuid), "deterministic");
    assert_ne!(external_id(3, uuid), external_id(4, uuid), "another till");
    assert_ne!(
        external_id(3, "a"),
        external_id(3, "b"),
        "another operation"
    );
    assert_ne!(
        external_id(31, "2"),
        external_id(3, "12"),
        "the separator counts"
    );
    // the longest client_uuid the Odoo contract allows still fits Portos
    let longest = "x".repeat(64);
    let mixed = "Z-_9".repeat(16);
    for client_uuid in ["a", uuid, longest.as_str(), mixed.as_str()] {
        let id = external_id(i64::MAX, client_uuid);
        assert_eq!(id.len(), 36, "{id}");
        assert!(id.len() <= EXTERNAL_ID_MAX_CHARS, "{id}");
        assert!(id.starts_with("dbj-"), "{id}");
        assert!(
            id[4..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{id}"
        );
    }
    assert_eq!(EXTERNAL_ID_MAX_CHARS, 40);
    assert!(client_uuid_ok("6f1c2a0e-7b1d-4c55_9A77"));
    assert!(client_uuid_ok("a"));
    assert!(client_uuid_ok(&"x".repeat(64)));
    for bad in ["", " ", "a b", "a/b", "á", "a.b", "a\u{0}"] {
        assert!(!client_uuid_ok(bad), "{bad:?}");
    }
    assert!(!client_uuid_ok(&"x".repeat(65)));
}

#[test]
fn test_has_at_most_decimals() {
    assert!(has_at_most_decimals(2.58, 2));
    assert!(!has_at_most_decimals(2.585, 2));
    assert!(has_at_most_decimals(1.123456, 6));
    assert!(!has_at_most_decimals(1.1234567, 6));
    assert!(
        !has_at_most_decimals(0.1 + 0.2, 2),
        "float noise is refused"
    );
    assert!(has_at_most_decimals(0.3, 2));
    assert!(has_at_most_decimals(10_000_000.0, 6));
    assert!(has_at_most_decimals(-9_999_999.99, 2));
    assert!(has_at_most_decimals(3.0, 0));
    assert!(!has_at_most_decimals(3.5, 0));
    assert!(!has_at_most_decimals(1e-7, 6));
    assert!(has_at_most_decimals(1e-7, 7));
    assert!(!has_at_most_decimals(f64::NAN, 6));
    assert!(!has_at_most_decimals(f64::INFINITY, 6));
}

#[test]
fn test_cash_register_code_and_uuid_formats() {
    assert!(cash_register_code_ok(ORP));
    assert!(cash_register_code_ok("8881234567890000"));
    for bad in [
        "888123456789000",
        "888123456789000012",
        "8881234567890000a",
        "",
        " 8881234567890000",
    ] {
        assert!(!cash_register_code_ok(bad), "{bad:?}");
    }
    assert!(uuid_ok("2d419057-807c-41be-a5d6-aafea8eba13e"));
    assert!(uuid_ok("2D419057-807C-41BE-A5D6-AAFEA8EBA13E"));
    for bad in [
        "2d419057807c41bea5d6aafea8eba13e",
        "2d419057-807c-41be-a5d6-aafea8eba13",
        "2d419057-807c-41be-a5d6-aafea8eba13ee",
        "2d41905g-807c-41be-a5d6-aafea8eba13e",
        "2d419057-807c-41be-a5d6",
        "",
    ] {
        assert!(!uuid_ok(bad), "{bad:?}");
    }
}

#[test]
fn test_valid_payloads_of_every_type() {
    check_payload(CASH, &sale()).expect("a full sale");
    let mut minimal = json!({"data": {"cashRegisterCode": ORP, "items": [
        {"type": "Positive", "name": "Rožok", "quantity": {"amount": 1}, "unitPrice": 0.12, "price": 0.12}
    ]}});
    check_payload(CASH, &minimal).expect("a minimal sale");
    minimal["data"]["receiptType"] = json!("Invalid");
    check_payload(INVALID, &minimal).expect("an invalid (training) receipt");
    for (op, amount, t) in [(DEPOSIT, 100.0, "Deposit"), (WITHDRAW, -55.5, "Withdraw")] {
        check_payload(
            op,
            &json!({"data": {"cashRegisterCode": ORP, "amount": amount}}),
        )
        .expect("cash move");
        check_payload(
            op,
            &json!({"data": {"cashRegisterCode": ORP, "amount": amount, "receiptType": t,
                     "headerText": "Vedúca", "footerText": "Podpis"}, "print": null}),
        )
        .expect("cash move with texts");
    }
    for copy in [
        json!({"cashRegisterCode": ORP, "externalId": "dbj-3-6f1c2a0e"}),
        json!({"cashRegisterCode": ORP, "id": "2d419057-807c-41be-a5d6-aafea8eba13e"}),
        json!({"cashRegisterCode": ORP, "year": 2026, "month": 10, "receiptNumber": 42}),
    ] {
        check_payload(FiscalOp::PrintCopy, &copy).expect("a copy");
    }
    check_payload(FiscalOp::OpenDrawer, &json!({})).expect("drawer, default pin");
    check_payload(FiscalOp::OpenDrawer, &json!({"pin": "Pin5"})).expect("drawer, pin 5");
    check_payload(FiscalOp::OpenDrawer, &json!({"pin": null})).expect("drawer, null pin");
}

#[test]
fn test_payload_shape_problems() {
    assert_eq!(problems(CASH, &json!([])), "payload must be an object");
    assert_eq!(
        problems(FiscalOp::OpenDrawer, &json!(null)),
        "payload must be an object"
    );
    assert_eq!(problems(CASH, &json!({})), "payload.data is required");
    assert_eq!(
        problems(CASH, &json!({"data": "x"})),
        "payload.data must be an object"
    );
    let mut extra = sale();
    extra["externalId"] = json!("mine");
    assert_eq!(problems(CASH, &extra), "payload.externalId is not allowed");
    let mut no_code = sale();
    no_code["data"]
        .as_object_mut()
        .unwrap()
        .remove("cashRegisterCode");
    assert_eq!(
        problems(CASH, &no_code),
        "payload.data.cashRegisterCode is required"
    );
    let mut bad_code = sale();
    bad_code["data"]["cashRegisterCode"] = json!(88812345678900001_u64);
    assert_eq!(
        problems(CASH, &bad_code),
        "payload.data.cashRegisterCode must be the 16-17 digit cash register (ORP) code"
    );
    let mut wrong_type = sale();
    wrong_type["data"]["receiptType"] = json!("Deposit");
    assert_eq!(
        problems(CASH, &wrong_type),
        "payload.data.receiptType must be CashRegister (or left out)"
    );
    let mut null_type = sale();
    null_type["data"]["receiptType"] = Value::Null;
    check_payload(CASH, &null_type).expect("a null receiptType is left out");
    let mut lower = sale();
    lower["data"]["receiptType"] = json!("cashregister");
    check_payload(CASH, &lower).expect("enums are case-insensitive, as Portos reads them");
    let mut number_type = sale();
    number_type["data"]["receiptType"] = json!(1);
    assert_eq!(
        problems(CASH, &number_type),
        "payload.data.receiptType must be CashRegister (or left out)"
    );
    // a code written as a string, but not the 16-17 digit ORP code
    for code in ["123", "8881234567890000a", "888123456789000012"] {
        let mut short = sale();
        short["data"]["cashRegisterCode"] = json!(code);
        assert_eq!(
            problems(CASH, &short),
            "payload.data.cashRegisterCode must be the 16-17 digit cash register (ORP) code",
            "{code}"
        );
        assert_eq!(
            problems(
                FiscalOp::PrintCopy,
                &json!({"cashRegisterCode": code, "externalId": "dbj-3-a"})
            ),
            "payload.cashRegisterCode must be the 16-17 digit cash register (ORP) code",
            "{code}"
        );
    }
}

#[test]
fn test_print_context_problems() {
    let with_print = |print: Value| {
        let mut p = sale();
        p["print"] = print;
        p
    };
    check_payload(CASH, &with_print(json!({"printerName": "PDF"}))).expect("case-insensitive name");
    check_payload(
        CASH,
        &with_print(json!({"printerName": "email", "options": null})),
    )
    .expect("no options");
    assert_eq!(
        problems(CASH, &with_print(json!({"printerName": "lpt1"}))),
        "payload.print.printerName must be one of pos, pdf, email"
    );
    assert_eq!(
        problems(CASH, &with_print(json!({"options": {}}))),
        "payload.print.printerName is required"
    );
    assert_eq!(
        problems(
            CASH,
            &with_print(json!({"printerName": "pos", "options": "x"}))
        ),
        "payload.print.options must be an object"
    );
    assert_eq!(
        problems(
            CASH,
            &with_print(json!({"printerName": "pos", "copies": 2}))
        ),
        "payload.print.copies is not allowed"
    );
    assert_eq!(
        problems(CASH, &with_print(json!("pos"))),
        "payload.print must be an object"
    );
}

#[test]
fn test_cash_move_problems() {
    assert_eq!(
        problems(DEPOSIT, &json!({"data": {"cashRegisterCode": ORP}})),
        "payload.data.amount is required"
    );
    assert_eq!(
        problems(
            WITHDRAW,
            &json!({"data": {"cashRegisterCode": ORP, "amount": 10.005}})
        ),
        "payload.data.amount must have at most 2 decimal places"
    );
    assert_eq!(
        problems(
            DEPOSIT,
            &json!({"data": {"cashRegisterCode": ORP, "amount": "10"}})
        ),
        "payload.data.amount must be a number"
    );
    assert_eq!(
        problems(
            DEPOSIT,
            &json!({"data": {"cashRegisterCode": ORP, "amount": 10_000_000.01}})
        ),
        "payload.data.amount must be between -10000000 and 10000000"
    );
    assert_eq!(
        problems(
            DEPOSIT,
            &json!({"data": {"cashRegisterCode": ORP, "amount": 1, "items": []}})
        ),
        "payload.data.items is not allowed"
    );
    assert_eq!(
        problems(
            DEPOSIT,
            &json!({"data": {"cashRegisterCode": ORP, "amount": 1, "receiptType": "Withdraw"}})
        ),
        "payload.data.receiptType must be Deposit (or left out)"
    );
}

/// The first item of a sale with `field` set to `value` (or removed).
fn item_with(field: &str, value: Option<Value>) -> Value {
    let mut p = sale();
    let item = p["data"]["items"][0].as_object_mut().unwrap();
    match value {
        Some(v) => {
            item.insert(field.to_string(), v);
        }
        None => {
            item.remove(field);
        }
    }
    p
}

#[test]
fn test_item_problems() {
    let i0 = "payload.data.items[0]";
    let cases: Vec<(&str, Option<Value>, String)> = vec![
        ("type", None, format!("{i0}.type is required")),
        (
            "type",
            Some(json!("gift")),
            format!("{i0}.type must be one of {}", ITEM_TYPES.join(", ")),
        ),
        ("name", None, format!("{i0}.name is required")),
        (
            "name",
            Some(json!("")),
            format!("{i0}.name must not be empty"),
        ),
        (
            "name",
            Some(json!("   ")),
            format!("{i0}.name must not be empty"),
        ),
        (
            "name",
            Some(json!("x".repeat(256))),
            format!("{i0}.name must have at most 255 characters"),
        ),
        (
            "name",
            Some(json!("a\u{7}b")),
            format!("{i0}.name must not contain control characters"),
        ),
        (
            "name",
            Some(json!(5)),
            format!("{i0}.name must be a string"),
        ),
        ("price", None, format!("{i0}.price is required")),
        (
            "price",
            Some(json!(2.585)),
            format!("{i0}.price must have at most 2 decimal places"),
        ),
        (
            "price",
            Some(json!(null)),
            format!("{i0}.price must be a number"),
        ),
        (
            "unitPrice",
            Some(json!(1.1234567)),
            format!("{i0}.unitPrice must have at most 6 decimal places"),
        ),
        (
            "unitPrice",
            Some(json!(-10_000_001)),
            format!("{i0}.unitPrice must be between -10000000 and 10000000"),
        ),
        (
            "vatRate",
            Some(json!(19.005)),
            format!("{i0}.vatRate must have at most 2 decimal places"),
        ),
        (
            "vatRate",
            Some(json!("19")),
            format!("{i0}.vatRate must be a number"),
        ),
        ("quantity", None, format!("{i0}.quantity is required")),
        (
            "quantity",
            Some(json!(null)),
            format!("{i0}.quantity is required"),
        ),
        (
            "quantity",
            Some(json!(2)),
            format!("{i0}.quantity must be an object"),
        ),
        (
            "quantity",
            Some(json!({"unit": "ks"})),
            format!("{i0}.quantity.amount is required"),
        ),
        (
            "quantity",
            Some(json!({"amount": -1})),
            format!("{i0}.quantity.amount must be between 0 and 10000000"),
        ),
        (
            "quantity",
            Some(json!({"amount": 1.23456})),
            format!("{i0}.quantity.amount must have at most 4 decimal places"),
        ),
        (
            "quantity",
            Some(json!({"amount": 1, "unit": "kusy"})),
            format!("{i0}.quantity.unit must have at most 3 characters"),
        ),
        (
            "quantity",
            Some(json!({"amount": 1, "packs": 2})),
            format!("{i0}.quantity.packs is not allowed"),
        ),
        (
            "referenceReceiptId",
            Some(json!("")),
            format!("{i0}.referenceReceiptId must not be empty"),
        ),
        (
            "specialRegulation",
            Some(json!("Gift")),
            format!(
                "{i0}.specialRegulation must be one of {}",
                TAX_FREE_REASONS.join(", ")
            ),
        ),
        (
            "voucherNumber",
            Some(json!("v".repeat(51))),
            format!("{i0}.voucherNumber must have at most 50 characters"),
        ),
        (
            "seller",
            Some(json!({"id": "2020123456", "type": "ICO"})),
            format!("{i0}.seller.type must be one of DIC, ICDPH"),
        ),
        (
            "seller",
            Some(json!({"type": "DIC"})),
            format!("{i0}.seller.id is required"),
        ),
        (
            "seller",
            Some(json!("2020123456")),
            format!("{i0}.seller must be an object"),
        ),
        (
            "description",
            Some(json!("d".repeat(2001))),
            format!("{i0}.description must have at most 2000 characters"),
        ),
        (
            "ean",
            Some(json!("858")),
            format!("{i0}.ean is not allowed"),
        ),
    ];
    for (field, value, expected) in cases {
        assert_eq!(
            problems(CASH, &item_with(field, value.clone())),
            expected,
            "{field}: {value:?}"
        );
    }
    // valid edge values
    for (field, value) in [
        ("vatRate", None),
        ("vatRate", Some(json!(null))),
        ("quantity", Some(json!({"amount": 0}))),
        (
            "quantity",
            Some(json!({"amount": 10_000_000, "unit": "kg"})),
        ),
        ("name", Some(json!("x".repeat(255)))),
        (
            "seller",
            Some(json!({"id": "SK2020123456", "type": "icdph"})),
        ),
        ("specialRegulation", Some(json!("UsedGood"))),
        (
            "referenceReceiptId",
            Some(json!("O-7DBCDA8A56EE426DBCDA8A56EE426D1A")),
        ),
    ] {
        check_payload(CASH, &item_with(field, value.clone()))
            .unwrap_or_else(|e| panic!("{field}: {value:?}: {e}"));
    }
}

#[test]
fn test_items_payments_customer_problems() {
    let with_data = |key: &str, value: Value| {
        let mut p = sale();
        p["data"][key] = value;
        p
    };
    let d = "payload.data";
    assert_eq!(
        problems(CASH, &with_data("items", json!([]))),
        format!("{d}.items must not be empty")
    );
    assert_eq!(
        problems(CASH, &with_data("items", json!({}))),
        format!("{d}.items must be a list")
    );
    assert_eq!(
        problems(CASH, &with_data("items", Value::Null)),
        format!("{d}.items is required")
    );
    assert_eq!(
        problems(CASH, &with_data("items", json!(["x"]))),
        format!("{d}.items[0] must be an object")
    );
    assert_eq!(
        problems(CASH, &with_data("payments", json!("cash"))),
        format!("{d}.payments must be a list")
    );
    check_payload(CASH, &with_data("payments", Value::Null)).expect("payments are optional");
    assert_eq!(
        problems(CASH, &with_data("payments", json!([{"amount": 1}]))),
        format!("{d}.payments[0].name is required")
    );
    assert_eq!(
        problems(
            CASH,
            &with_data("payments", json!([{"name": "Hotovosť", "amount": 1.005}]))
        ),
        format!("{d}.payments[0].amount must have at most 2 decimal places")
    );
    assert_eq!(
        problems(
            CASH,
            &with_data(
                "payments",
                json!([{"name": "Hotovosť", "amount": 1, "tip": 1}])
            )
        ),
        format!("{d}.payments[0].tip is not allowed")
    );
    assert_eq!(
        problems(CASH, &with_data("payments", json!([7]))),
        format!("{d}.payments[0] must be an object")
    );
    assert_eq!(
        problems(CASH, &with_data("roundingAmount", json!(0.025))),
        format!("{d}.roundingAmount must have at most 2 decimal places")
    );
    assert_eq!(
        problems(CASH, &with_data("roundingAmount", Value::Null)),
        format!("{d}.roundingAmount must be a number")
    );
    assert_eq!(
        problems(
            CASH,
            &with_data("customer", json!({"id": "12345678", "type": "VAT"}))
        ),
        format!("{d}.customer.type must be one of ICO, DIC, ICDPH, Other")
    );
    assert_eq!(
        problems(
            CASH,
            &with_data("customer", json!({"id": "1", "type": "ICO", "name": "x"}))
        ),
        format!("{d}.customer.name is not allowed")
    );
    assert_eq!(
        problems(CASH, &with_data("headerText", json!("h".repeat(2001)))),
        format!("{d}.headerText must have at most 2000 characters")
    );
    // an invalid (training) receipt names no customer
    let mut invalid = sale();
    invalid["data"]["receiptType"] = json!("Invalid");
    assert_eq!(
        problems(INVALID, &invalid),
        format!("{d}.customer is not allowed")
    );
    invalid["data"].as_object_mut().unwrap().remove("customer");
    check_payload(INVALID, &invalid).expect("an invalid receipt without customer");
}

#[test]
fn test_several_problems_are_all_reported_without_values() {
    let mut p = sale();
    p["data"]["items"][0]["name"] = json!("TAJNA-POLOZKA\u{1}");
    p["data"]["items"][0]["price"] = json!(1.234);
    p["data"]["payments"][0]["amount"] = json!(7.777);
    let text = problems(CASH, &p);
    assert_eq!(
        text,
        "payload.data.items[0].name must not contain control characters; \
         payload.data.items[0].price must have at most 2 decimal places; \
         payload.data.payments[0].amount must have at most 2 decimal places"
    );
    for value in ["TAJNA", "1.234", "7.777"] {
        assert!(!text.contains(value), "{value} leaked: {text}");
    }
}

#[test]
fn test_print_copy_problems() {
    let copy = FiscalOp::PrintCopy;
    let p = "payload";
    let one_way = format!(
        "{p} must name the receipt exactly one way: externalId, id, or year + month + receiptNumber"
    );
    assert_eq!(problems(copy, &json!({"cashRegisterCode": ORP})), one_way);
    assert_eq!(
        problems(
            copy,
            &json!({"cashRegisterCode": ORP, "externalId": "a", "id": "2d419057-807c-41be-a5d6-aafea8eba13e"})
        ),
        one_way
    );
    assert_eq!(
        problems(
            copy,
            &json!({"cashRegisterCode": ORP, "externalId": "a", "year": 2026})
        ),
        one_way
    );
    assert_eq!(
        problems(copy, &json!({"externalId": "a"})),
        format!("{p}.cashRegisterCode is required")
    );
    assert_eq!(
        problems(
            copy,
            &json!({"cashRegisterCode": ORP, "externalId": "e".repeat(129)})
        ),
        format!("{p}.externalId must have at most 128 characters")
    );
    assert_eq!(
        problems(copy, &json!({"cashRegisterCode": ORP, "externalId": null})),
        format!("{p}.externalId is required")
    );
    assert_eq!(
        problems(copy, &json!({"cashRegisterCode": ORP, "id": "42"})),
        format!("{p}.id must be the Portos request id (a UUID)")
    );
    assert_eq!(
        problems(copy, &json!({"cashRegisterCode": ORP, "id": 42})),
        format!("{p}.id must be the Portos request id (a UUID)")
    );
    assert_eq!(
        problems(
            copy,
            &json!({"cashRegisterCode": ORP, "year": 2018, "month": 13, "receiptNumber": 0})
        ),
        "payload.year must be a whole number between 2019 and 9999; \
         payload.month must be a whole number between 1 and 12; \
         payload.receiptNumber must be a whole number between 1 and 4294967295"
    );
    assert_eq!(
        problems(
            copy,
            &json!({"cashRegisterCode": ORP, "month": 10, "receiptNumber": 4_294_967_296_u64})
        ),
        "payload.year must be a whole number between 2019 and 9999; \
         payload.receiptNumber must be a whole number between 1 and 4294967295"
    );
    check_payload(copy, &json!({"cashRegisterCode": ORP, "year": 9999, "month": 1, "receiptNumber": 4_294_967_295_u64}))
        .expect("upper bounds");
    check_payload(
        copy,
        &json!({"cashRegisterCode": ORP, "year": 2019, "month": 12, "receiptNumber": 1}),
    )
    .expect("lower bounds");
    assert_eq!(
        problems(
            copy,
            &json!({"cashRegisterCode": ORP, "externalId": "a", "copies": 2})
        ),
        format!("{p}.copies is not allowed")
    );
}

#[test]
fn test_open_drawer_problems() {
    assert_eq!(
        problems(FiscalOp::OpenDrawer, &json!({"pin": "Pin3"})),
        "payload.pin must be one of Pin2, Pin5"
    );
    assert_eq!(
        problems(FiscalOp::OpenDrawer, &json!({"amount": 1})),
        "payload.amount is not allowed"
    );
}

#[test]
fn test_cash_register_code_of_each_op() {
    assert_eq!(cash_register_code(CASH, &sale()), Some(ORP));
    assert_eq!(
        cash_register_code(
            DEPOSIT,
            &json!({"data": {"cashRegisterCode": "8881234567890000"}})
        ),
        Some("8881234567890000")
    );
    assert_eq!(
        cash_register_code(FiscalOp::PrintCopy, &json!({"cashRegisterCode": ORP})),
        Some(ORP)
    );
    assert_eq!(
        cash_register_code(FiscalOp::OpenDrawer, &json!({"cashRegisterCode": ORP})),
        None
    );
    assert_eq!(cash_register_code(CASH, &json!({"data": {}})), None);
}

#[test]
fn test_register_body_uses_devbridges_external_id() {
    let body = register_body(&sale(), "dbj-3-u1");
    assert_eq!(body["request"]["externalId"], "dbj-3-u1");
    assert_eq!(body["request"]["data"], sale()["data"]);
    assert_eq!(
        body["print"],
        json!({"printerName": "pos", "options": {"OpenDrawer": true}})
    );
    assert_eq!(body.as_object().unwrap().len(), 2);
    assert_eq!(body["request"].as_object().unwrap().len(), 2);

    let no_print = json!({"data": {"cashRegisterCode": ORP, "amount": 5}});
    let body = register_body(&no_print, "dbj-4-u2");
    assert_eq!(
        body,
        json!({"request": {"data": {"cashRegisterCode": ORP, "amount": 5}, "externalId": "dbj-4-u2"}})
    );
    let null_print = json!({"data": {"amount": 5}, "print": null});
    assert!(register_body(&null_print, "x").get("print").is_none());
}

#[test]
fn test_copy_and_drawer_queries() {
    let q = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    };
    assert_eq!(
        copy_query(&json!({"cashRegisterCode": ORP, "externalId": "dbj-3-u"})),
        q(&[("CashRegisterCode", ORP), ("ExternalId", "dbj-3-u")])
    );
    assert_eq!(
        copy_query(
            &json!({"receiptNumber": 42, "month": 10, "year": 2026, "cashRegisterCode": ORP})
        ),
        q(&[
            ("CashRegisterCode", ORP),
            ("Year", "2026"),
            ("Month", "10"),
            ("ReceiptNumber", "42")
        ])
    );
    assert_eq!(
        copy_query(
            &json!({"cashRegisterCode": ORP, "id": "2d419057-807c-41be-a5d6-aafea8eba13e", "x": true})
        ),
        q(&[
            ("CashRegisterCode", ORP),
            ("Id", "2d419057-807c-41be-a5d6-aafea8eba13e")
        ])
    );
    assert_eq!(copy_query(&json!({"cashRegisterCode": null})), q(&[]));
    assert_eq!(drawer_query(&json!({"pin": "Pin2"})), q(&[("pin", "Pin2")]));
    assert_eq!(drawer_query(&json!({})), q(&[]));
    assert_eq!(drawer_query(&json!({"pin": 2})), q(&[]));
}

// ── Odoo's golden documents (#110) ─────────────────────────────────────────
// The receipt documents Odoo really composes (odoo-erp #9243 sale, #9419 its
// return), byte-for-byte from odoo-erp — source commits in
// testdata/odoo/README.md. odoo-erp checks the same files against its MIRROR
// of this checker; here the REAL checker reads them, so a drift on either side
// turns one of the two parity tests red.

const ODOO_SALE_9243: &str =
    include_str!("testdata/odoo/devbridge_cash_register_receipt_9243.json");
const ODOO_RETURN_9419: &str =
    include_str!("testdata/odoo/devbridge_cash_register_return_9419.json");

fn odoo_golden(text: &str) -> Value {
    serde_json::from_str(text).expect("a golden Odoo document is JSON")
}

#[test]
fn test_odoo_golden_receipts_pass_the_real_checker() {
    let op = FiscalOp::for_job_type("fiscal.cash_register").expect("the job type Odoo sends");
    assert_eq!(op, CASH);
    for (name, text) in [
        ("9243 sale", ODOO_SALE_9243),
        ("9419 return", ODOO_RETURN_9419),
    ] {
        let doc = odoo_golden(text);
        assert_eq!(check_payload(op, &doc), Ok(()), "{name}");
        assert_eq!(cash_register_code(op, &doc), Some(ORP), "{name}");
        // what goes to Portos: Odoo's data and print context unchanged, the
        // externalId always devbridge's own
        let ext = external_id(3, "odoo-golden");
        let body = register_body(&doc, &ext);
        assert_eq!(body["request"]["data"], doc["data"], "{name}");
        assert_eq!(body["request"]["externalId"], ext.as_str(), "{name}");
        assert_eq!(body["print"], json!({"printerName": "pos"}), "{name}");
        assert_eq!(body.as_object().unwrap().len(), 2, "{name}");
    }
}

/// The golden files exercise the rules this parity test relies on: the sale
/// pays with a negative change line, the return names the original receipt.
#[test]
fn test_odoo_golden_receipts_carry_what_the_contract_is_about() {
    let sale = odoo_golden(ODOO_SALE_9243);
    let payments = sale["data"]["payments"].as_array().unwrap();
    assert!(
        payments.iter().any(|p| p["amount"].as_f64().unwrap() < 0.0),
        "the 9243 sale has a negative payment line (the change)"
    );
    let ret = odoo_golden(ODOO_RETURN_9419);
    for item in ret["data"]["items"].as_array().unwrap() {
        assert_eq!(item["type"], "Returned");
        assert!(
            item["referenceReceiptId"]
                .as_str()
                .is_some_and(|r| !r.is_empty())
        );
        assert!(item["price"].as_f64().unwrap() < 0.0);
        assert!(item["quantity"]["amount"].as_f64().unwrap() > 0.0);
    }
    assert!(ret["data"]["payments"][0]["amount"].as_f64().unwrap() < 0.0);
}

/// Negative controls: the parity test CAN fail — one change in a copy of a
/// golden document, and the real checker names it.
#[test]
fn test_a_drifted_odoo_document_is_refused() {
    let i0 = "payload.data.items[0]";
    let mut renamed = odoo_golden(ODOO_RETURN_9419);
    let item = renamed["data"]["items"][0].as_object_mut().unwrap();
    let reference = item.remove("referenceReceiptId").unwrap();
    item.insert("referenceReceiptID".into(), reference);
    assert_eq!(
        problems(CASH, &renamed),
        format!("{i0}.referenceReceiptID is not allowed")
    );
    let mut wrong_type = odoo_golden(ODOO_RETURN_9419);
    wrong_type["data"]["items"][0]["type"] = json!("Return");
    assert_eq!(
        problems(CASH, &wrong_type),
        format!("{i0}.type must be one of {}", ITEM_TYPES.join(", "))
    );
    let mut change = odoo_golden(ODOO_SALE_9243);
    change["data"]["payments"][1]["amount"] = json!(-10.695);
    assert_eq!(
        problems(CASH, &change),
        "payload.data.payments[1].amount must have at most 2 decimal places"
    );
    let mut no_qty = odoo_golden(ODOO_SALE_9243);
    no_qty["data"]["items"][1]["quantity"] = json!(0.25);
    assert_eq!(
        problems(CASH, &no_qty),
        "payload.data.items[1].quantity must be an object"
    );
    let mut printer = odoo_golden(ODOO_SALE_9243);
    printer["print"]["printerName"] = json!("thermal");
    assert_eq!(
        problems(CASH, &printer),
        "payload.print.printerName must be one of pos, pdf, email"
    );
}
