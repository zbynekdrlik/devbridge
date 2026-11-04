//! The 23:50 safety balance (#102 lane D3, design odoo-erp issue 7598 §2):
//! Worldline's validation asks for a balance at least once a day. Odoo
//! composes `card.balance` after the last Z of the day (lane O2); when no
//! balance ran on a terminal by 23:50 LOCAL time, DevBridge runs one itself —
//! once per terminal and day, journaled before the call
//! (`card_balances`, [`super::ops`]), so a restart after 23:50 never runs a
//! second one. Odoo has no channel for its result yet: journal + log.

use std::time::Duration;

use chrono::{DateTime, Local, NaiveTime, Timelike};
use tracing::info;

use super::TimExecutor;
use super::ops::local_date;

/// How often the safety balance checks the clock (no terminal contact).
pub const SAFETY_TICK: Duration = Duration::from_secs(60);

/// Local time from which the day's safety balance is due.
pub fn safety_time() -> NaiveTime {
    NaiveTime::from_hms_opt(23, 50, 0).expect("23:50 is a time")
}

/// Whether the safety balance of the day is due at local time `now`
/// (23:50 until midnight).
pub fn safety_due(now: NaiveTime) -> bool {
    now >= safety_time()
}

impl TimExecutor {
    /// One check at `now`: a safety balance on every terminal that had no
    /// balance today. Returns how many were sent.
    pub async fn safety_tick(&self, now: DateTime<Local>) -> usize {
        if !safety_due(now.time()) || self.backend.is_err() {
            return 0;
        }
        let date = local_date(now);
        let mut sent = 0;
        for spec in self.terminals.values().cloned().collect::<Vec<_>>() {
            if self.safety_balance(spec, &date).await.is_some() {
                sent += 1;
            }
        }
        info!(local_date = %date, minute = now.minute(), sent, "TIM safety balance check");
        sent
    }

    /// Check every [`SAFETY_TICK`] for as long as the source runs.
    pub async fn safety_loop(&self) {
        info!(at = %safety_time(), terminals = self.terminals.len(), "TIM safety balance armed (local time, once a day per terminal)");
        loop {
            self.safety_tick(Local::now()).await;
            tokio::time::sleep(SAFETY_TICK).await;
        }
    }
}
