//! The TIM log store (#102 D1, design odoo-erp issue 7598 §3): every log
//! record the TIM library publishes (all levels, no filter — the Worldline
//! validation asks for the finest log; TIM 3.42 publishes up to FINE) goes to `<data dir>/logs/tim/tim-YYYY-MM-DD.log`
//! (local date). Files of past days are gzipped (`.log.gz`) and the `.gz`
//! files older than [`RETENTION_DAYS`] are deleted — at start and on every
//! day change. The logs never leave the store PC.

use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Local, NaiveDate};

use super::ffi::{LogRecord, TimLibrary};

/// Days a gzipped TIM log is kept (≥ 3 months, design §3).
pub const RETENTION_DAYS: i64 = 100;
/// Name of the settings dump written at start.
pub const SETTINGS_FILE: &str = "tim-settings.cfg";

/// `<data dir>/logs/tim`.
pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("tim")
}

/// `tim-YYYY-MM-DD.log`.
pub fn file_name(date: NaiveDate) -> String {
    format!("tim-{}.log", date.format("%Y-%m-%d"))
}

/// The date of a TIM log file name and whether it is gzipped.
pub fn parse_file_name(name: &str) -> Option<(NaiveDate, bool)> {
    let rest = name.strip_prefix("tim-")?;
    let (date, gz) = match rest.strip_suffix(".log.gz") {
        Some(date) => (date, true),
        None => (rest.strip_suffix(".log")?, false),
    };
    if date.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .map(|d| (d, gz))
}

/// TIM's level name (`ta_e_log_level_t`).
pub fn level_name(level: i32) -> &'static str {
    match level {
        0 => "OFF",
        1 => "SEVERE",
        2 => "WARNING",
        3 => "INFO",
        4 => "FINE",
        5 => "FINEST",
        _ => "LEVEL?",
    }
}

/// One log line: `<local RFC 3339 ms> <LEVEL> [<thread>] <method>: <message>`
/// plus ` params=[a, b]` when the record carries parameters.
pub fn format_record(
    now: DateTime<Local>,
    level: i32,
    thread: u32,
    method: Option<&str>,
    message: &str,
    parameters: &[String],
) -> String {
    let params = match parameters {
        [] => String::new(),
        p => format!(" params=[{}]", p.join(", ")),
    };
    format!(
        "{} {} [{thread}] {}: {message}{params}\n",
        now.format("%Y-%m-%dT%H:%M:%S%.3f%:z"),
        level_name(level),
        method.unwrap_or("-")
    )
}

/// What one housekeeping pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Housekeeping {
    pub compressed: Vec<String>,
    pub deleted: Vec<String>,
}

/// Gzip every TIM log of a day before `today` and delete the gzipped ones
/// older than [`RETENTION_DAYS`]. Other files are never touched.
pub fn housekeep(dir: &Path, today: NaiveDate) -> io::Result<Housekeeping> {
    let mut done = Housekeeping::default();
    let oldest_kept = today - chrono::Duration::days(RETENTION_DAYS);
    let mut names: Vec<String> = fs::read_dir(dir)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        let Some((date, gz)) = parse_file_name(&name) else {
            continue;
        };
        let path = dir.join(&name);
        let gz_name = if gz {
            name.clone()
        } else if date < today {
            let gz_name = format!("{name}.gz");
            compress(&path, &dir.join(&gz_name))?;
            done.compressed.push(name);
            gz_name
        } else {
            continue;
        };
        if date < oldest_kept {
            fs::remove_file(dir.join(&gz_name))?;
            done.deleted.push(gz_name);
        }
    }
    Ok(done)
}

/// `src` → `dst` (gzip), through `dst.tmp` + rename, then `src` removed.
fn compress(src: &Path, dst: &Path) -> io::Result<()> {
    let tmp = dst.with_extension("gz.tmp");
    {
        let mut input = File::open(src)?;
        let mut encoder =
            flate2::write::GzEncoder::new(File::create(&tmp)?, flate2::Compression::default());
        io::copy(&mut input, &mut encoder)?;
        encoder.finish()?.sync_all()?;
    }
    fs::rename(&tmp, dst)?;
    fs::remove_file(src)
}

/// The daily log files: appends to the file of the record's day and
/// housekeeps when the day changes.
pub struct DailyLog {
    dir: PathBuf,
    open: Option<(NaiveDate, File)>,
}

impl DailyLog {
    pub fn new(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            open: None,
        })
    }

    /// Append `line` to the file of `now`'s day. A new day opens its file
    /// at once; the past days are housekept on a thread of their own, so a
    /// TIM thread never waits for a gzip and a housekeeping error never
    /// costs a record.
    pub fn write(&mut self, now: DateTime<Local>, line: &str) -> io::Result<()> {
        let today = now.date_naive();
        if self.open.as_ref().is_none_or(|(day, _)| *day != today) {
            self.open = None;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(file_name(today)))?;
            self.open = Some((today, file));
            let dir = self.dir.clone();
            std::thread::spawn(move || housekeep_logged(&dir, today));
        }
        let (_, file) = self.open.as_mut().expect("opened above");
        file.write_all(line.as_bytes())
    }
}

/// Serialises housekeeping passes (start-up and day changes).
static HOUSEKEEP: Mutex<()> = Mutex::new(());

/// [`housekeep`] with its outcome logged; an error is a WARN, never fatal.
pub fn housekeep_logged(dir: &Path, today: NaiveDate) {
    let _one_at_a_time = HOUSEKEEP.lock().unwrap_or_else(|p| p.into_inner());
    let result = housekeep(dir, today);
    tracing::info!(dir = %dir.display(), day = %today, result = ?result, "TIM logs housekept");
}

/// Write `tim-settings.cfg` into `dir` (created when missing).
pub fn write_settings_file(dir: &Path, text: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(SETTINGS_FILE);
    fs::write(&path, text)?;
    Ok(path)
}

/// The process-wide sink of the TIM logger (one library, one callback).
static SINK: Mutex<Option<DailyLog>> = Mutex::new(None);

/// Route the process-wide TIM log records into `dir` (housekept now; only
/// a directory that cannot be created fails).
pub fn set_global_sink(dir: &Path) -> io::Result<()> {
    let log = DailyLog::new(dir)?;
    housekeep_logged(dir, Local::now().date_naive());
    *SINK.lock().unwrap_or_else(|p| p.into_inner()) = Some(log);
    Ok(())
}

/// Point the TIM logger at `dir` ([`set_global_sink`]) and register the
/// callback with `lib` (again on every load: harmless, and a library at
/// another path gets it too).
pub fn install_global_logger(lib: &TimLibrary, dir: &Path) -> io::Result<()> {
    set_global_sink(dir)?;
    // SAFETY: `on_log_record` matches `ta_cb_publish_log_record` and lives
    // for the whole process.
    unsafe { (lib.logger_set_global_logger)(Some(on_log_record), std::ptr::null_mut()) };
    Ok(())
}

/// Append one line to the global sink (no sink = dropped).
pub fn write_global(now: DateTime<Local>, line: &str) {
    let mut sink = SINK.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(log) = sink.as_mut()
        && let Err(e) = log.write(now, line)
    {
        tracing::warn!(dir = %log.dir.display(), error = %e, "cannot write a TIM log record");
    }
}

/// The C callback: called from the library's threads; never unwinds into C.
unsafe extern "C" fn on_log_record(record: *const LogRecord, _user: *mut c_void) {
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: the library passes a valid record for the call's duration.
        let Some(record) = (unsafe { record.as_ref() }) else {
            return;
        };
        let count = usize::try_from(record.parameter_count).unwrap_or(0);
        let parameters: Vec<String> = if record.parameters.is_null() {
            Vec::new()
        } else {
            // SAFETY: `parameters` holds `parameter_count` C strings (logging.h).
            unsafe { std::slice::from_raw_parts(record.parameters, count) }
                .iter()
                .map(|p| unsafe { super::c_text(*p) }.unwrap_or_default())
                .collect()
        };
        let now = Local::now();
        let line = format_record(
            now,
            record.level,
            record.thread,
            unsafe { super::c_text(record.method) }.as_deref(),
            &unsafe { super::c_text(record.message) }.unwrap_or_default(),
            &parameters,
        );
        write_global(now, &line);
    });
}
