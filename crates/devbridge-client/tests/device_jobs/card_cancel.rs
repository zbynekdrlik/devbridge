//! `card.cancel` through the whole Odoo path (#102 lane V, Worldline docx
//! case I5): the real `DeviceJobSource` claims a `card.purchase` that sits in
//! its card wait on the terminal queue, then claims the `card.cancel` — which
//! must NOT wait behind it (its own `<ip>#cancel` queue) — the cancel reaches
//! the purchase's own terminal object, and Odoo gets `done {cancel_sent}` for
//! the cancel and `failed cancelled` for the purchase. The TIM library is
//! replaced by a fake behind the public `TimBackend` seam (the external
//! system; the executor, journal, queues and acks are the real ones).

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::json;

use devbridge_client::device_jobs::executor::ExecutorRegistry;
use devbridge_client::device_jobs::journal::Journal;
use devbridge_client::device_jobs::tim::card::{LastTransaction, TwoPhase, TxFacts};
use devbridge_client::device_jobs::tim::reports::{BalanceReport, Receipts, ShiftReport};
use devbridge_client::device_jobs::tim::{
    PrintOption, TerminalSpec, TimBackend, TimCanceller, TimError, TimExecutor, TimTerminal,
};

use super::fake::*;

/// The card wait: `true` once a cancel reached it.
type Wait = Arc<(Mutex<WaitState>, Condvar)>;

#[derive(Default)]
struct WaitState {
    in_card_wait: bool,
    cancelled: bool,
    cancels: usize,
}

/// A terminal where the cardholder never comes: the transaction waits until
/// a cancel, then answers rc 2 (`api_cancel_ecr`), as TIM does.
struct WaitingTim(Wait);

impl TimBackend for WaitingTim {
    fn settings_text(&self, spec: &TerminalSpec) -> Result<String, TimError> {
        Ok(format!("TerminalSettings(ip={})", spec.ip))
    }
    fn terminal(&self, _: &TerminalSpec) -> Result<Box<dyn TimTerminal>, TimError> {
        Ok(Box::new(WaitingTerminal(Arc::clone(&self.0))))
    }
}

struct WaitingTerminal(Wait);

struct WaitingCanceller(Wait);

impl TimCanceller for WaitingCanceller {
    fn cancel(&self) -> Result<(), TimError> {
        let (state, cv) = &*self.0;
        let mut s = state.lock().unwrap();
        s.cancels += 1;
        if s.in_card_wait {
            s.cancelled = true;
            cv.notify_all();
        }
        Ok(())
    }
}

fn not_used<T>(call: &'static str) -> Result<T, TimError> {
    panic!("{call} is not part of a cancelled purchase")
}

impl TimTerminal for WaitingTerminal {
    fn canceller(&self) -> Result<Arc<dyn TimCanceller>, TimError> {
        Ok(Arc::new(WaitingCanceller(Arc::clone(&self.0))))
    }
    fn set_print_options(&mut self, _: &[PrintOption]) -> Result<(), TimError> {
        Ok(())
    }
    fn connect(&mut self) -> Result<(), TimError> {
        Ok(())
    }
    fn login(&mut self) -> Result<(), TimError> {
        Ok(())
    }
    fn terminal_id(&mut self) -> Result<Option<String>, TimError> {
        Ok(Some("12345678".into()))
    }
    fn logout(&mut self) -> Result<(), TimError> {
        Ok(())
    }
    fn disconnect(&mut self) -> Result<(), TimError> {
        Ok(())
    }
    fn last_transaction(&mut self) -> Result<LastTransaction, TimError> {
        Ok(LastTransaction::default())
    }
    fn transaction(&mut self, _: TwoPhase, _: i64) -> Result<TxFacts, TimError> {
        let (state, cv) = &*self.0;
        let mut s = state.lock().unwrap();
        s.in_card_wait = true;
        cv.notify_all();
        let (mut s, timeout) = cv
            .wait_timeout_while(s, Duration::from_secs(20), |s| !s.cancelled)
            .unwrap();
        s.in_card_wait = false;
        assert!(
            !timeout.timed_out(),
            "the cancel never reached the card wait"
        );
        Err(TimError {
            call: "ta_terminal_transaction",
            code: 2,
        })
    }
    fn commit(&mut self) -> Result<(), TimError> {
        not_used("commit")
    }
    fn rollback(&mut self) -> Result<(), TimError> {
        not_used("rollback")
    }
    fn reversal(&mut self, _: i64) -> Result<TxFacts, TimError> {
        not_used("reversal")
    }
    fn activate(&mut self) -> Result<ShiftReport, TimError> {
        not_used("activate")
    }
    fn deactivate(&mut self) -> Result<ShiftReport, TimError> {
        not_used("deactivate")
    }
    fn balance(&mut self) -> Result<BalanceReport, TimError> {
        not_used("balance")
    }
    fn reprint(&mut self) -> Result<Receipts, TimError> {
        not_used("reprint")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_card_cancel_reaches_a_purchase_in_its_card_wait_through_odoo() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let wait: Wait = Arc::default();
    let tim = TimExecutor::with_backend(
        vec![TerminalSpec {
            kasa_id: 3,
            ip: "10.78.2.8".into(),
            expected_tid: None,
            integrator_id: "7dc442d7-6528-4a05-ba84-5e66558e48b7".into(),
            commit_timeout_s: 60,
        }],
        Ok(Arc::new(WaitingTim(Arc::clone(&wait)))),
        dir.path(),
    )
    .with_reconnect_gap(Duration::ZERO)
    .with_journal(Arc::clone(&journal));
    let mut registry = ExecutorRegistry::default();
    registry.register(Arc::new(tim)).unwrap();
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());

    fake.lock().unwrap().queue(
        951,
        "card.purchase",
        json!({"amount": 2.5, "currency": "EUR"}),
    );
    eventually("the purchase sits in its card wait", || {
        wait.0.lock().unwrap().in_card_wait
    })
    .await;
    fake.lock()
        .unwrap()
        .queue(952, "card.cancel", json!({"card_job_id": 951}));
    eventually("cancel done and purchase failed", || {
        final_state(&fake, 952) == St::Done && final_state(&fake, 951) == St::Failed
    })
    .await;
    run.stop().await;

    let s = fake.lock().unwrap();
    assert_eq!(
        s.jobs[&952].result,
        Some(json!({"cancel_sent": true, "card_job_id": 951}))
    );
    assert_eq!(s.jobs[&951].error_code.as_deref(), Some("cancelled"));
    drop(s);
    assert_eq!(wait.0.lock().unwrap().cancels, 1, "one cancel, sent once");
    assert_clean(&fake);
}
