//! The fake TIM library of the unit tests (#102, CI — no SDK): scripted
//! answers per call, and the calls it saw.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::{
    DeviceJob, TerminalSpec, TimBackend, TimCanceller, TimError, TimExecutor, TimTerminal, card,
    reports,
};
use devbridge_core::config::TIM_DEFAULT_INTEGRATOR_ID;

/// Scripted failures of the fake library, and the calls it saw.
#[derive(Default)]
pub(super) struct Script {
    pub(super) fail: Vec<&'static str>,
    pub(super) terminal_id: Option<String>,
    pub(super) calls: Vec<String>,
    /// What `transaction_info_request` answers (default: no transaction).
    pub(super) last: card::LastTransaction,
    /// What the purchase answers: the authorisation or the rc (`None`:
    /// rc 5 function disallowed).
    pub(super) purchase: Option<Result<card::TxFacts, i32>>,
    /// rc of a failing commit / rollback (`None` = ok).
    pub(super) commit_rc: Option<i32>,
    pub(super) rollback_rc: Option<i32>,
    /// What a reversal answers (`None`: rc 5).
    pub(super) reversal: Option<Result<card::TxFacts, i32>>,
    /// rc of a failing activate / deactivate / balance / reprint (`None` = ok).
    pub(super) op_rc: Option<i32>,
    /// What activate / deactivate / balance / reprint answer.
    pub(super) shift: reports::ShiftReport,
    pub(super) balance: reports::BalanceReport,
    pub(super) reprint: reports::Receipts,
    /// The print options set on each terminal object, in order (lane V).
    pub(super) print_options: Vec<Vec<super::PrintOption>>,
    /// The purchase / credit BLOCKS (the cardholder never comes) until a
    /// cancel reaches it, then answers rc 2 `api_cancel_ecr` (card.cancel).
    pub(super) wait_for_cancel: bool,
    /// A transaction is blocked in its card wait right now.
    pub(super) in_card_wait: bool,
    /// A cancel reached the transaction in its card wait.
    pub(super) cancelled: bool,
}

/// How long a fake card wait lasts without a cancel (then rc 14 timeout —
/// a test that forgot to cancel fails instead of hanging).
pub(super) const FAKE_CARD_WAIT: Duration = Duration::from_secs(20);

#[derive(Clone, Default)]
pub(super) struct FakeTim(pub(super) Arc<Mutex<Script>>);

impl FakeTim {
    pub(super) fn failing(calls: &[&'static str]) -> Self {
        let fake = Self::default();
        fake.0.lock().unwrap().fail = calls.to_vec();
        fake.0.lock().unwrap().terminal_id = Some("12345678".into());
        fake
    }

    pub(super) fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().calls.clone()
    }

    /// The print options set on each terminal object so far.
    pub(super) fn print_options(&self) -> Vec<Vec<super::PrintOption>> {
        self.0.lock().unwrap().print_options.clone()
    }

    fn step(&self, call: &'static str) -> Result<(), TimError> {
        let mut s = self.0.lock().unwrap();
        s.calls.push(call.to_string());
        if s.fail.contains(&call) {
            return Err(TimError { call, code: 8 });
        }
        Ok(())
    }
}

impl TimBackend for FakeTim {
    fn settings_text(&self, spec: &TerminalSpec) -> Result<String, TimError> {
        self.step("settings")?;
        Ok(format!("TerminalSettings(ip={})", spec.ip))
    }

    fn terminal(&self, spec: &TerminalSpec) -> Result<Box<dyn TimTerminal>, TimError> {
        self.step("create")?;
        self.0
            .lock()
            .unwrap()
            .calls
            .push(format!("pos_id {}", spec.pos_id()));
        Ok(Box::new(FakeTerminal(self.clone())))
    }
}

struct FakeTerminal(FakeTim);

/// The fake's cancel handle: a cancel only acts on a transaction in its
/// card wait (the SDK: no effect without an open request).
struct FakeCanceller(FakeTim);

impl TimCanceller for FakeCanceller {
    fn cancel(&self) -> Result<(), TimError> {
        self.0.step("cancel")?;
        let mut s = self.0.0.lock().unwrap();
        if s.in_card_wait {
            s.cancelled = true;
        }
        Ok(())
    }
}

impl TimTerminal for FakeTerminal {
    fn canceller(&self) -> Result<Arc<dyn TimCanceller>, TimError> {
        // not recorded in `calls` (the session sequences stay as they
        // were); scripted to fail with `fail: ["canceller"]`
        if self.0.0.lock().unwrap().fail.contains(&"canceller") {
            return Err(TimError {
                call: "ta_object_retain",
                code: 139,
            });
        }
        Ok(Arc::new(FakeCanceller(self.0.clone())))
    }
    fn set_print_options(&mut self, options: &[super::PrintOption]) -> Result<(), TimError> {
        self.0.step("print_options")?;
        let mut s = self.0.0.lock().unwrap();
        s.print_options.push(options.to_vec());
        Ok(())
    }
    fn connect(&mut self) -> Result<(), TimError> {
        self.0.step("connect")
    }
    fn login(&mut self) -> Result<(), TimError> {
        self.0.step("login")
    }
    fn terminal_id(&mut self) -> Result<Option<String>, TimError> {
        self.0.step("terminal_id")?;
        Ok(self.0.0.lock().unwrap().terminal_id.clone())
    }
    fn logout(&mut self) -> Result<(), TimError> {
        self.0.step("logout")
    }
    fn disconnect(&mut self) -> Result<(), TimError> {
        self.0.step("disconnect")
    }
    fn last_transaction(&mut self) -> Result<card::LastTransaction, TimError> {
        self.0.step("last_transaction")?;
        Ok(self.0.0.lock().unwrap().last.clone())
    }
    fn transaction(
        &mut self,
        kind: card::TwoPhase,
        amount_minor: i64,
    ) -> Result<card::TxFacts, TimError> {
        self.0.step(match kind {
            card::TwoPhase::Purchase => "purchase",
            card::TwoPhase::Credit => "credit",
        })?;
        let mut s = self.0.0.lock().unwrap();
        s.calls.push(format!("amount {amount_minor}"));
        if s.wait_for_cancel {
            s.in_card_wait = true;
            drop(s);
            let until = std::time::Instant::now() + FAKE_CARD_WAIT;
            loop {
                let mut s = self.0.0.lock().unwrap();
                let code = if s.cancelled {
                    2
                } else if std::time::Instant::now() >= until {
                    14
                } else {
                    drop(s);
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                s.in_card_wait = false;
                s.cancelled = false;
                return Err(TimError {
                    call: "ta_terminal_transaction",
                    code,
                });
            }
        }
        match s.purchase.clone() {
            Some(Ok(facts)) => Ok(facts),
            Some(Err(code)) => Err(TimError {
                call: "ta_terminal_transaction",
                code,
            }),
            None => Err(TimError {
                call: "ta_terminal_transaction",
                code: 5,
            }),
        }
    }
    fn commit(&mut self) -> Result<(), TimError> {
        self.0.step("commit")?;
        match self.0.0.lock().unwrap().commit_rc {
            Some(code) => Err(TimError {
                call: "ta_terminal_commit",
                code,
            }),
            None => Ok(()),
        }
    }
    fn rollback(&mut self) -> Result<(), TimError> {
        self.0.step("rollback")?;
        match self.0.0.lock().unwrap().rollback_rc {
            Some(code) => Err(TimError {
                call: "ta_terminal_rollback",
                code,
            }),
            None => Ok(()),
        }
    }
    fn reversal(&mut self, trans_seq: i64) -> Result<card::TxFacts, TimError> {
        self.0.step("reversal")?;
        let mut s = self.0.0.lock().unwrap();
        // the ONLY argument a reversal sends
        s.calls.push(format!("trans_seq {trans_seq}"));
        match s.reversal.clone() {
            Some(Ok(facts)) => Ok(facts),
            Some(Err(code)) => Err(TimError {
                call: "ta_terminal_transaction2",
                code,
            }),
            None => Err(TimError {
                call: "ta_terminal_transaction2",
                code: 5,
            }),
        }
    }
    fn activate(&mut self) -> Result<reports::ShiftReport, TimError> {
        self.0.step("activate")?;
        let s = self.0.0.lock().unwrap();
        fake_op(s.op_rc, "ta_terminal_activate", s.shift.clone())
    }
    fn deactivate(&mut self) -> Result<reports::ShiftReport, TimError> {
        self.0.step("deactivate")?;
        let s = self.0.0.lock().unwrap();
        fake_op(s.op_rc, "ta_terminal_deactivate", s.shift.clone())
    }
    fn balance(&mut self) -> Result<reports::BalanceReport, TimError> {
        self.0.step("balance")?;
        let s = self.0.0.lock().unwrap();
        fake_op(s.op_rc, "ta_terminal_balance", s.balance.clone())
    }
    fn reprint(&mut self) -> Result<reports::Receipts, TimError> {
        self.0.step("reprint")?;
        let s = self.0.0.lock().unwrap();
        fake_op(s.op_rc, "ta_terminal_receipt_request", s.reprint.clone())
    }
}

fn fake_op<T>(rc: Option<i32>, call: &'static str, answer: T) -> Result<T, TimError> {
    match rc {
        Some(code) => Err(TimError { call, code }),
        None => Ok(answer),
    }
}

impl Drop for FakeTerminal {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().calls.push("dispose".into());
    }
}

pub(super) fn spec(kasa_id: i64, ip: &str, tid: Option<&str>) -> TerminalSpec {
    TerminalSpec {
        kasa_id,
        ip: ip.into(),
        expected_tid: tid.map(str::to_string),
        integrator_id: TIM_DEFAULT_INTEGRATOR_ID.into(),
        commit_timeout_s: 60,
    }
}

pub(super) fn job(job_type: &str, kasa_id: i64, payload: Value) -> DeviceJob {
    DeviceJob {
        id: 7,
        job_type: job_type.into(),
        client_uuid: "u-7".into(),
        kasa_id: Some(kasa_id),
        kasa_name: String::new(),
        pos_order: Value::Null,
        payload,
        created_at: String::new(),
        claimed_at: String::new(),
    }
}

pub(super) fn executor(fake: &FakeTim, dir: &Path) -> TimExecutor {
    TimExecutor::with_backend(
        vec![
            spec(3, "10.78.2.8", Some("12345678")),
            spec(4, "10.78.2.18", None),
        ],
        Ok(Arc::new(fake.clone())),
        dir,
    )
    .with_reconnect_gap(Duration::ZERO)
}
