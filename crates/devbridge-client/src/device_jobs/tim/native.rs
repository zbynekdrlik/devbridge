//! The real TIM C library behind [`super::TimBackend`]: a thin safe wrapper
//! over [`super::ffi`]. Every object devbridge creates or receives is owned
//! by an [`Object`] that releases it on drop; a [`NativeTerminal`] is
//! disposed and released on drop. Calls block — callers run them on a
//! blocking thread.

use std::ffi::c_int;
use std::path::Path;
use std::sync::Arc;

use super::ffi::{BOOL_FALSE, CONNECTION_ON_FIX_IP, RC_OK, TaObject, TaRc, TimLibrary};
use super::{TIM_PORT, TerminalSpec, TimBackend, TimError, TimTerminal, check, logs};

/// A retained `ta_object_t`, released on drop.
struct Object {
    lib: Arc<TimLibrary>,
    raw: TaObject,
}

impl Object {
    /// Take ownership of a retained object written by `fill`.
    fn out(
        lib: &Arc<TimLibrary>,
        call: &'static str,
        fill: impl FnOnce(*mut TaObject) -> TaRc,
    ) -> Result<Self, TimError> {
        let mut raw: TaObject = std::ptr::null_mut();
        let rc = fill(&mut raw);
        let object = Self {
            lib: Arc::clone(lib),
            raw,
        };
        check(call, rc).map(|()| object)
    }

    fn string(lib: &Arc<TimLibrary>, text: &str) -> Result<Self, TimError> {
        Self::out(lib, "ta_string_create", |out| {
            // SAFETY: `text` is valid for `len` bytes; the library copies it.
            unsafe { (lib.string_create)(out, text.as_ptr().cast(), text.len()) }
        })
    }

    /// The text of a string object (lossy UTF-8).
    fn text(&self) -> Result<String, TimError> {
        let mut ptr = std::ptr::null();
        let mut len = 0usize;
        // SAFETY: `raw` is a live string object; out-pointers are valid.
        check("ta_string_get_pointer", unsafe {
            (self.lib.string_get_pointer)(self.raw, &mut ptr)
        })?;
        check("ta_string_get_length", unsafe {
            (self.lib.string_get_length)(self.raw, &mut len)
        })?;
        if ptr.is_null() {
            return Ok(String::new());
        }
        // SAFETY: the library guarantees `len` bytes at `ptr` while the
        // string object lives (it does — `self` holds it).
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    fn is_valid(&self) -> bool {
        !self.raw.is_null()
    }
}

// SAFETY: a TIM object is a reference-counted handle of the library, which
// runs its own threads over the same objects; devbridge moves a handle to
// ONE blocking thread at a time (a session is created, used and dropped
// inside one `spawn_blocking` closure) and never shares it.
unsafe impl Send for Object {}

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: we own one retain of `raw` (null is accepted).
        unsafe { (self.lib.object_release_if_valid)(self.raw) };
    }
}

/// The loaded library.
pub struct NativeBackend {
    lib: Arc<TimLibrary>,
}

impl NativeBackend {
    /// Load the library at `path` and route its log records into the daily
    /// files of `log_dir` ([`logs::install_global_logger`]).
    pub fn load(path: &Path, log_dir: &Path) -> Result<Self, String> {
        if path.as_os_str().is_empty() {
            return Err("tim.library_path is empty".to_string());
        }
        let lib = Arc::new(TimLibrary::load(path)?);
        logs::install_global_logger(&lib, log_dir)
            .map_err(|e| format!("TIM log store {}: {e}", log_dir.display()))?;
        tracing::info!(library = %path.display(), log_dir = %log_dir.display(), "TIM library loaded");
        Ok(Self { lib })
    }

    /// The settings object of `spec`: OnFixIP `ip:7784`, integrator ID,
    /// auto-commit OFF, commit timeout.
    fn settings(&self, spec: &TerminalSpec) -> Result<Object, TimError> {
        let lib = &self.lib;
        let settings = Object::out(lib, "ta_terminal_settings_create", |out| unsafe {
            (lib.settings_create)(out)
        })?;
        let s = settings.raw;
        let ip = Object::string(lib, &spec.ip)?;
        let integrator = Object::string(lib, &spec.integrator_id)?;
        let timeout = c_int::try_from(spec.commit_timeout_s).unwrap_or(c_int::MAX);
        // SAFETY: `s`, `ip`, `integrator` are live objects of the right type;
        // setters retain what they keep.
        unsafe {
            check(
                "ta_terminal_settings_set_connection_mode",
                (lib.settings_set_connection_mode)(s, CONNECTION_ON_FIX_IP),
            )?;
            check(
                "ta_terminal_settings_set_connection_ip_string",
                (lib.settings_set_connection_ip_string)(s, ip.raw),
            )?;
            check(
                "ta_terminal_settings_set_connection_ip_port",
                (lib.settings_set_connection_ip_port)(s, TIM_PORT),
            )?;
            check(
                "ta_terminal_settings_set_integrator_id",
                (lib.settings_set_integrator_id)(s, integrator.raw),
            )?;
            check(
                "ta_terminal_settings_set_auto_commit",
                (lib.settings_set_auto_commit)(s, BOOL_FALSE),
            )?;
            check(
                "ta_terminal_settings_set_commit_timeout",
                (lib.settings_set_commit_timeout)(s, timeout),
            )?;
        }
        Ok(settings)
    }
}

impl TimBackend for NativeBackend {
    fn settings_text(&self, spec: &TerminalSpec) -> Result<String, TimError> {
        let settings = self.settings(spec)?;
        let lib = &self.lib;
        let text = Object::out(lib, "ta_object_to_string", |out| unsafe {
            (lib.object_to_string)(settings.raw, out)
        })?;
        text.text()
    }

    fn terminal(&self, spec: &TerminalSpec) -> Result<Box<dyn TimTerminal>, TimError> {
        let settings = self.settings(spec)?;
        let lib = &self.lib;
        // the terminal keeps its own copy of the settings (terminal.h)
        let object = Object::out(lib, "ta_terminal_create", |out| unsafe {
            (lib.terminal_create)(out, settings.raw)
        })?;
        let terminal = NativeTerminal { object };
        let pos_id = Object::string(lib, &spec.pos_id())?;
        // SAFETY: both are live objects; the terminal retains the POS-ID.
        check("ta_terminal_set_pos_id", unsafe {
            (lib.terminal_set_pos_id)(terminal.object.raw, pos_id.raw)
        })?;
        Ok(Box::new(terminal))
    }
}

/// A `ta_terminal` object, disposed and released on drop.
struct NativeTerminal {
    object: Object,
}

impl NativeTerminal {
    fn call(
        &self,
        call: &'static str,
        f: unsafe extern "C" fn(TaObject) -> TaRc,
    ) -> Result<(), TimError> {
        // SAFETY: `object` is a live, not yet disposed terminal.
        check(call, unsafe { f(self.object.raw) })
    }
}

impl TimTerminal for NativeTerminal {
    fn connect(&mut self) -> Result<(), TimError> {
        self.call("ta_terminal_connect", self.object.lib.terminal_connect)
    }

    fn login(&mut self) -> Result<(), TimError> {
        self.call("ta_terminal_login", self.object.lib.terminal_login)
    }

    fn terminal_id(&mut self) -> Result<Option<String>, TimError> {
        let lib = &self.object.lib;
        let id = Object::out(lib, "ta_terminal_get_terminal_id", |out| unsafe {
            (lib.terminal_get_terminal_id)(self.object.raw, out)
        })?;
        if !id.is_valid() {
            return Ok(None);
        }
        id.text().map(Some)
    }

    fn logout(&mut self) -> Result<(), TimError> {
        self.call("ta_terminal_logout", self.object.lib.terminal_logout)
    }

    fn disconnect(&mut self) -> Result<(), TimError> {
        self.call(
            "ta_terminal_disconnect",
            self.object.lib.terminal_disconnect,
        )
    }
}

impl Drop for NativeTerminal {
    fn drop(&mut self) {
        // SAFETY: disposing a live terminal once; the Object then releases it.
        let rc = unsafe { (self.object.lib.terminal_dispose)(self.object.raw) };
        if rc != RC_OK {
            tracing::warn!(rc, "ta_terminal_dispose failed");
        }
    }
}
