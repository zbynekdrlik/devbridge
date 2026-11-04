//! The Worldline card terminal through the TIM API C library (#102).
//!
//! Lane D1 (design odoo-erp issue 7598 §9): the binding, the per-use
//! session, the TIM logs and the heartbeat state. It runs NO `card.*` job
//! yet — [`TimExecutor::check`] refuses every one `unsupported` before it
//! is journaled ("nothing ran"); lane D2 adds purchase / commit / rollback.
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

pub mod ffi;
pub mod logs;
pub mod native;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use devbridge_core::config::PokladnaClientConfig;
use tracing::{error, info, warn};

use super::api::DeviceJob;
use super::executor::{
    CODE_UNSUPPORTED, DeviceExecutor, DeviceKind, Outcome, Reconciliation, Rejection,
};
use super::journal::JournalEntry;

/// TCP port of the terminal's ECR interface (TIM / SIXml, OnFixIP).
pub const TIM_PORT: i32 = 7784;

/// Least time between the end of a session and the next connect to the
/// same terminal. Measured on the EftSimulator 3.34 (4.11.2026): it notices
/// the ECR's disconnect ~0.5 s later and only then listens again — a
/// connect before that is refused (`api_connect_fail_terminal`). How long a
/// real terminal needs is checked on the ETU test terminal (lane V).
pub const RECONNECT_GAP: Duration = Duration::from_secs(2);

/// Heartbeat states (`terminal=…`).
pub const STATUS_OK: &str = "tim-ok";
pub const STATUS_BUSY: &str = "busy";
pub const STATUS_OFFLINE: &str = "offline";

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
    /// POS-ID of the till on the terminal (`KASA-<pos.config id>`).
    pub fn pos_id(&self) -> String {
        format!("KASA-{}", self.kasa_id)
    }

    /// Our side of `tim-settings.cfg` (the library adds its own dump).
    pub fn settings_lines(&self) -> String {
        format!(
            "kasa_id = {}\npos_id = {}\nconnection_mode = on_fix_ip\nconnection_ip = {}\n\
             connection_port = {TIM_PORT}\nintegrator_id = {}\nauto_commit = false\n\
             commit_timeout_s = {}\nexpected_terminal_id = {}\n",
            self.kasa_id,
            self.pos_id(),
            self.ip,
            self.integrator_id,
            self.commit_timeout_s,
            self.expected_tid.as_deref().unwrap_or("(not set)")
        )
    }
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

/// The TIM library as devbridge uses it.
pub trait TimBackend: Send + Sync {
    /// The library's own text of the settings `spec` gives a terminal
    /// (`ta_object_to_string`) — no terminal contact.
    fn settings_text(&self, spec: &TerminalSpec) -> Result<String, TimError>;

    /// A terminal object for `spec` with its settings and POS-ID — no
    /// terminal contact yet. Dropping it disposes it.
    fn terminal(&self, spec: &TerminalSpec) -> Result<Box<dyn TimTerminal>, TimError>;
}

/// One terminal object (`ta_terminal_*`). Calls block.
pub trait TimTerminal: Send {
    fn connect(&mut self) -> Result<(), TimError>;
    fn login(&mut self) -> Result<(), TimError>;
    /// The id the terminal reported at login (`None` = not known).
    fn terminal_id(&mut self) -> Result<Option<String>, TimError>;
    fn logout(&mut self) -> Result<(), TimError>;
    fn disconnect(&mut self) -> Result<(), TimError>;
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
    let mut terminal = backend.terminal(spec)?;
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
    states: Mutex<BTreeMap<String, TerminalState>>,
    /// Serialises sessions per terminal (one ECR connection at a time) and
    /// holds when the last one ended ([`RECONNECT_GAP`]).
    session_locks: BTreeMap<String, Arc<Mutex<Option<Instant>>>>,
    reconnect_gap: Duration,
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
    /// keep running.
    pub fn start(cfg: &PokladnaClientConfig, data_dir: &Path) -> Self {
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
        Self::with_backend(specs, backend, &log_dir)
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
        let executor = Self {
            backend,
            terminals,
            states: Mutex::new(states),
            session_locks,
            reconnect_gap: RECONNECT_GAP,
        };
        match logs::write_settings_file(log_dir, &executor.settings_dump()) {
            Ok(path) => info!(file = %path.display(), "TIM settings written"),
            Err(e) => error!(dir = %log_dir.display(), error = %e, "cannot write tim-settings.cfg"),
        }
        info!(
            library_loaded = executor.backend.is_ok(),
            terminals = ?executor.terminals.values().map(|s| (s.kasa_id, s.ip.as_str())).collect::<Vec<_>>(),
            status = executor.status(),
            "TIM card terminal executor ready (D1: card.* jobs are refused unsupported)"
        );
        executor
    }

    /// The same executor with another reconnect gap (tests).
    pub fn with_reconnect_gap(mut self, gap: Duration) -> Self {
        self.reconnect_gap = gap;
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

    /// One per-use session on till `kasa_id`'s terminal ([`run_session`]),
    /// on a blocking thread, at most one per terminal at a time.
    pub async fn session(&self, kasa_id: i64) -> Result<SessionReport, SessionError> {
        let backend = self.backend.clone().map_err(SessionError::NoLibrary)?;
        let spec = self
            .spec_of(kasa_id)
            .cloned()
            .ok_or(SessionError::NoTerminal(kasa_id))?;
        let lock = Arc::clone(&self.session_locks[&spec.ip]);
        let ip = spec.ip.clone();
        let gap = self.reconnect_gap;
        info!(kasa_id, terminal = %ip, pos_id = %spec.pos_id(), "TIM session start");
        let joined = tokio::task::spawn_blocking(move || {
            let mut last_end = lock.lock().unwrap_or_else(|p| p.into_inner());
            let wait = reconnect_wait(*last_end, Instant::now(), gap);
            info!(terminal = %spec.ip, wait_ms = wait.as_millis() as u64, "TIM reconnect gap");
            std::thread::sleep(wait);
            let result = run_session(backend.as_ref(), &spec);
            *last_end = Some(Instant::now());
            result
        });
        self.set_state(&ip, TerminalState::Busy);
        let result = joined
            .await
            .unwrap_or_else(|e| Err(SessionError::Aborted(e.to_string())));
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

    /// Never reached in D1 ([`Self::check`] refuses every job first); the
    /// terminal is not contacted here, so "nothing ran" is the truth.
    async fn execute(&self, job: &DeviceJob) -> Outcome {
        Outcome::failed(CODE_UNSUPPORTED, unsupported_text(&job.job_type))
    }

    /// D1 never runs a job, so it cannot know a card job's outcome — the
    /// row stays unknown for D2's terminal reconcile.
    async fn reconcile(&self, entry: &JournalEntry) -> Reconciliation {
        Reconciliation::StillUnknown(format!(
            "the TIM executor cannot read the outcome of job {} yet",
            entry.job.id
        ))
    }

    fn status(&self) -> String {
        let states = self.states.lock().unwrap_or_else(|p| p.into_inner());
        aggregate_status(self.backend.is_ok(), states.values()).to_string()
    }

    fn check(&self, job: &DeviceJob) -> Result<(), Rejection> {
        Err(Rejection {
            code: CODE_UNSUPPORTED,
            text: unsupported_text(&job.job_type),
        })
    }

    fn endpoints(&self) -> Vec<String> {
        if self.terminals.is_empty() {
            return vec![String::new()];
        }
        self.terminals.keys().cloned().collect()
    }

    fn endpoint(&self, job: &DeviceJob) -> String {
        job.kasa_id
            .and_then(|k| self.spec_of(k))
            .map(|s| s.ip.clone())
            .unwrap_or_default()
    }
}

fn unsupported_text(job_type: &str) -> String {
    format!(
        "the TIM card terminal does not run '{job_type}' yet (devbridge issue 102, lane D2) - the terminal was not contacted, nothing ran"
    )
}
