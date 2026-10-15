//! A fake Portos eKasa API 7.0.4 (#100) — the swagger subset the executor
//! uses, with Portos' own rules: a receipt is stored BEFORE the answer, the
//! same externalId registered again returns the ORIGINAL receipt (vendor
//! docs §6 "Ochrana proti duplicitnému zaevidovaniu dokladu"), a lookup of an
//! unknown externalId answers 404 / code -104. Knobs script the failures:
//! a lost answer after storing (the crash between POST and response), 4xx /
//! 5xx answers, slow drawers, an offline product/info.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

/// How the next register POSTs are answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Register {
    /// Stored, 200 `isSuccessful: true` (online).
    Online,
    /// Stored, 202 `isSuccessful: null` (offline receipt).
    Offline,
    /// Stored, then no answer for `ms` (the response is lost).
    StoreThenHang(u64),
    /// Stored, then HTTP 500 code -502 (print failed after registration).
    StoreThenPrintError,
    /// NOT stored, answered with this status + body (validation, …).
    Refuse(u16, Value),
}

/// How drawer / copy requests are answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    /// 200 with `{flag: true}` after `ms` (a long `ms` = no answer).
    Ok(u64),
    /// This status + body.
    Answer(u16, Value),
}

#[derive(Debug)]
pub struct PortosFake {
    /// externalId → the stored `RegisterReceiptResultDTO`.
    pub receipts: BTreeMap<String, Value>,
    /// Every register POST: (receipt type path segment, body).
    pub posts: Vec<(String, Value)>,
    /// Every lookup's query.
    pub lookups: Vec<BTreeMap<String, String>>,
    /// Every unprocessed-list query.
    pub unprocessed_calls: Vec<BTreeMap<String, String>>,
    /// externalIds the unprocessed list shows.
    pub unprocessed: Vec<String>,
    pub drawer_calls: Vec<BTreeMap<String, String>>,
    pub copy_calls: Vec<BTreeMap<String, String>>,
    pub register: Register,
    pub drawer: Control,
    pub copy: Control,
    /// Answer every lookup with this status + body instead.
    pub lookup_answer: Option<(u16, Value)>,
    /// `product/info` answers 503 when false.
    pub info_ok: bool,
    pub printer: Value,
    pub next_receipt_number: i64,
}

impl Default for PortosFake {
    fn default() -> Self {
        Self {
            receipts: BTreeMap::new(),
            posts: Vec::new(),
            lookups: Vec::new(),
            unprocessed_calls: Vec::new(),
            unprocessed: Vec::new(),
            drawer_calls: Vec::new(),
            copy_calls: Vec::new(),
            register: Register::Online,
            drawer: Control::Ok(0),
            copy: Control::Ok(0),
            lookup_answer: None,
            info_ok: true,
            printer: json!({"state": "Ready", "paperState": "Ready"}),
            next_receipt_number: 1,
        }
    }
}

pub type PortosShared = Arc<Mutex<PortosFake>>;

pub fn new_portos() -> PortosShared {
    Arc::new(Mutex::new(PortosFake::default()))
}

impl PortosFake {
    /// A receipt as Portos stores it (`receipt_type`: `CashRegister`, …).
    pub fn receipt(
        &mut self,
        external_id: &str,
        receipt_type: &str,
        data: &Value,
        online: bool,
    ) -> Value {
        let number = self.next_receipt_number;
        self.next_receipt_number += 1;
        let field = |key: &str, default: Value| data.get(key).cloned().unwrap_or(default);
        let response = if online {
            json!({"data": {"id": format!("O-{number}")}, "processDate": "2026-10-15T10:00:01+02:00"})
        } else {
            Value::Null
        };
        let is_successful = if online { json!(true) } else { Value::Null };
        json!({
            "request": {
                "data": {
                    "receiptType": receipt_type,
                    "amount": field("amount", json!(2.58)),
                    "roundingAmount": field("roundingAmount", json!(0.0)),
                    "issueDate": "2026-10-15T10:00:00+02:00",
                    "receiptNumber": number,
                    "okp": format!("okp-{number}"),
                    "pkp": "pkp",
                    "items": field("items", Value::Null),
                    "cashRegisterCode": field("cashRegisterCode", Value::Null),
                },
                "id": format!("00000000-0000-0000-0000-{number:012}"),
                "externalId": external_id,
                "date": "2026-10-15T10:00:00+02:00",
                "sendingCount": 1
            },
            "response": response,
            "isSuccessful": is_successful,
            "error": null,
            "$type": "Receipt"
        })
    }

    /// A receipt registered by someone else (e.g. the offline path) under
    /// `external_id`.
    pub fn preregister(&mut self, external_id: &str) {
        let receipt = self.receipt(external_id, "CashRegister", &json!({}), true);
        self.receipts.insert(external_id.to_string(), receipt);
    }

    pub fn post_count(&self) -> usize {
        self.posts.len()
    }
}

fn problem(status: u16, code: i64, title: &str) -> Response {
    let body =
        json!({"traceId": null, "code": code, "title": title, "status": status, "detail": null});
    (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
}

fn answer(status: u16, body: Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
}

async fn lookup(
    State(st): State<PortosShared>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    let mut s = st.lock().unwrap();
    s.lookups.push(q.clone());
    if let Some((status, body)) = s.lookup_answer.clone() {
        return answer(status, body);
    }
    assert!(
        q.contains_key("CashRegisterCode"),
        "lookup without CashRegisterCode: {q:?}"
    );
    match q.get("ExternalId").and_then(|e| s.receipts.get(e)) {
        Some(receipt) => answer(200, receipt.clone()),
        None => problem(404, -104, "Doklad sa nenašiel"),
    }
}

async fn register(
    State(st): State<PortosShared>,
    Path(kind): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (mode, stored) = {
        let mut s = st.lock().unwrap();
        s.posts.push((kind.clone(), body.clone()));
        let mode = s.register.clone();
        let external_id = body["request"]["externalId"]
            .as_str()
            .expect("externalId")
            .to_string();
        if let Register::Refuse(status, refusal) = &mode {
            return answer(*status, refusal.clone());
        }
        // the same externalId again: the ORIGINAL registration comes back
        let stored = match s.receipts.get(&external_id) {
            Some(original) => original.clone(),
            None => {
                let online = mode != Register::Offline;
                let receipt_type = match kind.as_str() {
                    "cash_register" => "CashRegister",
                    "invalid" => "Invalid",
                    "deposit" => "Deposit",
                    "withdraw" => "Withdraw",
                    other => panic!("no such Portos receipt endpoint: {other}"),
                };
                let receipt =
                    s.receipt(&external_id, receipt_type, &body["request"]["data"], online);
                s.receipts.insert(external_id, receipt.clone());
                receipt
            }
        };
        (mode, stored)
    };
    match mode {
        Register::Online => answer(200, stored),
        Register::Offline => answer(202, stored),
        Register::StoreThenHang(ms) => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            answer(200, stored)
        }
        Register::StoreThenPrintError => problem(500, -502, "Tlač dokladu zlyhala"),
        Register::Refuse(..) => unreachable!(),
    }
}

async fn unprocessed(
    State(st): State<PortosShared>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    let mut s = st.lock().unwrap();
    s.unprocessed_calls.push(q);
    if s.unprocessed.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }
    let list: Vec<Value> = s
        .unprocessed
        .iter()
        .map(|e| json!({"request": {"id": "x", "externalId": e, "date": "2026-10-15T10:00:00+02:00", "sendingCount": 0},
                        "isSuccessful": null, "response": null, "error": null}))
        .collect();
    answer(200, Value::Array(list))
}

async fn control(
    st: &PortosShared,
    flag: &str,
    drawer: bool,
    q: BTreeMap<String, String>,
) -> Response {
    let mode = {
        let mut s = st.lock().unwrap();
        if drawer {
            s.drawer_calls.push(q);
            s.drawer.clone()
        } else {
            s.copy_calls.push(q);
            s.copy.clone()
        }
    };
    let done = || {
        let mut body = serde_json::Map::new();
        body.insert(flag.to_string(), json!(true));
        answer(200, Value::Object(body))
    };
    match mode {
        Control::Ok(ms) => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            done()
        }
        Control::Answer(status, body) => answer(status, body),
    }
}

async fn open_drawer(
    State(st): State<PortosShared>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    control(&st, "opened", true, q).await
}

async fn print_copy(
    State(st): State<PortosShared>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    control(&st, "printed", false, q).await
}

async fn product_info(State(st): State<PortosShared>) -> Response {
    if !st.lock().unwrap().info_ok {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    answer(
        200,
        json!({"vendorName": "Nine Digit, s.r.o.", "swid": "x",
        "api": {"name": "Portos eKasa", "version": "7.0.4"},
        "ppekk": {"name": "Portos eKasa", "version": "7.0.0"}, "chdu": {"name": "InMemory", "version": "1"}}),
    )
}

async fn printer_status(State(st): State<PortosShared>) -> Response {
    answer(200, st.lock().unwrap().printer.clone())
}

/// Start the fake; returns its base URL.
pub async fn start_portos(state: PortosShared) -> String {
    let app = Router::new()
        .route("/api/v1/requests/receipts/receipt", get(lookup))
        .route("/api/v1/requests/receipts/print_copy", post(print_copy))
        .route("/api/v1/requests/receipts/{kind}", post(register))
        .route("/api/v1/requests/unprocessed", get(unprocessed))
        .route("/api/v1/printers/open_drawer", post(open_drawer))
        .route("/api/v1/printers/status", get(printer_status))
        .route("/api/v1/product/info", get(product_info))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}
