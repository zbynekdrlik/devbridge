//! Unit tests of the `fiscal.print_text` payload (#120): a text composed by
//! Odoo, printed through `printers/print`, never a receipt.

use super::*;
use serde_json::json;

const PRINT: FiscalOp = FiscalOp::PrintText;

fn text() -> Value {
    json!({
        "title": "PRIEBEŽNÁ UZÁVIERKA (X)",
        "lines": ["NEFIŠKÁLNY DOKLAD –", "", "Hotovosť: 12,40 €"],
        "cut": true
    })
}

fn problems(payload: &Value) -> String {
    check_payload(PRINT, payload).expect_err("must be refused")
}

#[test]
fn test_the_job_type_is_the_print_op() {
    assert_eq!(FiscalOp::for_job_type("fiscal.print_text"), Some(PRINT));
}

#[test]
fn test_a_valid_text_passes_at_every_limit() {
    check_payload(PRINT, &text()).expect("blank lines are allowed");
    let width = "ž".repeat(40);
    let lines = vec![width.clone(); 80];
    let at_limit = json!({"title": width, "lines": lines, "cut": false});
    check_payload(PRINT, &at_limit).expect("40 characters, 80 lines");
    check_payload(PRINT, &json!({"title": "X", "lines": ["a"], "cut": false})).expect("one line");
}

#[test]
fn test_every_receipt_key_is_refused() {
    for key in [
        "items",
        "payments",
        "receiptType",
        "cashRegisterCode",
        "data",
        "print",
        "other",
    ] {
        let mut payload = text();
        payload[key] = json!([]);
        assert_eq!(
            problems(&payload),
            format!("payload.{key} is not allowed"),
            "{key}"
        );
    }
}

#[test]
fn test_title_limits() {
    let cases = [
        (json!(null), "payload.title is required"),
        (json!(""), "payload.title must not be empty"),
        (json!("   "), "payload.title must not be empty"),
        (json!(7), "payload.title must be a string"),
        (
            json!("x".repeat(41)),
            "payload.title must have at most 40 characters",
        ),
        (
            json!("a\u{1a}pageBreak\u{1a}"),
            "payload.title must not contain control or format characters",
        ),
    ];
    for (title, want) in cases {
        let mut payload = text();
        payload["title"] = title;
        assert_eq!(problems(&payload), want);
    }
    let mut missing = text();
    missing.as_object_mut().unwrap().remove("title");
    assert_eq!(problems(&missing), "payload.title is required");
}

#[test]
fn test_lines_limits() {
    let cases = [
        (json!(null), "payload.lines is required"),
        (json!("a"), "payload.lines must be a list"),
        (json!([]), "payload.lines must not be empty"),
        (
            json!(vec!["a"; 81]),
            "payload.lines must have at most 80 items",
        ),
        (json!(["a", 1]), "payload.lines[1] must be a string"),
        (
            json!(["x".repeat(41)]),
            "payload.lines[0] must have at most 40 characters",
        ),
    ];
    for (lines, want) in cases {
        let mut payload = text();
        payload["lines"] = lines;
        assert_eq!(problems(&payload), want);
    }
    let mut missing = text();
    missing.as_object_mut().unwrap().remove("lines");
    assert_eq!(problems(&missing), "payload.lines is required");
}

#[test]
fn test_control_and_format_characters_are_refused_in_a_line() {
    for bad in [
        '\u{0}',
        '\n',
        '\u{1a}',
        '\u{1e}',
        '\u{1f}',
        '\u{7f}',
        '\u{9f}',
        '\u{ad}',
        '\u{600}',
        '\u{605}',
        '\u{61c}',
        '\u{6dd}',
        '\u{70f}',
        '\u{890}',
        '\u{891}',
        '\u{8e2}',
        '\u{180e}',
        '\u{200b}',
        '\u{200f}',
        '\u{202a}',
        '\u{202e}',
        '\u{2060}',
        '\u{2064}',
        '\u{2066}',
        '\u{206f}',
        '\u{feff}',
        '\u{fff9}',
        '\u{fffb}',
        '\u{110bd}',
        '\u{110cd}',
        '\u{13430}',
        '\u{1343f}',
        '\u{1bca0}',
        '\u{1bca3}',
        '\u{1d173}',
        '\u{1d17a}',
        '\u{e0001}',
        '\u{e0020}',
        '\u{e007f}',
    ] {
        let mut payload = text();
        payload["lines"] = json!([format!("a{bad}b")]);
        assert_eq!(
            problems(&payload),
            "payload.lines[0] must not contain control or format characters",
            "U+{:04X}",
            u32::from(bad)
        );
    }
    for fine in [
        '€', '–', 'ž', ' ', '\u{606}', '\u{2010}', '\u{2065}', '\u{fffc}',
    ] {
        let mut payload = text();
        payload["lines"] = json!([format!("a{fine}b")]);
        check_payload(PRINT, &payload).expect("printable");
    }
}

#[test]
fn test_cut_must_be_a_bool() {
    for cut in [json!(null), json!("true"), json!(1)] {
        let mut payload = text();
        payload["cut"] = cut;
        assert_eq!(problems(&payload), "payload.cut must be true or false");
    }
    let mut missing = text();
    missing.as_object_mut().unwrap().remove("cut");
    assert_eq!(problems(&missing), "payload.cut must be true or false");
}

#[test]
fn test_a_problem_never_quotes_the_text() {
    let mut payload = text();
    payload["lines"] = json!(["TAJNE\u{7}"]);
    assert!(!problems(&payload).contains("TAJNE"));
}

#[test]
fn test_the_body_is_the_rows_and_the_cutter_token() {
    assert_eq!(
        print_text_body(&text()),
        json!({
            "text": "PRIEBEŽNÁ UZÁVIERKA (X)\nNEFIŠKÁLNY DOKLAD –\n\nHotovosť: 12,40 €\u{1a}pageBreak\u{1a}",
            "contentFlags": []
        })
    );
    let mut uncut = text();
    uncut["cut"] = json!(false);
    let body = print_text_body(&uncut);
    assert_eq!(
        body["text"],
        "PRIEBEŽNÁ UZÁVIERKA (X)\nNEFIŠKÁLNY DOKLAD –\n\nHotovosť: 12,40 €"
    );
    assert!(body.get("cashRegisterCode").is_none());
}

#[test]
fn test_a_text_names_no_cash_register() {
    let payload = json!({"cashRegisterCode": "88812345678900001"});
    assert_eq!(cash_register_code(PRINT, &payload), None);
}
