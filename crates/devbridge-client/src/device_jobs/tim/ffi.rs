//! The subset of the Worldline TIM API C library devbridge calls (#102 D1-D3, lane V),
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
//! - a GETTER of a response object (`ta_transaction_response_get_*`,
//!   `ta_card_data_get_*`, `ta_list_get_at`, …) hands out a BORROWED object
//!   ("is not retained"): never released by devbridge, valid while its
//!   parent lives. The transaction / info / rollback / activate /
//!   deactivate / balance / receipt-request responses and every created
//!   object (amount, integer, transaction request / data, list, print
//!   option) are retained
//!   (released by devbridge); a setter retains what it keeps;
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
/// `ta_c_tt_purchase` / `ta_c_tt_credit` / `ta_c_tt_reversal`
/// (`ta_e_transaction_type_t`).
pub const TRANSACTION_PURCHASE: c_int = 1;
pub const TRANSACTION_CREDIT: c_int = 2;
pub const TRANSACTION_REVERSAL: c_int = 3;
/// `ta_c_rrt_reprint` (`ta_e_receipt_request_type_t`): the latest receipt.
pub const RECEIPT_REQUEST_REPRINT: c_int = 1;
/// `ta_c_rcp_merchant` / `ta_c_rcp_cardholder` / `ta_c_rcp_both`
/// (`ta_e_recipient_t`).
pub const RECIPIENT_MERCHANT: c_int = 1;
pub const RECIPIENT_CARDHOLDER: c_int = 2;
pub const RECIPIENT_BOTH: c_int = 3;
/// `ta_c_pfmt_on_device_with_receipt` (`ta_e_print_format_t`): the slip is
/// printed on the terminal's own printer AND its text returned to the ECR
/// (lane V, ROZHODNUTÉ #102 issuecomment-6766472016).
pub const PRINT_FORMAT_ON_DEVICE_WITH_RECEIPT: c_int = 5;
/// `ta_c_currency_eur` — the 51st enumerator of `ta_e_currency_t` (counted
/// from `ta_c_currency_undefined` = 0; the simulator echoes `currency=EUR`).
pub const CURRENCY_EUR: c_int = 50;

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
    // lane D2: the transaction and what its responses hold
    pub terminal_transaction:
        unsafe extern "C" fn(TaObject, c_int, TaObject, *mut TaObject) -> TaRc,
    pub terminal_commit: unsafe extern "C" fn(TaObject) -> TaRc,
    pub terminal_rollback: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub terminal_transaction_info_request: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub amount_create: unsafe extern "C" fn(*mut TaObject, i64, c_int) -> TaRc,
    pub amount_get_value: unsafe extern "C" fn(TaObject, *mut i64) -> TaRc,
    pub amount_get_currency: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub integer_get_value: unsafe extern "C" fn(TaObject, *mut i64) -> TaRc,
    pub list_get_count: unsafe extern "C" fn(TaObject, *mut usize) -> TaRc,
    pub list_get_at: unsafe extern "C" fn(TaObject, usize, *mut TaObject) -> TaRc,
    pub response_get_type: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub response_get_amount: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub response_get_transaction_information: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub response_get_card_data: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub response_get_print_data: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub info_get_pos_entry_mode: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub info_get_cvm: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub info_get_auth_code: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub info_get_trans_ref: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub info_get_trans_seq: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub info_get_trm_trans_ref: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub card_get_brand_name: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub card_get_card_number_printable: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub print_data_get_receipts: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub receipt_get_recipient: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub receipt_get_value: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub last_get_transaction_type: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub last_get_outcome: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub last_get_transaction_response: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    // lane D3: reversal, shift, balance, receipt copy
    pub terminal_transaction2:
        unsafe extern "C" fn(TaObject, c_int, TaObject, *mut TaObject) -> TaRc,
    pub transaction_request_create: unsafe extern "C" fn(*mut TaObject) -> TaRc,
    pub transaction_request_set_transaction_data: unsafe extern "C" fn(TaObject, TaObject) -> TaRc,
    pub transaction_data_create: unsafe extern "C" fn(*mut TaObject) -> TaRc,
    pub transaction_data_set_trans_seq: unsafe extern "C" fn(TaObject, TaObject) -> TaRc,
    pub integer_create: unsafe extern "C" fn(*mut TaObject, i64) -> TaRc,
    pub terminal_activate: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub terminal_deactivate: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub terminal_balance: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub terminal_receipt_request: unsafe extern "C" fn(TaObject, c_int, *mut TaObject) -> TaRc,
    pub activate_get_print_data: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub activate_get_act_seq_counter: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub deactivate_get_print_data: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub deactivate_get_counters: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub balance_get_print_data: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub balance_get_counters: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub receipt_response_get_print_data: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub counters_get_seq_counter: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub counters_get_counters: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub counter_get_brand_name: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub counter_get_acq_id: unsafe extern "C" fn(TaObject, *mut i64) -> TaRc,
    pub counter_get_count: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub counter_get_totals: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub total_get_count: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub total_get_amount_sum: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub total_get_currency: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub total_get_trx_details: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    pub trx_detail_get_transaction_type: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub trx_detail_get_count: unsafe extern "C" fn(TaObject, *mut c_int) -> TaRc,
    pub trx_detail_get_amount_sum: unsafe extern "C" fn(TaObject, *mut TaObject) -> TaRc,
    // lane V: the terminal prints its own slips (print options)
    pub list_create: unsafe extern "C" fn(*mut TaObject) -> TaRc,
    pub list_add: unsafe extern "C" fn(TaObject, TaObject) -> TaRc,
    /// `(out, recipient, print_format, print_width, print_flags)`.
    pub print_option_create:
        unsafe extern "C" fn(*mut TaObject, c_int, c_int, c_int, c_int) -> TaRc,
    pub terminal_set_print_options: unsafe extern "C" fn(TaObject, TaObject) -> TaRc,
    // lane V: card.cancel (Worldline docx case I5)
    /// `ta_object_retain(object)` — the cancel handle's own reference.
    pub object_retain: unsafe extern "C" fn(TaObject) -> TaRc,
    /// `ta_terminal_cancel(terminal)`: best effort, never acknowledged by the
    /// terminal; the running request then returns with its cancel rc.
    pub terminal_cancel: unsafe extern "C" fn(TaObject) -> TaRc,
    /// Keeps the code above mapped. NEVER unloaded (`ManuallyDrop`): the
    /// library's own threads may still run and call our logger.
    _library: std::mem::ManuallyDrop<libloading::Library>,
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
            terminal_transaction: sym!("ta_terminal_transaction"),
            terminal_commit: sym!("ta_terminal_commit"),
            terminal_rollback: sym!("ta_terminal_rollback"),
            terminal_transaction_info_request: sym!("ta_terminal_transaction_info_request"),
            amount_create: sym!("ta_amount_create"),
            amount_get_value: sym!("ta_amount_get_value"),
            amount_get_currency: sym!("ta_amount_get_currency"),
            integer_get_value: sym!("ta_integer_get_value"),
            list_get_count: sym!("ta_list_get_count"),
            list_get_at: sym!("ta_list_get_at"),
            response_get_type: sym!("ta_transaction_response_get_type"),
            response_get_amount: sym!("ta_transaction_response_get_amount"),
            response_get_transaction_information: sym!(
                "ta_transaction_response_get_transaction_information"
            ),
            response_get_card_data: sym!("ta_transaction_response_get_card_data"),
            response_get_print_data: sym!("ta_transaction_response_get_print_data"),
            info_get_pos_entry_mode: sym!("ta_transaction_information_get_pos_entry_mode"),
            info_get_cvm: sym!("ta_transaction_information_get_cvm"),
            info_get_auth_code: sym!("ta_transaction_information_get_auth_code"),
            info_get_trans_ref: sym!("ta_transaction_information_get_trans_ref"),
            info_get_trans_seq: sym!("ta_transaction_information_get_trans_seq"),
            info_get_trm_trans_ref: sym!("ta_transaction_information_get_trm_trans_ref"),
            card_get_brand_name: sym!("ta_card_data_get_brand_name"),
            card_get_card_number_printable: sym!("ta_card_data_get_card_number_printable"),
            print_data_get_receipts: sym!("ta_print_data_get_receipts"),
            receipt_get_recipient: sym!("ta_receipt_get_recipient"),
            receipt_get_value: sym!("ta_receipt_get_value"),
            last_get_transaction_type: sym!(
                "ta_transaction_info_request_response_get_transaction_type"
            ),
            last_get_outcome: sym!("ta_transaction_info_request_response_get_outcome"),
            last_get_transaction_response: sym!(
                "ta_transaction_info_request_response_get_transaction_response"
            ),
            terminal_transaction2: sym!("ta_terminal_transaction2"),
            transaction_request_create: sym!("ta_transaction_request_create"),
            transaction_request_set_transaction_data: sym!(
                "ta_transaction_request_set_transaction_data"
            ),
            transaction_data_create: sym!("ta_transaction_data_create"),
            transaction_data_set_trans_seq: sym!("ta_transaction_data_set_trans_seq"),
            integer_create: sym!("ta_integer_create"),
            terminal_activate: sym!("ta_terminal_activate"),
            terminal_deactivate: sym!("ta_terminal_deactivate"),
            terminal_balance: sym!("ta_terminal_balance"),
            terminal_receipt_request: sym!("ta_terminal_receipt_request"),
            activate_get_print_data: sym!("ta_activate_response_get_print_data"),
            activate_get_act_seq_counter: sym!("ta_activate_response_get_act_seq_counter"),
            deactivate_get_print_data: sym!("ta_deactivate_response_get_print_data"),
            deactivate_get_counters: sym!("ta_deactivate_response_get_counters"),
            balance_get_print_data: sym!("ta_balance_response_get_print_data"),
            balance_get_counters: sym!("ta_balance_response_get_counters"),
            receipt_response_get_print_data: sym!("ta_receipt_request_response_get_print_data"),
            counters_get_seq_counter: sym!("ta_counters_get_seq_counter"),
            counters_get_counters: sym!("ta_counters_get_counters"),
            counter_get_brand_name: sym!("ta_counter_get_brand_name"),
            counter_get_acq_id: sym!("ta_counter_get_acq_id"),
            counter_get_count: sym!("ta_counter_get_count"),
            counter_get_totals: sym!("ta_counter_get_totals"),
            total_get_count: sym!("ta_total_get_count"),
            total_get_amount_sum: sym!("ta_total_get_amount_sum"),
            total_get_currency: sym!("ta_total_get_currency"),
            total_get_trx_details: sym!("ta_total_get_trx_details"),
            trx_detail_get_transaction_type: sym!("ta_trx_detail_get_transaction_type"),
            trx_detail_get_count: sym!("ta_trx_detail_get_count"),
            trx_detail_get_amount_sum: sym!("ta_trx_detail_get_amount_sum"),
            list_create: sym!("ta_list_create"),
            list_add: sym!("ta_list_add"),
            print_option_create: sym!("ta_print_option_create"),
            terminal_set_print_options: sym!("ta_terminal_set_print_options"),
            object_retain: sym!("ta_object_retain"),
            terminal_cancel: sym!("ta_terminal_cancel"),
            _library: std::mem::ManuallyDrop::new(library),
        })
    }
}
