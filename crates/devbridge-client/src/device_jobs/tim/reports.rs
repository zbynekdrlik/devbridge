//! What the terminal answers to the non-payment card jobs (#102 lane D3) —
//! the shift (`card.activate` / `card.deactivate`), the day closing
//! (`card.balance`, also the 23:50 safety balance) and the copy of the latest
//! receipt (`card.print_copy`) — and the job results built from it. Pure: the
//! native reader fills these, unit tests build them by hand.

use serde_json::{Value, json};

use super::card::transaction_type_name;

/// The two slips of a terminal answer (recipient merchant / cardholder).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Receipts {
    pub merchant: Option<String>,
    pub cardholder: Option<String>,
}

/// One sum of a brand total per transaction type (`ta_trx_detail`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrxDetail {
    pub transaction_type: Option<i32>,
    pub count: Option<i32>,
    pub amount_minor: Option<i64>,
}

/// One currency total of a brand (`ta_total`): `amount_minor` is the
/// terminal's NET sum (purchases − reversals − credits, measured 4.11.2026).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Total {
    /// `Some(true)` = EUR.
    pub currency_eur: Option<bool>,
    pub count: Option<i32>,
    pub amount_minor: Option<i64>,
    pub details: Vec<TrxDetail>,
}

/// The counters of one brand (`ta_counter`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrandCounter {
    pub brand_name: Option<String>,
    pub acq_id: Option<i64>,
    pub count: Option<i32>,
    pub totals: Vec<Total>,
}

/// `ta_counters`: the period / shift sequence and the brands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counters {
    pub seq_counter: Option<i32>,
    pub brands: Vec<BrandCounter>,
}

/// `ta_terminal_activate` / `ta_terminal_deactivate`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShiftReport {
    /// The activation's sequence (activate only).
    pub act_seq_counter: Option<i32>,
    /// The shift's sums (deactivate only).
    pub counters: Option<Counters>,
    pub receipts: Receipts,
}

/// `ta_terminal_balance`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BalanceReport {
    pub counters: Option<Counters>,
    pub receipts: Receipts,
}

fn euros(minor: Option<i64>) -> Option<f64> {
    minor.map(|m| m as f64 / 100.0)
}

fn currency(eur: Option<bool>) -> Option<&'static str> {
    (eur == Some(true)).then_some("EUR")
}

fn receipts_json(receipts: &Receipts) -> Value {
    json!({"merchant": receipts.merchant, "cardholder": receipts.cardholder})
}

/// The EUR sum over every brand: `(count, amount in cents)`.
pub fn eur_total(counters: &Counters) -> (i64, i64) {
    let mut count = 0i64;
    let mut amount = 0i64;
    for total in counters.brands.iter().flat_map(|b| &b.totals) {
        if total.currency_eur == Some(true) {
            count += i64::from(total.count.unwrap_or(0));
            amount += total.amount_minor.unwrap_or(0);
        }
    }
    (count, amount)
}

/// The brands of `counters` as Odoo stores them (per-brand counters).
pub fn brands_json(counters: &Counters) -> Value {
    Value::Array(
        counters
            .brands
            .iter()
            .map(|b| {
                json!({
                    "brand_name": b.brand_name,
                    "acq_id": b.acq_id,
                    "count": b.count,
                    "totals": b.totals.iter().map(|t| json!({
                        "currency": currency(t.currency_eur),
                        "count": t.count,
                        "amount": euros(t.amount_minor),
                        "details": t.details.iter().map(|d| json!({
                            "transaction_type": d.transaction_type.map(transaction_type_name),
                            "count": d.count,
                            "amount": euros(d.amount_minor),
                        })).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

fn counters_fields(counters: Option<&Counters>) -> (Value, Value, Value) {
    match counters {
        Some(c) => {
            let (count, amount) = eur_total(c);
            (
                json!(c.seq_counter),
                brands_json(c),
                json!({"currency": "EUR", "count": count, "amount": amount as f64 / 100.0}),
            )
        }
        None => (Value::Null, json!([]), Value::Null),
    }
}

/// The result of a balance (`state: "closed"`): the period, the per-brand
/// counters and the EUR total, as the terminal's day-closing slip shows them.
pub fn balance_result(tid: Option<&str>, report: &BalanceReport) -> Value {
    let (period, brands, total) = counters_fields(report.counters.as_ref());
    json!({
        "state": "closed",
        "terminal": {"tid": tid},
        "period": period,
        "brands": brands,
        "total": total,
        "receipts": receipts_json(&report.receipts),
    })
}

/// The result of `card.activate` (`activated`) / `card.deactivate`
/// (`deactivated`, with the shift's counters).
pub fn shift_result(state: &str, tid: Option<&str>, report: &ShiftReport) -> Value {
    let (shift, brands, total) = counters_fields(report.counters.as_ref());
    json!({
        "state": state,
        "terminal": {"tid": tid},
        "act_seq_counter": report.act_seq_counter,
        "shift": shift,
        "brands": brands,
        "total": total,
        "receipts": receipts_json(&report.receipts),
    })
}

/// Whether the terminal handed back any slip text. With the print format
/// `on_device_with_receipt` it should; `false` = the terminal did not honour
/// the print options (a lane V finding — whether it printed is unknown).
pub fn receipts_returned(receipts: &Receipts) -> bool {
    receipts.merchant.is_some() || receipts.cardholder.is_some()
}

/// The result of a `card.print_copy` the terminal printed itself.
pub fn copy_result(card_job_id: i64, trans_seq: i64, receipts: &Receipts) -> Value {
    json!({
        "state": "printed",
        "card_job_id": card_job_id,
        "trans_seq": trans_seq,
        "receipts": receipts_json(receipts),
        "receipts_returned": receipts_returned(receipts),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The simulator's balance after 2 purchases (2.70), a reversal (2.00)
    /// and a credit (0.50), 4.11.2026.
    pub(crate) fn sim_counters() -> Counters {
        Counters {
            seq_counter: Some(4),
            brands: vec![BrandCounter {
                brand_name: Some("VISA".into()),
                acq_id: Some(2),
                count: Some(4),
                totals: vec![
                    Total {
                        currency_eur: Some(true),
                        count: Some(4),
                        amount_minor: Some(20),
                        details: vec![
                            TrxDetail {
                                transaction_type: Some(1),
                                count: Some(2),
                                amount_minor: Some(270),
                            },
                            TrxDetail {
                                transaction_type: Some(3),
                                count: Some(1),
                                amount_minor: Some(200),
                            },
                        ],
                    },
                    Total {
                        currency_eur: Some(false),
                        count: Some(9),
                        amount_minor: Some(999),
                        details: vec![],
                    },
                ],
            }],
        }
    }

    #[test]
    fn test_balance_result_shape() {
        let report = BalanceReport {
            counters: Some(sim_counters()),
            receipts: Receipts {
                merchant: Some("Daily closing".into()),
                cardholder: None,
            },
        };
        assert_eq!(
            balance_result(Some("12345678"), &report),
            json!({
                "state": "closed",
                "terminal": {"tid": "12345678"},
                "period": 4,
                "brands": [{
                    "brand_name": "VISA", "acq_id": 2, "count": 4,
                    "totals": [
                        {"currency": "EUR", "count": 4, "amount": 0.2, "details": [
                            {"transaction_type": "purchase", "count": 2, "amount": 2.7},
                            {"transaction_type": "reversal", "count": 1, "amount": 2.0},
                        ]},
                        {"currency": null, "count": 9, "amount": 9.99, "details": []},
                    ],
                }],
                "total": {"currency": "EUR", "count": 4, "amount": 0.2},
                "receipts": {"merchant": "Daily closing", "cardholder": null},
            })
        );
    }

    #[test]
    fn test_eur_total_sums_every_brand_in_eur_only() {
        let mut counters = sim_counters();
        counters.brands.push(BrandCounter {
            brand_name: Some("MC".into()),
            totals: vec![Total {
                currency_eur: Some(true),
                count: Some(3),
                amount_minor: Some(-150),
                details: vec![],
            }],
            ..BrandCounter::default()
        });
        assert_eq!(eur_total(&counters), (7, -130));
        assert_eq!(eur_total(&Counters::default()), (0, 0));
    }

    #[test]
    fn test_shift_and_copy_results() {
        let open = ShiftReport {
            act_seq_counter: Some(3),
            counters: None,
            receipts: Receipts::default(),
        };
        assert_eq!(
            shift_result("activated", None, &open),
            json!({"state": "activated", "terminal": {"tid": null}, "act_seq_counter": 3,
                   "shift": null, "brands": [], "total": null,
                   "receipts": {"merchant": null, "cardholder": null}})
        );
        let close = ShiftReport {
            act_seq_counter: None,
            counters: Some(sim_counters()),
            receipts: Receipts::default(),
        };
        let r = shift_result("deactivated", Some("1"), &close);
        assert_eq!(r["shift"], json!(4));
        assert_eq!(
            r["total"],
            json!({"currency": "EUR", "count": 4, "amount": 0.2})
        );
        let copy = copy_result(
            9,
            102,
            &Receipts {
                merchant: Some("m".into()),
                cardholder: Some("c".into()),
            },
        );
        assert_eq!(
            copy,
            json!({"state": "printed", "card_job_id": 9, "trans_seq": 102,
                   "receipts": {"merchant": "m", "cardholder": "c"}, "receipts_returned": true})
        );
        // either slip alone counts; none = the terminal returned no text
        for (merchant, cardholder, returned) in [
            (Some("m"), None, true),
            (None, Some("c"), true),
            (None, None, false),
        ] {
            let receipts = Receipts {
                merchant: merchant.map(str::to_string),
                cardholder: cardholder.map(str::to_string),
            };
            assert_eq!(receipts_returned(&receipts), returned, "{receipts:?}");
            assert_eq!(
                copy_result(9, 102, &receipts)["receipts_returned"],
                returned
            );
        }
    }
}
