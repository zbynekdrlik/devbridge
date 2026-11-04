//! The real TIM C library behind [`super::TimBackend`]: a thin safe wrapper
//! over [`super::ffi`]. Every object devbridge creates or receives is owned
//! by an [`Object`] that releases it on drop; a [`NativeTerminal`] is
//! disposed and released on drop. Calls block — callers run them on a
//! blocking thread.

use std::ffi::c_int;
use std::path::Path;
use std::sync::Arc;

use super::card::{LastTransaction, TxFacts};
use super::ffi::{
    BOOL_FALSE, CONNECTION_ON_FIX_IP, CURRENCY_EUR, RC_OK, TRANSACTION_PURCHASE, TaObject, TaRc,
    TimLibrary,
};
use super::{TIM_PORT, TerminalSpec, TimBackend, TimError, TimTerminal, check, logs};

/// `ta_c_rcp_merchant` / `ta_c_rcp_cardholder` (`ta_e_recipient_t`).
const RECIPIENT_MERCHANT: c_int = 1;
const RECIPIENT_CARDHOLDER: c_int = 2;
/// `ta_c_rcp_both`: one receipt for both.
const RECIPIENT_BOTH: c_int = 3;

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

/// A BORROWED object a getter handed out ("is not retained"): valid while
/// the retained response it came from lives, never released.
#[derive(Clone, Copy)]
struct Borrowed(TaObject);

/// Readers of the borrowed objects of a response. Every getter is optional
/// in the library (`ta_object_invalid` = not set): a failed or empty one
/// reads as `None`, never as an error.
struct Reader<'a> {
    lib: &'a TimLibrary,
}

type GetObject = unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc;
type GetInt = unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc;

impl Reader<'_> {
    fn object(&self, parent: Borrowed, get: GetObject) -> Option<Borrowed> {
        if parent.0.is_null() {
            return None;
        }
        let mut out: TaObject = std::ptr::null_mut();
        // SAFETY: `parent` is a live object of the getter's type.
        let rc = unsafe { get(parent.0, &mut out) };
        (rc == RC_OK && !out.is_null()).then_some(Borrowed(out))
    }

    fn int(&self, parent: Borrowed, get: GetInt) -> Option<i32> {
        if parent.0.is_null() {
            return None;
        }
        let mut out: c_int = 0;
        // SAFETY: as above; the out-pointer is valid.
        let rc = unsafe { get(parent.0, &mut out) };
        (rc == RC_OK).then_some(out)
    }

    /// A string object's text (lossy UTF-8).
    fn text(&self, string: Option<Borrowed>) -> Option<String> {
        let string = string?;
        let mut ptr = std::ptr::null();
        let mut len = 0usize;
        // SAFETY: a live string object; out-pointers are valid.
        unsafe {
            if (self.lib.string_get_pointer)(string.0, &mut ptr) != RC_OK
                || (self.lib.string_get_length)(string.0, &mut len) != RC_OK
                || ptr.is_null()
            {
                return None;
            }
            // SAFETY: `len` bytes at `ptr` while the string lives.
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), len);
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
    }

    /// An integer object's value.
    fn integer(&self, integer: Option<Borrowed>) -> Option<i64> {
        let integer = integer?;
        let mut value = 0i64;
        // SAFETY: a live integer object.
        let rc = unsafe { (self.lib.integer_get_value)(integer.0, &mut value) };
        (rc == RC_OK).then_some(value)
    }

    /// The facts of a `transaction_response` devbridge keeps. The full PAN
    /// (`card_number`) and `card_ref` are never read.
    fn facts(&self, response: Borrowed) -> TxFacts {
        let lib = self.lib;
        let amount = self.object(response, lib.response_get_amount);
        let info = self.object(response, lib.response_get_transaction_information);
        let card = self.object(response, lib.response_get_card_data);
        let mut facts = TxFacts {
            transaction_type: self.int(response, lib.response_get_type),
            amount_minor: amount.and_then(|a| {
                let mut v = 0i64;
                // SAFETY: a live amount object.
                (unsafe { (lib.amount_get_value)(a.0, &mut v) } == RC_OK).then_some(v)
            }),
            currency_eur: amount
                .and_then(|a| self.int(a, lib.amount_get_currency))
                .map(|c| c == CURRENCY_EUR),
            // every field named (no `..Default`): a deleted field does not
            // compile, so no field-deletion mutant reaches this FFI reader
            trans_seq: None,
            trans_ref: None,
            trm_trans_ref: None,
            auth_code: None,
            pos_entry_mode: None,
            cvm: None,
            brand_name: None,
            card_number_printable: None,
            merchant_receipt: None,
            cardholder_receipt: None,
        };
        if let Some(info) = info {
            facts.trans_seq = self.integer(self.object(info, lib.info_get_trans_seq));
            facts.trans_ref = self.integer(self.object(info, lib.info_get_trans_ref));
            facts.trm_trans_ref = self.text(self.object(info, lib.info_get_trm_trans_ref));
            facts.auth_code = self.text(self.object(info, lib.info_get_auth_code));
            facts.pos_entry_mode = self.int(info, lib.info_get_pos_entry_mode);
            facts.cvm = self.int(info, lib.info_get_cvm);
        }
        if let Some(card) = card {
            facts.brand_name = self.text(self.object(card, lib.card_get_brand_name));
            facts.card_number_printable =
                self.text(self.object(card, lib.card_get_card_number_printable));
        }
        let receipts = self
            .object(response, lib.response_get_print_data)
            .and_then(|pd| self.object(pd, lib.print_data_get_receipts));
        if let Some(list) = receipts {
            let mut count = 0usize;
            // SAFETY: a live list object.
            if unsafe { (lib.list_get_count)(list.0, &mut count) } == RC_OK {
                for i in 0..count {
                    let mut item: TaObject = std::ptr::null_mut();
                    // SAFETY: `i < count`; the element is borrowed.
                    if unsafe { (lib.list_get_at)(list.0, i, &mut item) } != RC_OK || item.is_null()
                    {
                        continue;
                    }
                    let item = Borrowed(item);
                    let text = self.text(self.object(item, lib.receipt_get_value));
                    match self.int(item, lib.receipt_get_recipient) {
                        Some(RECIPIENT_MERCHANT) => facts.merchant_receipt = text,
                        Some(RECIPIENT_CARDHOLDER) => facts.cardholder_receipt = text,
                        Some(RECIPIENT_BOTH) => {
                            facts.merchant_receipt = text.clone();
                            facts.cardholder_receipt = text;
                        }
                        _ => {}
                    }
                }
            }
        }
        facts
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

    fn last_transaction(&mut self) -> Result<LastTransaction, TimError> {
        let lib = Arc::clone(&self.object.lib);
        let answer = Object::out(&lib, "ta_terminal_transaction_info_request", |out| unsafe {
            (lib.terminal_transaction_info_request)(self.object.raw, out)
        })?;
        let reader = Reader { lib: &lib };
        let root = Borrowed(answer.raw);
        let response = reader.object(root, lib.last_get_transaction_response);
        Ok(LastTransaction {
            transaction_type: reader.int(root, lib.last_get_transaction_type).unwrap_or(0),
            outcome: reader.int(root, lib.last_get_outcome).unwrap_or(0),
            facts: response.map(|r| reader.facts(r)),
        })
    }

    fn purchase(&mut self, amount_minor: i64) -> Result<TxFacts, TimError> {
        let lib = Arc::clone(&self.object.lib);
        let amount = Object::out(&lib, "ta_amount_create", |out| unsafe {
            (lib.amount_create)(out, amount_minor, CURRENCY_EUR)
        })?;
        let response = Object::out(&lib, "ta_terminal_transaction", |out| unsafe {
            (lib.terminal_transaction)(self.object.raw, TRANSACTION_PURCHASE, amount.raw, out)
        })?;
        Ok(Reader { lib: &lib }.facts(Borrowed(response.raw)))
    }

    fn commit(&mut self) -> Result<(), TimError> {
        self.call("ta_terminal_commit", self.object.lib.terminal_commit)
    }

    fn rollback(&mut self) -> Result<(), TimError> {
        let lib = Arc::clone(&self.object.lib);
        // the declined-receipt print data it may return is released unread
        Object::out(&lib, "ta_terminal_rollback", |out| unsafe {
            (lib.terminal_rollback)(self.object.raw, out)
        })
        .map(drop)
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
