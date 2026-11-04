//! The subset of the Worldline TIM API C library devbridge calls (#102 D1),
//! written by hand from the SDK's C headers (TimSdk 26-09,
//! `C/TimApi/Include/timapi/`). No header text, generated binding or SDK
//! binary is part of this repository: the library is opened at RUNTIME
//! with `libloading` from `[client.pokladna.tim] library_path`.
//!
//! ABI facts taken from the headers:
//! - every object is an opaque `ta_object_t` (`struct _ta_object *`),
//!   `ta_object_invalid` = null; a function that hands one out RETAINS it,
//!   the caller releases it (`ta_object_release_if_valid`);
//! - every function returns `ta_e_result_code_t` (a C enum = `int`),
//!   `ta_c_rc_ok` = 1; no calling-convention macro (cdecl, the only x64 one);
//! - `ta_e_boolean_t`: undefined 0, true 1, false 2 (false is NOT 0);
//! - `ta_e_connection_mode_t`: broadcast 0, on_fix_ip 1;
//! - `ta_s_log_record_t` layout and `ta_cb_publish_log_record` (`logging.h`).

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::path::Path;

/// `ta_object_t`.
pub type TaObject = *mut c_void;
/// `ta_e_result_code_t`.
pub type TaRc = c_int;

/// `ta_c_rc_ok`.
pub const RC_OK: TaRc = 1;
/// `ta_c_b_false`.
pub const BOOL_FALSE: c_int = 2;
/// `ta_c_cm_on_fix_ip`.
pub const CONNECTION_ON_FIX_IP: c_int = 1;

/// `ta_s_log_record_t` (`logging.h`). Only `level`, `thread`, `method` and
/// `message` are read; the rest keeps the layout exact.
#[repr(C)]
pub struct LogRecord {
    pub level: c_int,
    pub thread: c_uint,
    pub file: *const c_char,
    pub method: *const c_char,
    pub line: c_int,
    pub message: *const c_char,
    pub parameters: *const *const c_char,
    pub parameter_count: c_int,
    pub stack_trace: *const c_void,
    pub stack_trace_count: c_int,
    /// `time_t` — 64-bit on Windows x64 (MSVC) and Linux x86-64.
    pub timestamp: i64,
}

/// `ta_cb_publish_log_record`.
pub type LogCallback = unsafe extern "C" fn(record: *const LogRecord, user: *mut c_void);

/// The loaded library: every function devbridge uses, resolved at load
/// time (a missing symbol fails the load, never a later call).
pub struct TimLibrary {
    pub object_release_if_valid: unsafe extern "C" fn(TaObject) -> TaRc,
    pub object_to_string: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub string_create: unsafe extern "C" fn(*mut TaObject, *const c_char, usize) -> TaRc,
    pub string_get_pointer: unsafe extern "C" fn(TaObject, *mut *const c_char) -> TaRc,
    pub string_get_length: unsafe extern "C" fn(TaObject, *mut usize) -> TaRc,
    pub settings_create: unsafe extern "C" fn(*mut TaObject) -> TaRc,
    pub settings_set_connection_mode: unsafe extern "C" fn(TaObject, c_int) -> TaRc,
    pub settings_set_connection_ip_string: unsafe extern "C" fn(TaObject, TaObject) -> TaRc,
    pub settings_set_connection_ip_port: unsafe extern "C" fn(TaObject, c_int) -> TaRc,
    pub settings_set_integrator_id: unsafe extern "C" fn(TaObject, TaObject) -> TaRc,
    pub settings_set_auto_commit: unsafe extern "C" fn(TaObject, c_int) -> TaRc,
    pub settings_set_commit_timeout: unsafe extern "C" fn(TaObject, c_int) -> TaRc,
    pub terminal_create: unsafe extern "C" fn(*mut TaObject, TaObject) -> TaRc,
    pub terminal_dispose: unsafe extern "C" fn(TaObject) -> TaRc,
    pub terminal_set_pos_id: unsafe extern "C" fn(TaObject, TaObject) -> TaRc,
    pub terminal_get_terminal_id: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub terminal_connect: unsafe extern "C" fn(TaObject) -> TaRc,
    pub terminal_login: unsafe extern "C" fn(TaObject) -> TaRc,
    pub terminal_logout: unsafe extern "C" fn(TaObject) -> TaRc,
    pub terminal_disconnect: unsafe extern "C" fn(TaObject) -> TaRc,
    pub logger_set_global_logger: unsafe extern "C" fn(Option<LogCallback>, *mut c_void),
    /// Keeps the code above mapped; never unloaded while the process runs
    /// (the library's own threads call our logger).
    _library: libloading::Library,
}

impl TimLibrary {
    /// Open the library at `path` and resolve every symbol.
    pub fn load(path: &Path) -> Result<Self, String> {
        // SAFETY: loading a library runs its initialisers; the path is the
        // operator-configured Worldline TIM library and nothing else.
        let library = unsafe { libloading::Library::new(path) }
            .map_err(|e| format!("cannot load the TIM library {}: {e}", path.display()))?;
        macro_rules! sym {
            ($name:literal) => {{
                // SAFETY: the type above is the header's signature of $name.
                let symbol = unsafe { library.get(concat!($name, "\0").as_bytes()) }
                    .map_err(|e| format!("TIM library {} has no {}: {e}", path.display(), $name))?;
                *symbol
            }};
        }
        Ok(Self {
            object_release_if_valid: sym!("ta_object_release_if_valid"),
            object_to_string: sym!("ta_object_to_string"),
            string_create: sym!("ta_string_create"),
            string_get_pointer: sym!("ta_string_get_pointer"),
            string_get_length: sym!("ta_string_get_length"),
            settings_create: sym!("ta_terminal_settings_create"),
            settings_set_connection_mode: sym!("ta_terminal_settings_set_connection_mode"),
            settings_set_connection_ip_string: sym!(
                "ta_terminal_settings_set_connection_ip_string"
            ),
            settings_set_connection_ip_port: sym!("ta_terminal_settings_set_connection_ip_port"),
            settings_set_integrator_id: sym!("ta_terminal_settings_set_integrator_id"),
            settings_set_auto_commit: sym!("ta_terminal_settings_set_auto_commit"),
            settings_set_commit_timeout: sym!("ta_terminal_settings_set_commit_timeout"),
            terminal_create: sym!("ta_terminal_create"),
            terminal_dispose: sym!("ta_terminal_dispose"),
            terminal_set_pos_id: sym!("ta_terminal_set_pos_id"),
            terminal_get_terminal_id: sym!("ta_terminal_get_terminal_id"),
            terminal_connect: sym!("ta_terminal_connect"),
            terminal_login: sym!("ta_terminal_login"),
            terminal_logout: sym!("ta_terminal_logout"),
            terminal_disconnect: sym!("ta_terminal_disconnect"),
            logger_set_global_logger: sym!("ta_logger_set_global_logger"),
            _library: library,
        })
    }
}
