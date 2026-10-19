//! Odoo print API client (#90 contract, stream david4 2026-09-28).
//!
//! All three endpoints are Odoo `type="json"` routes spoken through the
//! shared [`crate::odoo_rpc::OdooTransport`] (JSON-RPC 2.0 envelope, Bearer
//! key, both HTTP-200 error kinds — top-level `error` first, then
//! `result.error` text; #99 extracted it from here so the device-job source
//! uses the same transport). The API key is only ever put into the
//! `Authorization` header.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::odoo_rpc::OdooTransport;
pub use crate::odoo_rpc::{RpcError, envelope, parse_envelope};

/// Longest `error` text Odoo stores on a line (`error_text`, 200 chars).
pub const MAX_ACK_ERROR_CHARS: usize = 200;

/// Whole-request timeout of every label call.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Cut an ack error text to Odoo's 200-character field (char boundary safe).
pub fn truncate_ack_error(text: &str) -> String {
    text.chars().take(MAX_ACK_ERROR_CHARS).collect()
}

/// One label line of `/food/print/next`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NextLine {
    pub line_id: i64,
    #[serde(default)]
    pub sequence: i64,
    #[serde(default, deserialize_with = "de_text")]
    pub product_name: String,
    #[serde(deserialize_with = "de_qty")]
    pub print_qty: i64,
    #[serde(default, deserialize_with = "de_text")]
    pub best_before: String,
    #[serde(default, deserialize_with = "de_text")]
    pub label_png_base64: String,
    #[serde(default, deserialize_with = "de_text")]
    pub line_type: String,
}

/// A `/next` line that does not match the contract (missing / wrong-typed
/// field). It is never printed; with a `line_id` it is acked as an error so
/// one bad line cannot block the whole batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidLine {
    pub line_id: Option<i64>,
    pub reason: String,
}

/// One optional number of a batch (#108). Odoo sends `false` for an empty
/// field, and an unset `fields.Float` reads as `0.0`, so a missing key,
/// `false`, `null` and `0` are all `Absent` (no label is 0 mm, no printer
/// 0 dpi). A value that is not a JSON number is kept as `NotANumber` (its
/// JSON text): the batch is then rejected line by line with a `size` ack
/// instead of failing the whole poll, which would leave the batch blocked
/// without a visible reason.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum BatchNumber {
    #[default]
    Absent,
    Number(f64),
    NotANumber(String),
}

/// The label roll of the batch's format, as Odoo sent it (#108): the crate
/// QR labels are 50 × 30 mm on the same printer as the 72.7 × 110.1 mm
/// bakery labels. All `Absent` = the `[client.odoo]` roll.
/// [`super::label_size::geometry_for`] validates it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BatchLabelSize {
    pub width_mm: BatchNumber,
    pub height_mm: BatchNumber,
    pub dpi: BatchNumber,
}

/// `/food/print/next` result: the oldest printing batch, or no lines.
#[derive(Debug, Clone, PartialEq)]
pub struct NextBatch {
    pub batch_id: Option<i64>,
    pub production_date: String,
    pub lines: Vec<NextLine>,
    pub invalid: Vec<InvalidLine>,
    /// Batch-level `label_width_mm` / `label_height_mm` / `dpi` (#108).
    pub label_size: BatchLabelSize,
}

/// Wire shape of the `/next` result; lines are parsed one by one.
#[derive(Deserialize)]
struct RawBatch {
    #[serde(default, deserialize_with = "de_opt_id")]
    batch_id: Option<i64>,
    #[serde(default, deserialize_with = "de_text")]
    production_date: String,
    #[serde(default)]
    lines: Vec<Value>,
    #[serde(default, deserialize_with = "de_batch_number")]
    label_width_mm: BatchNumber,
    #[serde(default, deserialize_with = "de_batch_number")]
    label_height_mm: BatchNumber,
    #[serde(default, deserialize_with = "de_batch_number")]
    dpi: BatchNumber,
}

impl NextBatch {
    /// Lines in print order (`sequence`; Odoo already sorts, the stable sort
    /// keeps its order for equal sequences).
    pub fn lines_in_order(&self) -> Vec<&NextLine> {
        let mut v: Vec<&NextLine> = self.lines.iter().collect();
        v.sort_by_key(|l| l.sequence);
        v
    }
}

fn de_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::String(s) => s,
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    })
}

/// An Odoo id: a number, or `false`/`null` (Odoo's "empty") as `None`.
fn de_opt_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    match Value::deserialize(d)? {
        Value::Null | Value::Bool(false) => Ok(None),
        Value::Number(n) if n.is_i64() => Ok(n.as_i64()),
        other => Err(serde::de::Error::custom(format!(
            "expected an integer id or false, got {other}"
        ))),
    }
}

/// A batch-level number (#108): absent / `false` / `null` / `0` (Odoo's
/// unset float) → `Absent`, another JSON number → `Number`, anything else →
/// `NotANumber` (never a parse error).
pub fn batch_number(value: Value) -> BatchNumber {
    match value {
        Value::Null | Value::Bool(false) => BatchNumber::Absent,
        Value::Number(n) => match n.as_f64() {
            Some(f) => {
                if f == 0.0 {
                    BatchNumber::Absent
                } else {
                    BatchNumber::Number(f)
                }
            }
            None => BatchNumber::NotANumber(n.to_string()),
        },
        other => BatchNumber::NotANumber(other.to_string()),
    }
}

fn de_batch_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BatchNumber, D::Error> {
    Ok(batch_number(Value::deserialize(d)?))
}

/// `print_qty` as an integer; Odoo may send `40` or `40.0`.
fn de_qty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    if let Some(i) = v.as_i64() {
        return Ok(i);
    }
    match v.as_f64() {
        Some(f) if f.fract() == 0.0 => Ok(f as i64),
        _ => Err(serde::de::Error::custom(format!(
            "print_qty must be a whole number, got {v}"
        ))),
    }
}

/// Parse the `/next` result object.
pub fn parse_next(result: Value) -> Result<NextBatch, RpcError> {
    // serde would also read a JSON array positionally into the struct.
    if !result.is_object() {
        return Err(RpcError::Protocol(format!(
            "/food/print/next result is not an object: {result}"
        )));
    }
    let raw: RawBatch = serde_json::from_value(result)
        .map_err(|e| RpcError::Protocol(format!("/food/print/next result: {e}")))?;
    let mut lines = Vec::with_capacity(raw.lines.len());
    let mut invalid = Vec::new();
    for value in raw.lines {
        let line_id = value.get("line_id").and_then(Value::as_i64);
        match serde_json::from_value::<NextLine>(value) {
            Ok(line) => lines.push(line),
            Err(e) => invalid.push(InvalidLine {
                line_id,
                reason: format!("invalid line: {e}"),
            }),
        }
    }
    if (!lines.is_empty() || !invalid.is_empty()) && raw.batch_id.is_none() {
        return Err(RpcError::Protocol(
            "/food/print/next returned lines without batch_id".into(),
        ));
    }
    Ok(NextBatch {
        batch_id: raw.batch_id,
        production_date: raw.production_date,
        lines,
        invalid,
        label_size: BatchLabelSize {
            width_mm: raw.label_width_mm,
            height_mm: raw.label_height_mm,
            dpi: raw.dpi,
        },
    })
}

/// `/food/print/ack` params: `printed_qty` + `error: null` for a printed
/// line, `printed_qty = 0` + the reason otherwise.
pub fn ack_params(line_id: i64, printed_qty: i64, error: Option<&str>) -> Value {
    json!({
        "line_id": line_id,
        "printed_qty": printed_qty,
        "error": error.map(truncate_ack_error),
    })
}

/// Accepted ack; `duplicate` = Odoo already had this exact final state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckAccepted {
    pub duplicate: bool,
}

pub fn parse_ack(result: &Value) -> Result<AckAccepted, RpcError> {
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(RpcError::Protocol(format!(
            "/food/print/ack result without ok:true: {result}"
        )));
    }
    Ok(AckAccepted {
        duplicate: result.get("duplicate").and_then(Value::as_bool) == Some(true),
    })
}

/// `/food/print/heartbeat` params. `client_version` / `last_job_id` /
/// `last_result` are optional on the Odoo side and ignored until odoo-erp
/// #6665 part B stores them; `paper_size` (#108) lands in the route's
/// `**kwargs` and is ignored until Odoo stores it too.
#[derive(Debug, Clone, PartialEq)]
pub struct Heartbeat {
    pub printer_name: String,
    pub paper_status: String,
    pub error_status: String,
    pub client_version: String,
    pub last_job_id: Option<i64>,
    pub last_result: String,
    /// The roll the printer is assumed to hold: `SIZE` of the last batch
    /// document the client printed or tried to print (Odoo's roll interlock
    /// named that roll), the `[client.odoo]` roll before the first one
    /// (`"50x30 mm"`, `"72.7x110.1 mm"`).
    pub paper_size: String,
}

impl Heartbeat {
    pub fn params(&self) -> Value {
        json!({
            "printer_name": self.printer_name,
            "paper_status": self.paper_status,
            "error_status": self.error_status,
            "client_version": self.client_version,
            "last_job_id": self.last_job_id,
            "last_result": self.last_result,
            "paper_size": self.paper_size,
        })
    }
}

/// Async HTTP client for the three endpoints.
pub struct OdooRpc {
    transport: OdooTransport,
}

impl OdooRpc {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, RpcError> {
        Ok(Self {
            transport: OdooTransport::new(base_url, api_key, CALL_TIMEOUT)?,
        })
    }

    /// Full URL of an endpoint path (`/food/print/next`).
    pub fn endpoint(&self, path: &str) -> String {
        self.transport.endpoint(path)
    }

    async fn call(&self, path: &str, params: Value) -> Result<Value, RpcError> {
        self.transport.call(path, params).await
    }

    pub async fn next(&self) -> Result<NextBatch, RpcError> {
        parse_next(self.call("/food/print/next", json!({})).await?)
    }

    pub async fn ack(
        &self,
        line_id: i64,
        printed_qty: i64,
        error: Option<&str>,
    ) -> Result<AckAccepted, RpcError> {
        let result = self
            .call("/food/print/ack", ack_params(line_id, printed_qty, error))
            .await?;
        parse_ack(&result)
    }

    pub async fn heartbeat(&self, hb: &Heartbeat) -> Result<Value, RpcError> {
        self.call("/food/print/heartbeat", hb.params()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_next_contract_example_and_empty_queue() {
        let result = json!({"batch_id": 12, "production_date": "2026-09-29",
        "lines": [
            {"line_id": 346, "sequence": 20, "product_name": false, "print_qty": 1,
             "best_before": false, "label_png_base64": "", "line_type": "separator"},
            {"line_id": 345, "sequence": 10, "product_name": "Chlieb spišský 450g rezaný",
             "print_qty": 40.0, "best_before": "03.10.2026",
             "label_png_base64": "iVBOR", "line_type": "product"}
        ]});
        let b = parse_next(result).unwrap();
        assert_eq!(b.batch_id, Some(12));
        assert_eq!(b.production_date, "2026-09-29");
        let ordered: Vec<i64> = b.lines_in_order().iter().map(|l| l.line_id).collect();
        assert_eq!(ordered, vec![345, 346]);
        let first = b.lines_in_order()[0].clone();
        assert_eq!(first.print_qty, 40);
        assert_eq!(first.product_name, "Chlieb spišský 450g rezaný");
        assert_eq!(first.label_png_base64, "iVBOR");
        let sep = &b.lines_in_order()[1];
        assert_eq!(sep.product_name, "");
        assert_eq!(sep.best_before, "");
        assert_eq!(sep.line_type, "separator");

        let empty = parse_next(json!({"lines": []})).unwrap();
        assert!(empty.lines.is_empty());
        assert_eq!(empty.batch_id, None);
    }

    #[test]
    fn test_parse_next_rejects_bad_shapes() {
        assert!(matches!(
            parse_next(json!({"lines": [{"line_id": 1, "print_qty": 1}]})),
            Err(RpcError::Protocol(_))
        ));
        // only an invalid line, still needs its batch
        assert!(matches!(
            parse_next(json!({"lines": [{"line_id": 1}]})),
            Err(RpcError::Protocol(_))
        ));
        assert!(matches!(
            parse_next(json!({"batch_id": "x", "lines": []})),
            Err(RpcError::Protocol(_))
        ));
        assert!(matches!(parse_next(json!([1])), Err(RpcError::Protocol(_))));
        assert!(matches!(
            parse_next(json!({"batch_id": 1.5, "lines": []})),
            Err(RpcError::Protocol(_))
        ));
        assert!(matches!(
            parse_next(json!({"batch_id": true, "lines": []})),
            Err(RpcError::Protocol(_))
        ));
    }

    #[test]
    fn test_parse_next_isolates_invalid_lines() {
        let b = parse_next(json!({"batch_id": 7, "lines": [
            {"line_id": 1, "print_qty": 1.5},
            {"print_qty": 1},
            {"line_id": 3, "print_qty": 2, "label_png_base64": "x"}
        ]}))
        .unwrap();
        assert_eq!(b.lines.len(), 1);
        assert_eq!(b.lines[0].line_id, 3);
        assert_eq!(b.invalid.len(), 2);
        assert_eq!(b.invalid[0].line_id, Some(1));
        assert!(
            b.invalid[0].reason.starts_with("invalid line: "),
            "{:?}",
            b.invalid[0]
        );
        assert!(
            b.invalid[0].reason.contains("whole number"),
            "{:?}",
            b.invalid[0]
        );
        assert_eq!(b.invalid[1].line_id, None);
    }

    #[test]
    fn test_batch_id_false_or_null_is_empty() {
        for v in [json!(false), json!(null)] {
            let b = parse_next(json!({"batch_id": v, "lines": []})).unwrap();
            assert_eq!(b.batch_id, None);
            assert!(b.lines.is_empty() && b.invalid.is_empty());
        }
    }

    #[test]
    fn test_ack_params_contract_mapping() {
        assert_eq!(
            ack_params(345, 40, None),
            json!({"line_id": 345, "printed_qty": 40, "error": null})
        );
        assert_eq!(
            ack_params(346, 0, Some("empty png")),
            json!({"line_id": 346, "printed_qty": 0, "error": "empty png"})
        );
        let long = "č".repeat(250);
        let p = ack_params(1, 0, Some(&long));
        assert_eq!(p["error"].as_str().unwrap().chars().count(), 200);
    }

    #[test]
    fn test_parse_ack() {
        assert_eq!(
            parse_ack(&json!({"ok": true})).unwrap(),
            AckAccepted { duplicate: false }
        );
        assert_eq!(
            parse_ack(&json!({"ok": true, "duplicate": true})).unwrap(),
            AckAccepted { duplicate: true }
        );
        assert!(matches!(
            parse_ack(&json!({"ok": false})),
            Err(RpcError::Protocol(_))
        ));
        assert!(matches!(parse_ack(&json!({})), Err(RpcError::Protocol(_))));
    }

    #[test]
    fn test_heartbeat_params() {
        let hb = Heartbeat {
            printer_name: "TSC ML241P Spišská".into(),
            paper_status: "ok".into(),
            error_status: "offline".into(),
            client_version: "0.8.41".into(),
            last_job_id: Some(12),
            last_result: "printed 3 labels".into(),
            paper_size: "50x30 mm".into(),
        };
        assert_eq!(
            hb.params(),
            json!({"printer_name": "TSC ML241P Spišská", "paper_status": "ok",
                   "error_status": "offline", "client_version": "0.8.41",
                   "last_job_id": 12, "last_result": "printed 3 labels",
                   "paper_size": "50x30 mm"})
        );
    }

    #[test]
    fn test_parse_next_reads_the_batch_label_size() {
        let b = parse_next(
            json!({"batch_id": 21, "label_width_mm": 50, "label_height_mm": 30.0,
            "dpi": 203, "lines": [{"line_id": 1, "print_qty": 1}]}),
        )
        .unwrap();
        assert_eq!(
            b.label_size,
            BatchLabelSize {
                width_mm: BatchNumber::Number(50.0),
                height_mm: BatchNumber::Number(30.0),
                dpi: BatchNumber::Number(203.0),
            }
        );
        assert_eq!(b.lines.len(), 1);
    }

    #[test]
    fn test_parse_next_without_a_size_or_with_false_null_is_absent() {
        let absent = BatchLabelSize::default();
        assert_eq!(absent.width_mm, BatchNumber::Absent);
        assert_eq!(absent.height_mm, BatchNumber::Absent);
        assert_eq!(absent.dpi, BatchNumber::Absent);
        let none = parse_next(json!({"batch_id": 3, "lines": []})).unwrap();
        assert_eq!(none.label_size, absent);
        let empty = parse_next(json!({"batch_id": 3, "label_width_mm": false,
            "label_height_mm": null, "dpi": false, "lines": []}))
        .unwrap();
        assert_eq!(empty.label_size, absent);
        // A format whose size is unset in Odoo (fields.Float reads 0.0).
        let unset = parse_next(json!({"batch_id": 3, "label_width_mm": 0.0,
            "label_height_mm": 0, "dpi": 0, "lines": []}))
        .unwrap();
        assert_eq!(unset.label_size, absent);
    }

    #[test]
    fn test_parse_next_keeps_a_non_number_size_instead_of_failing_the_poll() {
        let b = parse_next(json!({"batch_id": 4, "label_width_mm": "50",
            "label_height_mm": true, "dpi": [203], "lines": [{"line_id": 1, "print_qty": 1}]}))
        .unwrap();
        assert_eq!(
            b.label_size.width_mm,
            BatchNumber::NotANumber("\"50\"".into())
        );
        assert_eq!(
            b.label_size.height_mm,
            BatchNumber::NotANumber("true".into())
        );
        assert_eq!(b.label_size.dpi, BatchNumber::NotANumber("[203]".into()));
        assert_eq!(b.lines.len(), 1, "the lines are still parsed");
    }

    #[test]
    fn test_batch_number_mapping() {
        assert_eq!(batch_number(json!(null)), BatchNumber::Absent);
        assert_eq!(batch_number(json!(false)), BatchNumber::Absent);
        // Odoo's unset fields.Float
        assert_eq!(batch_number(json!(0)), BatchNumber::Absent);
        assert_eq!(batch_number(json!(0.0)), BatchNumber::Absent);
        assert_eq!(batch_number(json!(-0.0)), BatchNumber::Absent);
        assert_eq!(batch_number(json!(0.5)), BatchNumber::Number(0.5));
        assert_eq!(batch_number(json!(72.7)), BatchNumber::Number(72.7));
        assert_eq!(batch_number(json!(-5)), BatchNumber::Number(-5.0));
        assert_eq!(
            batch_number(json!(u64::MAX)),
            BatchNumber::Number(u64::MAX as f64)
        );
        assert_eq!(
            batch_number(json!(true)),
            BatchNumber::NotANumber("true".into())
        );
        assert_eq!(
            batch_number(json!({"mm": 5})),
            BatchNumber::NotANumber("{\"mm\":5}".into())
        );
    }

    #[test]
    fn test_endpoint_trims_trailing_slash() {
        let rpc = OdooRpc::new("https://erp.example.test/", "k-123").unwrap();
        assert_eq!(
            rpc.endpoint("/food/print/next"),
            "https://erp.example.test/food/print/next"
        );
    }

    #[test]
    fn test_numeric_text_fields_are_kept() {
        let b = parse_next(json!({"batch_id": 3, "production_date": 20260929,
            "lines": [{"line_id": 1, "print_qty": 1, "product_name": 42}]}))
        .unwrap();
        assert_eq!(b.production_date, "20260929");
        assert_eq!(b.lines[0].product_name, "42");
        assert_eq!(b.lines[0].sequence, 0);
    }

    #[test]
    fn test_truncate_ack_error() {
        assert_eq!(truncate_ack_error("short"), "short");
        assert_eq!(truncate_ack_error(&"a".repeat(201)).len(), 200);
    }
}
