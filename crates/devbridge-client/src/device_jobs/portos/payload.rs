//! The `fiscal.*` job payloads devbridge accepts (#100) and their mapping to
//! Portos eKasa API 7.0.4 requests — the contract is
//! `docs/device-jobs-fiscal.md`.
//!
//! A payload is checked BEFORE the job is journaled ([`check_payload`]): a
//! wrong shape, a number with more decimal places than eKasa keeps (price 2,
//! unit price 6, quantity 4 — the `ekasa-clientphp` rounding rules) or an
//! unknown key is refused `bad_payload` and Portos is never contacted.
//! Business rules (VAT rates, item signs, sums) stay Portos' own: it answers
//! a breach with HTTP 400 (nothing registered) and the job fails with
//! Portos' text.
//!
//! Problem texts name the field and the rule, never a value (receipt data
//! stays out of logs and acks).

use serde_json::{Map, Value, json};

/// Error code of a job whose payload devbridge refuses.
pub const CODE_BAD_PAYLOAD: &str = "bad_payload";

/// Largest absolute amount / price / quantity eKasa accepts.
const MAX_ABS_AMOUNT: f64 = 10_000_000.0;
/// Longest item name / payment name eKasa accepts.
const MAX_NAME_CHARS: usize = 255;
/// Longest externalId a `print_copy` may name.
const MAX_EXTERNAL_ID_CHARS: usize = 128;

/// A receipt Portos registers (one eKasa record each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptKind {
    CashRegister,
    Invalid,
    Deposit,
    Withdraw,
}

impl ReceiptKind {
    /// The register endpoint.
    pub fn path(self) -> &'static str {
        match self {
            Self::CashRegister => "/api/v1/requests/receipts/cash_register",
            Self::Invalid => "/api/v1/requests/receipts/invalid",
            Self::Deposit => "/api/v1/requests/receipts/deposit",
            Self::Withdraw => "/api/v1/requests/receipts/withdraw",
        }
    }

    /// Portos' `receiptType` of this kind.
    pub fn receipt_type(self) -> &'static str {
        match self {
            Self::CashRegister => "CashRegister",
            Self::Invalid => "Invalid",
            Self::Deposit => "Deposit",
            Self::Withdraw => "Withdraw",
        }
    }
}

/// What a `fiscal.*` job asks Portos to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiscalOp {
    /// Register a receipt — exactly once, keyed by the devbridge externalId.
    Register(ReceiptKind),
    /// Print a copy of a registered receipt (no fiscal record).
    PrintCopy,
    /// Open the cash drawer (no fiscal record).
    OpenDrawer,
}

impl FiscalOp {
    pub fn for_job_type(job_type: &str) -> Option<Self> {
        Some(match job_type {
            "fiscal.cash_register" => Self::Register(ReceiptKind::CashRegister),
            "fiscal.invalid" => Self::Register(ReceiptKind::Invalid),
            "fiscal.deposit" => Self::Register(ReceiptKind::Deposit),
            "fiscal.withdraw" => Self::Register(ReceiptKind::Withdraw),
            "fiscal.print_copy" => Self::PrintCopy,
            "fiscal.open_drawer" => Self::OpenDrawer,
            _ => return None,
        })
    }
}

/// The idempotency key devbridge gives every receipt of a job:
/// `dbj-<kasa_id>-<client_uuid>`. Every path that may register the receipt of
/// this operation uses the same key, so Portos can be asked about it.
pub fn external_id(kasa_id: i64, client_uuid: &str) -> String {
    format!("dbj-{kasa_id}-{client_uuid}")
}

/// The Odoo contract's `client_uuid`: `[A-Za-z0-9_-]{1,64}`.
pub fn client_uuid_ok(client_uuid: &str) -> bool {
    (1..=64).contains(&client_uuid.len())
        && client_uuid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `true` when `x` has at most `places` decimal places as written (its
/// shortest round-trip form): `2.58` has 2, `0.1 + 0.2` has 17.
pub fn has_at_most_decimals(x: f64, places: usize) -> bool {
    x.is_finite() && format!("{x:.places$}").parse::<f64>() == Ok(x)
}

/// `^[0-9]{16,17}$` — the ORP code of a cash register.
pub fn cash_register_code_ok(code: &str) -> bool {
    (16..=17).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_digit())
}

/// `8-4-4-4-12` hex digits.
fn uuid_ok(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Collects the payload's problems (field path + rule, never a value).
struct Checker {
    problems: Vec<String>,
}

impl Checker {
    fn new() -> Self {
        Self {
            problems: Vec::new(),
        }
    }

    fn fail(&mut self, path: &str, rule: &str) {
        self.problems.push(format!("{path} {rule}"));
    }

    fn object<'a>(&mut self, v: &'a Value, path: &str) -> Option<&'a Map<String, Value>> {
        let obj = v.as_object();
        if obj.is_none() {
            self.fail(path, "must be an object");
        }
        obj
    }

    fn only_keys(&mut self, obj: &Map<String, Value>, path: &str, allowed: &[&str]) {
        for key in obj.keys() {
            if !allowed.contains(&key.as_str()) {
                self.fail(&format!("{path}.{key}"), "is not allowed");
            }
        }
    }

    /// A string of `1..=max` characters (`printable`: no control characters).
    fn string(&mut self, obj: &Map<String, Value>, path: &str, key: &str, rule: Text) {
        let field = format!("{path}.{key}");
        match obj.get(key) {
            None | Some(Value::Null) if !rule.required => {}
            None | Some(Value::Null) => self.fail(&field, "is required"),
            Some(Value::String(s)) => {
                let chars = s.chars().count();
                if chars == 0 || s.trim().is_empty() {
                    self.fail(&field, "must not be empty");
                } else if chars > rule.max {
                    self.fail(
                        &field,
                        &format!("must have at most {} characters", rule.max),
                    );
                } else if rule.printable && s.chars().any(char::is_control) {
                    self.fail(&field, "must not contain control characters");
                }
            }
            Some(_) => self.fail(&field, "must be a string"),
        }
    }

    /// A number in `-MAX..=MAX` (or `0..=MAX`) with at most `places` decimals.
    fn number(&mut self, obj: &Map<String, Value>, path: &str, key: &str, rule: Num) {
        let field = format!("{path}.{key}");
        match obj.get(key) {
            None if !rule.required => {}
            None => self.fail(&field, "is required"),
            Some(Value::Null) if rule.nullable => {}
            Some(Value::Number(n)) => match n.as_f64() {
                Some(x) if !has_at_most_decimals(x, rule.places) => self.fail(
                    &field,
                    &format!("must have at most {} decimal places", rule.places),
                ),
                Some(x) if x.abs() > MAX_ABS_AMOUNT || (rule.non_negative && x < 0.0) => {
                    let low = if rule.non_negative {
                        0.0
                    } else {
                        -MAX_ABS_AMOUNT
                    };
                    self.fail(
                        &field,
                        &format!("must be between {low} and {MAX_ABS_AMOUNT}"),
                    );
                }
                Some(_) => {}
                None => self.fail(&field, "must be a finite number"),
            },
            Some(_) => self.fail(&field, "must be a number"),
        }
    }

    /// One of `allowed` (ASCII case-insensitive, as Portos reads it).
    fn one_of(
        &mut self,
        obj: &Map<String, Value>,
        path: &str,
        key: &str,
        allowed: &[&str],
        required: bool,
    ) {
        let field = format!("{path}.{key}");
        match obj.get(key) {
            None | Some(Value::Null) if !required => {}
            None | Some(Value::Null) => self.fail(&field, "is required"),
            Some(Value::String(s)) if allowed.iter().any(|a| a.eq_ignore_ascii_case(s)) => {}
            Some(_) => self.fail(&field, &format!("must be one of {}", allowed.join(", "))),
        }
    }

    /// An optional `{"id": string, "type": <allowed>}` (customer / seller).
    fn party(&mut self, obj: &Map<String, Value>, path: &str, key: &str, types: &[&str]) {
        let field = format!("{path}.{key}");
        match obj.get(key) {
            None | Some(Value::Null) => {}
            Some(v) => {
                if let Some(party) = self.object(v, &field) {
                    self.only_keys(party, &field, &["id", "type"]);
                    self.string(party, &field, "id", Text::required(64));
                    self.one_of(party, &field, "type", types, true);
                }
            }
        }
    }

    fn cash_register_code(&mut self, obj: &Map<String, Value>, path: &str, key: &str) {
        let field = format!("{path}.{key}");
        match obj.get(key) {
            Some(Value::String(code)) if cash_register_code_ok(code) => {}
            None | Some(Value::Null) => self.fail(&field, "is required"),
            Some(_) => self.fail(&field, "must be the 16-17 digit cash register (ORP) code"),
        }
    }

    fn finish(self) -> Result<(), String> {
        if self.problems.is_empty() {
            Ok(())
        } else {
            Err(self.problems.join("; "))
        }
    }
}

/// String rule.
#[derive(Clone, Copy)]
struct Text {
    required: bool,
    max: usize,
    printable: bool,
}

impl Text {
    fn required(max: usize) -> Self {
        Self {
            required: true,
            max,
            printable: true,
        }
    }
    fn optional(max: usize) -> Self {
        Self {
            required: false,
            max,
            printable: true,
        }
    }
    /// Header / footer / description: Portos print tokens use control
    /// characters (`\u001a…`), so they are allowed there.
    fn optional_tokens(max: usize) -> Self {
        Self {
            required: false,
            max,
            printable: false,
        }
    }
}

/// Number rule.
#[derive(Clone, Copy)]
struct Num {
    required: bool,
    nullable: bool,
    places: usize,
    non_negative: bool,
}

impl Num {
    fn required(places: usize) -> Self {
        Self {
            required: true,
            nullable: false,
            places,
            non_negative: false,
        }
    }
    fn optional(places: usize) -> Self {
        Self {
            required: false,
            ..Self::required(places)
        }
    }
}

const ITEM_TYPES: &[&str] = &[
    "Positive",
    "ReturnedContainer",
    "Returned",
    "Correction",
    "Discount",
    "Advance",
    "Voucher",
];
const TAX_FREE_REASONS: &[&str] = &[
    "VATReverseCharge",
    "VATExemptionGood",
    "TravelAgency",
    "UsedGood",
    "Artwork",
    "CollectiblesAndAntiques",
];
const CUSTOMER_ID_TYPES: &[&str] = &["ICO", "DIC", "ICDPH", "Other"];
const SELLER_ID_TYPES: &[&str] = &["DIC", "ICDPH"];
const PRINTER_NAMES: &[&str] = &["pos", "pdf", "email"];
const DRAWER_PINS: &[&str] = &["Pin2", "Pin5"];
/// Longest header / footer / item description devbridge passes on.
const MAX_FREE_TEXT_CHARS: usize = 2000;

/// Check a job payload for `op`. `Err` = the problems, joined with `; `.
pub fn check_payload(op: FiscalOp, payload: &Value) -> Result<(), String> {
    let mut c = Checker::new();
    if let Some(obj) = c.object(payload, "payload") {
        match op {
            FiscalOp::Register(kind) => check_register(&mut c, obj, kind),
            FiscalOp::PrintCopy => check_print_copy(&mut c, obj),
            FiscalOp::OpenDrawer => {
                c.only_keys(obj, "payload", &["pin"]);
                c.one_of(obj, "payload", "pin", DRAWER_PINS, false);
            }
        }
    }
    c.finish()
}

fn check_register(c: &mut Checker, obj: &Map<String, Value>, kind: ReceiptKind) {
    c.only_keys(obj, "payload", &["data", "print"]);
    match obj.get("print") {
        None | Some(Value::Null) => {}
        Some(print) => {
            if let Some(p) = c.object(print, "payload.print") {
                c.only_keys(p, "payload.print", &["printerName", "options"]);
                c.one_of(p, "payload.print", "printerName", PRINTER_NAMES, true);
                if let Some(options) = p.get("options").filter(|o| !o.is_null()) {
                    c.object(options, "payload.print.options");
                }
            }
        }
    }
    let Some(data) = obj.get("data") else {
        c.fail("payload.data", "is required");
        return;
    };
    let Some(data) = c.object(data, "payload.data") else {
        return;
    };
    let path = "payload.data";
    c.cash_register_code(data, path, "cashRegisterCode");
    c.string(
        data,
        path,
        "headerText",
        Text::optional_tokens(MAX_FREE_TEXT_CHARS),
    );
    c.string(
        data,
        path,
        "footerText",
        Text::optional_tokens(MAX_FREE_TEXT_CHARS),
    );
    let wrong_type = data.get("receiptType").is_some_and(|t| {
        !t.is_null()
            && !t
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(kind.receipt_type()))
    });
    if wrong_type {
        c.fail(
            &format!("{path}.receiptType"),
            &format!("must be {} (or left out)", kind.receipt_type()),
        );
    }
    match kind {
        ReceiptKind::Deposit | ReceiptKind::Withdraw => {
            c.only_keys(
                data,
                path,
                &[
                    "amount",
                    "receiptType",
                    "headerText",
                    "footerText",
                    "cashRegisterCode",
                ],
            );
            c.number(data, path, "amount", Num::required(2));
        }
        ReceiptKind::CashRegister | ReceiptKind::Invalid => {
            let mut allowed = vec![
                "items",
                "payments",
                "roundingAmount",
                "receiptType",
                "headerText",
                "footerText",
                "cashRegisterCode",
            ];
            if kind == ReceiptKind::CashRegister {
                allowed.push("customer");
                c.party(data, path, "customer", CUSTOMER_ID_TYPES);
            }
            c.only_keys(data, path, &allowed);
            c.number(data, path, "roundingAmount", Num::optional(2));
            check_items(c, data, path);
            check_payments(c, data, path);
        }
    }
}

fn check_items(c: &mut Checker, data: &Map<String, Value>, path: &str) {
    let field = format!("{path}.items");
    let items = match data.get("items") {
        Some(Value::Array(items)) if !items.is_empty() => items,
        Some(Value::Array(_)) => return c.fail(&field, "must not be empty"),
        None | Some(Value::Null) => return c.fail(&field, "is required"),
        Some(_) => return c.fail(&field, "must be a list"),
    };
    for (i, item) in items.iter().enumerate() {
        let ipath = format!("{field}[{i}]");
        let Some(it) = c.object(item, &ipath) else {
            continue;
        };
        c.only_keys(
            it,
            &ipath,
            &[
                "type",
                "name",
                "price",
                "unitPrice",
                "quantity",
                "referenceReceiptId",
                "vatRate",
                "specialRegulation",
                "voucherNumber",
                "seller",
                "description",
            ],
        );
        c.one_of(it, &ipath, "type", ITEM_TYPES, true);
        c.string(it, &ipath, "name", Text::required(MAX_NAME_CHARS));
        c.number(it, &ipath, "price", Num::required(2));
        c.number(it, &ipath, "unitPrice", Num::required(6));
        c.number(
            it,
            &ipath,
            "vatRate",
            Num {
                nullable: true,
                ..Num::optional(2)
            },
        );
        c.string(
            it,
            &ipath,
            "referenceReceiptId",
            Text::optional(MAX_NAME_CHARS),
        );
        c.one_of(it, &ipath, "specialRegulation", TAX_FREE_REASONS, false);
        c.string(it, &ipath, "voucherNumber", Text::optional(50));
        c.string(
            it,
            &ipath,
            "description",
            Text::optional_tokens(MAX_FREE_TEXT_CHARS),
        );
        c.party(it, &ipath, "seller", SELLER_ID_TYPES);
        let qpath = format!("{ipath}.quantity");
        match it.get("quantity") {
            None | Some(Value::Null) => c.fail(&qpath, "is required"),
            Some(q) => {
                if let Some(q) = c.object(q, &qpath) {
                    c.only_keys(q, &qpath, &["amount", "unit"]);
                    c.number(
                        q,
                        &qpath,
                        "amount",
                        Num {
                            non_negative: true,
                            ..Num::required(4)
                        },
                    );
                    c.string(q, &qpath, "unit", Text::optional(3));
                }
            }
        }
    }
}

fn check_payments(c: &mut Checker, data: &Map<String, Value>, path: &str) {
    let field = format!("{path}.payments");
    let payments = match data.get("payments") {
        None | Some(Value::Null) => return,
        Some(Value::Array(p)) => p,
        Some(_) => return c.fail(&field, "must be a list"),
    };
    for (i, payment) in payments.iter().enumerate() {
        let ppath = format!("{field}[{i}]");
        if let Some(p) = c.object(payment, &ppath) {
            c.only_keys(p, &ppath, &["name", "amount"]);
            c.string(p, &ppath, "name", Text::required(MAX_NAME_CHARS));
            c.number(p, &ppath, "amount", Num::required(2));
        }
    }
}

fn check_print_copy(c: &mut Checker, obj: &Map<String, Value>) {
    let path = "payload";
    c.only_keys(
        obj,
        path,
        &[
            "cashRegisterCode",
            "externalId",
            "id",
            "year",
            "month",
            "receiptNumber",
        ],
    );
    c.cash_register_code(obj, path, "cashRegisterCode");
    let by_number = ["year", "month", "receiptNumber"]
        .iter()
        .filter(|k| obj.contains_key(**k))
        .count();
    let ways = usize::from(obj.contains_key("externalId"))
        + usize::from(obj.contains_key("id"))
        + usize::from(by_number > 0);
    if ways != 1 {
        c.fail(path, "must name the receipt exactly one way: externalId, id, or year + month + receiptNumber");
        return;
    }
    if obj.contains_key("externalId") {
        c.string(
            obj,
            path,
            "externalId",
            Text::required(MAX_EXTERNAL_ID_CHARS),
        );
    } else if obj.contains_key("id") {
        if !obj.get("id").and_then(Value::as_str).is_some_and(uuid_ok) {
            c.fail("payload.id", "must be the Portos request id (a UUID)");
        }
    } else {
        for (key, low, high) in [
            ("year", 2019, 9999),
            ("month", 1, 12),
            ("receiptNumber", 1, 4_294_967_295),
        ] {
            if !obj
                .get(key)
                .and_then(Value::as_i64)
                .is_some_and(|n| (low..=high).contains(&n))
            {
                c.fail(
                    &format!("payload.{key}"),
                    &format!("must be a whole number between {low} and {high}"),
                );
            }
        }
    }
}

/// The cash register (ORP) code a checked payload names.
pub fn cash_register_code(op: FiscalOp, payload: &Value) -> Option<&str> {
    match op {
        FiscalOp::Register(_) => payload.pointer("/data/cashRegisterCode"),
        FiscalOp::PrintCopy => payload.get("cashRegisterCode"),
        FiscalOp::OpenDrawer => None,
    }
    .and_then(Value::as_str)
}

/// The body of a register POST: `{"request": {"data", "externalId"},
/// "print"?}` — the externalId is ALWAYS devbridge's own.
pub fn register_body(payload: &Value, external_id: &str) -> Value {
    let mut body = json!({
        "request": {
            "data": payload.get("data").cloned().unwrap_or(Value::Null),
            "externalId": external_id,
        }
    });
    if let Some(print) = payload.get("print").filter(|p| !p.is_null()) {
        body["print"] = print.clone();
    }
    body
}

/// The query of a `print_copy` POST (Portos' own parameter names).
pub fn copy_query(payload: &Value) -> Vec<(String, String)> {
    [
        ("cashRegisterCode", "CashRegisterCode"),
        ("externalId", "ExternalId"),
        ("id", "Id"),
        ("year", "Year"),
        ("month", "Month"),
        ("receiptNumber", "ReceiptNumber"),
    ]
    .into_iter()
    .filter_map(|(key, param)| {
        let value = match payload.get(key)? {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return None,
        };
        Some((param.to_string(), value))
    })
    .collect()
}

/// The query of an `open_drawer` POST (`pin`, else Portos' configured pin).
pub fn drawer_query(payload: &Value) -> Vec<(String, String)> {
    payload
        .get("pin")
        .and_then(Value::as_str)
        .map(|pin| vec![("pin".to_string(), pin.to_string())])
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "payload_tests.rs"]
mod tests;
