//! The Worldline card terminal through the TIM API C library (#102).
//!
//! Lane D1 (design odoo-erp issue 7598 §9): the binding, the per-use
//! session, the TIM logs and the heartbeat state. Lane D2: `card.purchase`
//! two-phase ([`purchase`], the held session), `card.commit` /
//! `card.rollback` on the terminal's settle queue, the deadline rollback and
//! the restart reconcile ([`jobs`], decisions in [`card`]; contract
//! `docs/device-jobs-card.md`). Lane D3: `card.credit` (the same held
//! session), `card.reversal` / `card.activate` / `card.deactivate` /
//! `card.balance` / `card.print_copy` (one session each, [`ops`], answers in
//! [`reports`]) and the 23:50 safety balance ([`safety`]). Lane V:
//! `card.cancel` stops a purchase / credit still in its card wait
//! (`ta_terminal_cancel` on the running session's terminal object, its own
//! `<ip>#cancel` queue, [`TimCanceller`]). Any other `card.*` type is
//! refused `unsupported` before it is journaled.
//!
//! - [`TimBackend`] / [`TimTerminal`] are the seam: [`native`] drives the
//!   real library (loaded at runtime, [`ffi`]), unit tests a fake.
//! - **Connect per use.** A [`run_session`] creates the terminal object,
//!   connects, logs in, reads the terminal id, logs out, disconnects and
//!   disposes it. Nothing is held between sessions: the terminal takes ONE
//!   ECR connection and the store's Codex still uses it. D1 never opens a
//!   session by itself (no timer probe) — [`TimExecutor::session`] is the
//!   primitive the simulator test and D2's jobs run.
//! - Heartbeat `terminal=`: `offline` (library not loaded, no till with a
//!   terminal, or the last session of a terminal failed), `busy` (a
//!   session runs), `tim-ok` (library loaded, no failed session).
//! - Every TIM log record goes to `<data dir>/logs/tim/tim-YYYY-MM-DD.log`
//!   ([`logs`]); `tim-settings.cfg` there holds the settings at start.

#[cfg(test)]
mod cancel_tests;
pub mod card;
#[cfg(test)]
mod card_jobs_tests;
#[cfg(test)]
mod fake;
pub mod ffi;
mod jobs;
pub mod logs;
pub mod native;
mod ops;
#[cfg(test)]
mod ops_safety_tests;
#[cfg(test)]
mod ops_tests;
pub mod purchase;
pub mod reports;
pub mod safety;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use devbridge_core::config::PokladnaClientConfig;
use tracing::{error, info, warn};

use super::api::DeviceJob;
use super::executor::{DeviceExecutor, DeviceKind, Outcome, Reconciliation, Rejection};
use super::journal::{Journal, JournalEntry};
use card::{LastTransaction, TwoPhase, TxFacts};
use purchase::SettleRequest;
use reports::{BalanceReport, Receipts, ShiftReport};

/// TCP port of the terminal's ECR interface (TIM / SIXml, OnFixIP).
pub const TIM_PORT: i32 = 7784;

/// Least time between the end of a session and the next connect to the
/// same terminal. Measured on the EftSimulator 3.34 (4.11.2026): it notices
/// the ECR's disconnect ~0.5 s later and only then listens again — a
/// connect before that is refused (`api_connect_fail_terminal`). How long a
/// real terminal needs is checked on the ETU test terminal (lane V).
pub const RECONNECT_GAP: Duration = Duration::from_secs(2);

/// One `print_option` of a terminal (`ta_print_option_create`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrintOption {
    /// `ta_e_recipient_t` ([`ffi::RECIPIENT_CARDHOLDER`], …).
    pub recipient: i32,
    /// `ta_e_print_format_t`.
    pub format: i32,
    /// Print width in characters (the SDK's default is 40).
    pub width: i32,
    /// `ta_e_print_flag_t` bits (0 = nothing suppressed).
    pub flags: i32,
}

/// The print options of EVERY session (lane V, ROZHODNUTÉ #102
/// issuecomment-6766472016): the terminal prints the cardholder AND the
/// merchant slip on its own printer (`on_device_with_receipt`) and still
/// returns their text, which the job result keeps. Set on the terminal
/// object before the connect, so before any transaction / receipt request.
pub const PRINT_OPTIONS: [PrintOption; 2] = [
    PrintOption {
        recipient: ffi::RECIPIENT_CARDHOLDER,
        format: ffi::PRINT_FORMAT_ON_DEVICE_WITH_RECEIPT,
        width: 40,
        flags: 0,
    },
    PrintOption {
        recipient: ffi::RECIPIENT_MERCHANT,
        format: ffi::PRINT_FORMAT_ON_DEVICE_WITH_RECEIPT,
        width: 40,
        flags: 0,
    },
];

/// Heartbeat states (`terminal=…`).
pub const STATUS_OK: &str = "tim-ok";
pub const STATUS_BUSY: &str = "busy";
pub const STATUS_OFFLINE: &str = "offline";

/// The settle channel of every held payment, by purchase job id.
pub(crate) type Holders = Arc<Mutex<HashMap<i64, std::sync::mpsc::Sender<SettleRequest>>>>;

/// The cancel handle of every purchase / credit IN ITS CARD WAIT (inside
/// `ta_terminal_transaction`), by job id — `card.cancel` (lane V).
pub(crate) type Cancellers = Arc<Mutex<HashMap<i64, Arc<dyn TimCanceller>>>>;

/// One till's terminal: everything its TIM settings are built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSpec {
    pub kasa_id: i64,
    pub ip: String,
    /// The terminal id the till expects (`terminal_tid`), checked after login.
    pub expected_tid: Option<String>,
    pub integrator_id: String,
    pub commit_timeout_s: u32,
}

impl TerminalSpec {
    /// POS-ID of the till on the terminal: the plain `pos.config` id
    /// (`3`) — the SDK limits it to a number of at most six digits (EP2),
    /// ROZHODNUTÉ on #102 4.11.2026; startup validation keeps it in range.
    pub fn pos_id(&self) -> String {
        self.kasa_id.to_string()
    }

    /// Our side of `tim-settings.cfg` (the library adds its own dump).
    pub fn settings_lines(&self) -> String {
        format!(
            "kasa_id = {}\npos_id = {}\nconnection_mode = on_fix_ip\nconnection_ip = {}\n\
             connection_port = {TIM_PORT}\nintegrator_id = {}\nauto_commit = false\n\
             commit_timeout_s = {}\nexpected_terminal_id = {}\n\
             print_options = {}\n",
            self.kasa_id,
            self.pos_id(),
            self.ip,
            self.integrator_id,
            self.commit_timeout_s,
            self.expected_tid.as_deref().unwrap_or("(not set)"),
            print_options_text()
        )
    }
}

/// [`PRINT_OPTIONS`] as `tim-settings.cfg` shows them.
pub fn print_options_text() -> String {
    PRINT_OPTIONS
        .iter()
        .map(|o| {
            format!(
                "recipient={} format={} width={} flags={}",
                o.recipient, o.format, o.width, o.flags
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// A failed TIM call: which function and its `ta_e_result_code_t`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimError {
    pub call: &'static str,
    pub code: i32,
}

impl TimError {
    /// Name of the result code (`ta_c_rc_<name>`, `result_code.h`).
    pub fn code_name(&self) -> &'static str {
        match self.code {
            0 => "undefined",
            1 => "ok",
            2 => "api_cancel_ecr",
            3 => "api_invalid_answer",
            5 => "api_function_disallowed",
            7 => "api_connect_fail_server",
            8 => "api_connect_fail_terminal",
            9 => "api_connection_lost_server",
            10 => "api_connection_lost_terminal",
            11 => "ethernet_disconnected",
            13 => "api_timeout_server",
            14 => "api_timeout_terminal",
            139 => "invalid_argument",
            140 => "out_of_memory",
            _ => "other",
        }
    }
}

impl std::fmt::Display for TimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} failed: rc {} ({})",
            self.call,
            self.code,
            self.code_name()
        )
    }
}

/// `Ok` for `ta_c_rc_ok`, else the call's [`TimError`].
pub fn check(call: &'static str, rc: i32) -> Result<(), TimError> {
    if rc == ffi::RC_OK {
        Ok(())
    } else {
        Err(TimError { call, code: rc })
    }
}

/// Text of a nullable NUL-terminated C string (lossy UTF-8).
///
/// # Safety
/// `ptr` is null or a NUL-terminated string valid for the call (the TIM
/// library's log record fields, or a string Rust owns).
pub unsafe fn c_text(ptr: *const std::ffi::c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller's contract above.
    Some(
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// The TIM library as devbridge uses it.
pub trait TimBackend: Send + Sync {
    /// The library's own text of the settings `spec` gives a terminal
    /// (`ta_object_to_string`) — no terminal contact.
    fn settings_text(&self, spec: &TerminalSpec) -> Result<String, TimError>;

    /// A terminal object for `spec` with its settings and POS-ID — no
    /// terminal contact yet. Dropping it disposes it. Sessions open it
    /// through [`open_terminal`] (which sets the print options).
    fn terminal(&self, spec: &TerminalSpec) -> Result<Box<dyn TimTerminal>, TimError>;
}

/// The cancel side of a terminal object, callable from ANOTHER thread
/// while the session's thread is blocked inside a request (`card.cancel`).
/// It holds its own reference to the object: a cancel after the session
/// disposed it is answered `invalid_argument`, never a dangling call.
pub trait TimCanceller: Send + Sync {
    /// `ta_terminal_cancel`: best effort, never acknowledged by the
    /// terminal — the running request returns with its cancel rc, or the
    /// terminal ignores it (no open request, or past the point of cancel).
    fn cancel(&self) -> Result<(), TimError>;
}

/// One terminal object (`ta_terminal_*`). Calls block.
pub trait TimTerminal: Send {
    /// A cancel handle of this terminal object ([`TimCanceller`]).
    fn canceller(&self) -> Result<Arc<dyn TimCanceller>, TimError>;
    /// `ta_terminal_set_print_options` (before the connect).
    fn set_print_options(&mut self, options: &[PrintOption]) -> Result<(), TimError>;
    fn connect(&mut self) -> Result<(), TimError>;
    fn login(&mut self) -> Result<(), TimError>;
    /// The id the terminal reported at login (`None` = not known).
    fn terminal_id(&mut self) -> Result<Option<String>, TimError>;
    fn logout(&mut self) -> Result<(), TimError>;
    fn disconnect(&mut self) -> Result<(), TimError>;
    /// `ta_terminal_transaction_info_request`: the latest transaction.
    fn last_transaction(&mut self) -> Result<LastTransaction, TimError>;
    /// `ta_terminal_transaction(purchase | credit, amount in EUR cents)`
    /// with AutoCommit off: the authorisation, or the terminal's refusal.
    fn transaction(&mut self, kind: TwoPhase, amount_minor: i64) -> Result<TxFacts, TimError>;
    fn commit(&mut self) -> Result<(), TimError>;
    fn rollback(&mut self) -> Result<(), TimError>;
    /// `ta_terminal_transaction2(reversal)` whose request carries ONLY
    /// `transaction_data.trans_seq` (no amount — the FAQ: further arguments
    /// may make it fail). No commit.
    fn reversal(&mut self, trans_seq: i64) -> Result<TxFacts, TimError>;
    /// `ta_terminal_activate` / `ta_terminal_deactivate` (the shift).
    fn activate(&mut self) -> Result<ShiftReport, TimError>;
    fn deactivate(&mut self) -> Result<ShiftReport, TimError>;
    /// `ta_terminal_balance` (the day closing).
    fn balance(&mut self) -> Result<BalanceReport, TimError>;
    /// `ta_terminal_receipt_request(reprint)`: the slips of the LATEST
    /// transaction.
    fn reprint(&mut self) -> Result<Receipts, TimError>;
}

/// The terminal object of EVERY session: `backend.terminal(spec)` with the
/// [`PRINT_OPTIONS`] set, so the terminal prints its own slips from the
/// first request on (lane V). A failed setter fails the session before
/// any terminal contact.
pub fn open_terminal(
    backend: &dyn TimBackend,
    spec: &TerminalSpec,
) -> Result<Box<dyn TimTerminal>, TimError> {
    let mut terminal = backend.terminal(spec)?;
    terminal.set_print_options(&PRINT_OPTIONS)?;
    Ok(terminal)
}

/// What a session read from the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReport {
    pub terminal_id: Option<String>,
    pub elapsed: Duration,
}

/// Why a session failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The TIM library is not loaded (path / symbols / `library_path` unset).
    NoLibrary(String),
    /// No till `kasa_id` with a `terminal_ip`.
    NoTerminal(i64),
    /// A TIM call failed.
    Tim(TimError),
    /// The terminal at this IP is not the till's (`terminal_tid`).
    WrongTerminal {
        expected: String,
        reported: Option<String>,
    },
    /// The session's worker thread died (a panic inside the library call).
    Aborted(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLibrary(why) => write!(f, "TIM library not loaded: {why}"),
            Self::NoTerminal(kasa) => write!(f, "till {kasa} has no terminal_ip"),
            Self::Tim(e) => write!(f, "{e}"),
            Self::WrongTerminal { expected, reported } => write!(
                f,
                "terminal reported id {} but the till expects {expected}",
                reported.as_deref().unwrap_or("(none)")
            ),
            Self::Aborted(why) => write!(f, "session aborted: {why}"),
        }
    }
}

impl From<TimError> for SessionError {
    fn from(e: TimError) -> Self {
        Self::Tim(e)
    }
}

/// One per-use session on `spec`'s terminal: connect → login → terminal
/// id (checked against `expected_tid`) → logout → disconnect. After a
/// successful connect, logout (when logged in) and disconnect ALWAYS run,
/// also when a later step failed, so the terminal is free for Codex again;
/// the terminal object is disposed when this returns.
pub fn run_session(
    backend: &dyn TimBackend,
    spec: &TerminalSpec,
) -> Result<SessionReport, SessionError> {
    let started = Instant::now();
    let mut terminal = open_terminal(backend, spec)?;
    terminal.connect()?;
    let mut logged_in = false;
    let result = (|| {
        terminal.login()?;
        logged_in = true;
        let terminal_id = terminal.terminal_id()?;
        if let Some(expected) = &spec.expected_tid
            && terminal_id.as_deref() != Some(expected.as_str())
        {
            return Err(SessionError::WrongTerminal {
                expected: expected.clone(),
                reported: terminal_id,
            });
        }
        Ok(terminal_id)
    })();
    if logged_in && let Err(e) = terminal.logout() {
        warn!(kasa_id = spec.kasa_id, terminal = %spec.ip, error = %e, "TIM logout failed");
    }
    if let Err(e) = terminal.disconnect() {
        warn!(kasa_id = spec.kasa_id, terminal = %spec.ip, error = %e, "TIM disconnect failed");
    }
    result.map(|terminal_id| SessionReport {
        terminal_id,
        elapsed: started.elapsed(),
    })
}

/// How long a connect at `now` must still wait after the session that
/// ended at `last_end` (`None` = no session yet).
pub fn reconnect_wait(last_end: Option<Instant>, now: Instant, gap: Duration) -> Duration {
    last_end.map_or(Duration::ZERO, |end| {
        (end + gap).saturating_duration_since(now)
    })
}

/// A terminal's state for the heartbeat.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalState {
    /// No failed session since start (or the last one succeeded).
    Ready,
    Busy,
    /// The last session failed.
    Failed(String),
}

/// A terminal's heartbeat state: `Busy` while a session runs or waits,
/// else the last session's result.
fn effective_state(last: &TerminalState, in_flight: usize) -> TerminalState {
    if in_flight > 0 {
        TerminalState::Busy
    } else {
        last.clone()
    }
}

/// One running-or-waiting session of a terminal; leaving it (drop) ends it.
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn enter(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(counter))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The heartbeat value of a library state and the terminals' states.
fn aggregate_status<'a>(
    library_loaded: bool,
    states: impl IntoIterator<Item = &'a TerminalState>,
) -> &'static str {
    let mut any = false;
    let mut busy = false;
    for state in states {
        any = true;
        match state {
            TerminalState::Failed(_) => return STATUS_OFFLINE,
            TerminalState::Busy => busy = true,
            TerminalState::Ready => {}
        }
    }
    match (library_loaded && any, busy) {
        (false, _) => STATUS_OFFLINE,
        (true, true) => STATUS_BUSY,
        (true, false) => STATUS_OK,
    }
}

/// The `card` executor of a `[client.pokladna.tim]` config.
pub struct TimExecutor {
    backend: Result<Arc<dyn TimBackend>, String>,
    /// Terminals by IP (one terminal = one till, startup validation).
    terminals: BTreeMap<String, TerminalSpec>,
    /// The last session's result per terminal (`Ready` / `Failed`).
    states: Mutex<BTreeMap<String, TerminalState>>,
    /// Sessions running or waiting per terminal (`busy` while > 0) — held
    /// by the blocking session itself, so a dropped caller cannot end it.
    in_flight: BTreeMap<String, Arc<AtomicUsize>>,
    /// Serialises sessions per terminal (one ECR connection at a time) and
    /// holds when the last one ended ([`RECONNECT_GAP`]).
    session_locks: BTreeMap<String, Arc<Mutex<Option<Instant>>>>,
    reconnect_gap: Duration,
    /// The source's journal (card rows, device contact) — `None` = card
    /// jobs are refused (only the D1 session primitive works).
    journal: Option<Arc<Journal>>,
    /// Rollback this long after an authorisation without `card.commit`.
    deadline: Duration,
    /// The settle channel of every payment held right now, by job id.
    holders: Holders,
    /// The cancel handle of every purchase / credit in its card wait.
    cancellers: Cancellers,
    /// `None` = the terminal's CommitTimeout (tests shorten it).
    release_within: Option<Duration>,
    /// When this executor started (a crash before it is the latest moment
    /// a held payment may have been authorised).
    started_at: chrono::DateTime<chrono::Utc>,
}

impl TimExecutor {
    /// The tills' terminals of `cfg` (`[[client.pokladna.kasy]] terminal_ip`).
    pub fn terminal_specs(cfg: &PokladnaClientConfig) -> Vec<TerminalSpec> {
        let Some(tim) = &cfg.tim else {
            return Vec::new();
        };
        cfg.kasy
            .iter()
            .filter_map(|kasa| {
                Some(TerminalSpec {
                    kasa_id: kasa.id,
                    ip: kasa.terminal_ip.clone()?,
                    expected_tid: kasa.terminal_tid.clone(),
                    integrator_id: tim.integrator_id.clone(),
                    commit_timeout_s: tim.commit_timeout_s,
                })
            })
            .collect()
    }

    /// Start TIM for `cfg` (which has `[client.pokladna.tim]`): open the
    /// log store, load the library, register the logger and write
    /// `tim-settings.cfg`. Never fails: a library that does not load is an
    /// ERROR log and a terminal reported `offline` — printing and receipts
    /// keep running. Card jobs journal into `journal`; the terminal prints
    /// its own slips ([`PRINT_OPTIONS`]).
    pub fn start(cfg: &PokladnaClientConfig, data_dir: &Path, journal: Arc<Journal>) -> Self {
        let specs = Self::terminal_specs(cfg);
        let log_dir = logs::log_dir(data_dir);
        let library_path = cfg
            .tim
            .as_ref()
            .map(|t| PathBuf::from(&t.library_path))
            .unwrap_or_default();
        let backend = native::NativeBackend::load(&library_path, &log_dir)
            .map(|b| Arc::new(b) as Arc<dyn TimBackend>);
        if let Err(why) = &backend {
            error!(library = %library_path.display(), error = %why, "TIM library not loaded - the card terminal is reported offline");
        }
        let deadline = cfg.tim.as_ref().map_or(
            devbridge_core::config::TIM_DEFAULT_ROLLBACK_DEADLINE_S,
            |t| t.rollback_deadline_s,
        );
        Self::with_backend(specs, backend, &log_dir)
            .with_journal(journal)
            .with_deadline(Duration::from_secs(u64::from(deadline)))
    }

    /// The executor over `backend` (tests pass a fake); writes
    /// `tim-settings.cfg` into `log_dir`.
    pub fn with_backend(
        specs: Vec<TerminalSpec>,
        backend: Result<Arc<dyn TimBackend>, String>,
        log_dir: &Path,
    ) -> Self {
        let terminals: BTreeMap<String, TerminalSpec> =
            specs.into_iter().map(|s| (s.ip.clone(), s)).collect();
        let states = terminals
            .keys()
            .map(|ip| (ip.clone(), TerminalState::Ready))
            .collect();
        let session_locks = terminals
            .keys()
            .map(|ip| (ip.clone(), Arc::new(Mutex::new(None))))
            .collect();
        let in_flight = terminals
            .keys()
            .map(|ip| (ip.clone(), Arc::new(AtomicUsize::new(0))))
            .collect();
        let executor = Self {
            backend,
            terminals,
            states: Mutex::new(states),
            in_flight,
            session_locks,
            reconnect_gap: RECONNECT_GAP,
            journal: None,
            deadline: Duration::from_secs(u64::from(
                devbridge_core::config::TIM_DEFAULT_ROLLBACK_DEADLINE_S,
            )),
            holders: Arc::new(Mutex::new(HashMap::new())),
            cancellers: Arc::new(Mutex::new(HashMap::new())),
            release_within: None,
            started_at: chrono::Utc::now(),
        };
        match logs::write_settings_file(log_dir, &executor.settings_dump()) {
            Ok(path) => info!(file = %path.display(), "TIM settings written"),
            Err(e) => error!(dir = %log_dir.display(), error = %e, "cannot write tim-settings.cfg"),
        }
        info!(
            library_loaded = executor.backend.is_ok(),
            terminals = ?executor.terminals.values().map(|s| (s.kasa_id, s.ip.as_str())).collect::<Vec<_>>(),
            status = executor.status(),
            "TIM card terminal executor ready (card.purchase / credit / commit / rollback / cancel / reversal / activate / deactivate / balance / print_copy)"
        );
        executor
    }

    /// The same executor with another reconnect gap (tests).
    pub fn with_reconnect_gap(mut self, gap: Duration) -> Self {
        self.reconnect_gap = gap;
        self
    }

    /// Card jobs journal into `journal`.
    pub fn with_journal(mut self, journal: Arc<Journal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Another start moment (tests: a restart long ago).
    #[cfg(test)]
    pub(crate) fn with_started_at(mut self, at: chrono::DateTime<chrono::Utc>) -> Self {
        self.started_at = at;
        self
    }

    /// Another release window after an unknown outcome (tests).
    #[cfg(test)]
    pub(crate) fn with_release_within(mut self, within: Duration) -> Self {
        self.release_within = Some(within);
        self
    }

    /// Another rollback deadline (config `rollback_deadline_s`; tests).
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// `tim-settings.cfg`: per terminal our settings, then the library's.
    fn settings_dump(&self) -> String {
        let mut out = String::from("# devbridge TIM settings at start\n");
        for problem in self.problems() {
            error!(problem = %problem, "TIM card terminal reported offline");
            out.push_str(&format!("# {problem}\n"));
        }
        for spec in self.terminals.values() {
            out.push_str(&format!(
                "\n[terminal {}]\n{}",
                spec.ip,
                spec.settings_lines()
            ));
            if let Ok(backend) = &self.backend {
                match backend.settings_text(spec) {
                    Ok(text) => out.push_str(&format!("library_settings = {text}\n")),
                    Err(e) => out.push_str(&format!("library_settings_error = {e}\n")),
                }
            }
        }
        out
    }

    /// Why the terminal is `offline` from the start (empty = it is not).
    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(why) = &self.backend {
            problems.push(format!("library not loaded: {why}"));
        }
        if self.terminals.is_empty() {
            problems.push("no till has a terminal_ip".to_string());
        }
        problems
    }

    /// The terminal of till `kasa_id`.
    fn spec_of(&self, kasa_id: i64) -> Option<&TerminalSpec> {
        self.terminals.values().find(|s| s.kasa_id == kasa_id)
    }

    fn set_state(&self, ip: &str, state: TerminalState) {
        let mut states = self.states.lock().unwrap_or_else(|p| p.into_inner());
        let previous = states.insert(ip.to_string(), state.clone());
        info!(terminal = %ip, ?previous, now = ?state, "TIM terminal state");
    }

    /// Run `work` on `spec`'s terminal on a blocking thread: one at a time
    /// per terminal (its session lock), after the reconnect gap, counted
    /// `busy` while it runs or waits. `None` = the TIM library is not
    /// loaded; `Some(Err)` = the blocking thread died (a panic in a call).
    async fn on_terminal<T: Send + 'static>(
        &self,
        spec: TerminalSpec,
        work: impl FnOnce(&dyn TimBackend, &TerminalSpec) -> T + Send + 'static,
    ) -> Option<Result<T, String>> {
        let backend = self.backend.clone().ok()?;
        let lock = Arc::clone(&self.session_locks[&spec.ip]);
        let gap = self.reconnect_gap;
        let busy = InFlight::enter(&self.in_flight[&spec.ip]);
        let joined = tokio::task::spawn_blocking(move || {
            let _busy = busy;
            let mut last_end = lock.lock().unwrap_or_else(|p| p.into_inner());
            let wait = reconnect_wait(*last_end, Instant::now(), gap);
            info!(terminal = %spec.ip, wait_ms = wait.as_millis() as u64, "TIM reconnect gap");
            std::thread::sleep(wait);
            let result = work(backend.as_ref(), &spec);
            *last_end = Some(Instant::now());
            result
        });
        Some(joined.await.map_err(|e| e.to_string()))
    }

    /// One per-use session on till `kasa_id`'s terminal ([`run_session`]),
    /// on a blocking thread, at most one per terminal at a time.
    pub async fn session(&self, kasa_id: i64) -> Result<SessionReport, SessionError> {
        if let Err(why) = &self.backend {
            return Err(SessionError::NoLibrary(why.clone()));
        }
        let spec = self
            .spec_of(kasa_id)
            .cloned()
            .ok_or(SessionError::NoTerminal(kasa_id))?;
        let ip = spec.ip.clone();
        info!(kasa_id, terminal = %ip, pos_id = %spec.pos_id(), "TIM session start");
        let result = match self
            .on_terminal(spec, |backend, spec| run_session(backend, spec))
            .await
        {
            Some(Ok(result)) => result,
            Some(Err(e)) => Err(SessionError::Aborted(e)),
            None => Err(SessionError::NoLibrary(
                "the TIM library is not loaded".into(),
            )),
        };
        match &result {
            Ok(report) => {
                info!(kasa_id, terminal = %ip, terminal_id = ?report.terminal_id, elapsed_ms = report.elapsed.as_millis() as u64, "TIM session done");
                self.set_state(&ip, TerminalState::Ready);
            }
            Err(e) => {
                warn!(kasa_id, terminal = %ip, error = %e, "TIM session failed");
                self.set_state(&ip, TerminalState::Failed(e.to_string()));
            }
        }
        result
    }
}

#[async_trait]
impl DeviceExecutor for TimExecutor {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Card
    }

    fn name(&self) -> &str {
        "tim"
    }

    async fn execute(&self, job: &DeviceJob) -> Outcome {
        self.execute_card(job).await
    }

    async fn reconcile(&self, entry: &JournalEntry) -> Reconciliation {
        self.reconcile_card(entry).await
    }

    fn status(&self) -> String {
        let states = self.states.lock().unwrap_or_else(|p| p.into_inner());
        let effective: Vec<TerminalState> = states
            .iter()
            .map(|(ip, state)| effective_state(state, self.in_flight[ip].load(Ordering::SeqCst)))
            .collect();
        aggregate_status(self.backend.is_ok(), &effective).to_string()
    }

    fn check(&self, job: &DeviceJob) -> Result<(), Rejection> {
        self.check_card(job)
    }

    /// The 23:50 safety balance (#102 D3).
    async fn background(&self) {
        self.safety_loop().await;
    }

    /// Per terminal: its own queue (purchases + their reconciles), its
    /// settle queue (commit / rollback hand their request to the held
    /// purchase, which blocks the first one) and its cancel queue
    /// (`card.cancel` must reach a purchase that blocks the terminal queue
    /// AND may block the settle queue).
    fn endpoints(&self) -> Vec<String> {
        if self.terminals.is_empty() {
            return vec![String::new()];
        }
        self.terminals
            .keys()
            .flat_map(|ip| [ip.clone(), settle_endpoint(ip), cancel_endpoint(ip)])
            .collect()
    }

    fn endpoint(&self, job: &DeviceJob) -> String {
        let ip = job
            .kasa_id
            .and_then(|k| self.spec_of(k))
            .map(|s| s.ip.clone())
            .unwrap_or_default();
        if ip.is_empty() {
            ip
        } else if jobs::is_settle(&job.job_type) {
            settle_endpoint(&ip)
        } else if job.job_type == card::JOB_CANCEL {
            cancel_endpoint(&ip)
        } else {
            ip
        }
    }
}

/// The settle queue of a terminal (`<ip>#settle`).
pub fn settle_endpoint(ip: &str) -> String {
    format!("{ip}#settle")
}

/// The cancel queue of a terminal (`<ip>#cancel`, `card.cancel`).
pub fn cancel_endpoint(ip: &str) -> String {
    format!("{ip}#cancel")
}

fn unsupported_text(job_type: &str) -> String {
    format!(
        "the TIM card terminal does not run '{job_type}' - the terminal was not contacted, nothing ran"
    )
}
