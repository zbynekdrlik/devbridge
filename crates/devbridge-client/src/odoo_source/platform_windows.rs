//! Windows half of the heartbeat printer-status read (#90, #97): the queue
//! state straight from winspool — `OpenPrinterW` → `GetPrinterW` level 2 →
//! `PRINTER_INFO_2W.Status` / `.Attributes` → `ClosePrinter` (on drop).
//!
//! No process is spawned: the `Get-Printer` PowerShell this replaces
//! cold-started for 2.6 s on an idle store PC and ran past its 10 s timeout
//! on a busy one (#97). Compiled only on Windows, so `.cargo/mutants.toml`
//! excludes it; the bit mapping, the guard against a hung spooler and the
//! log rate limit are tested helpers in `printer_status.rs`.

use windows_sys::Win32::Graphics::Printing::{GetPrinterW, PRINTER_INFO_2W};

use super::{QueueState, StatusError};
use crate::winspool::PrinterHandle;

/// The answer to a too-small buffer (incl. the sizing call).
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

/// The printer's level-2 data (comment, port, …) can grow between the
/// sizing call and the read; a too-small buffer is retried this often.
const READ_ATTEMPTS: usize = 3;

pub(super) fn query(printer: &str) -> Result<QueueState, StatusError> {
    let handle = PrinterHandle::open(printer).map_err(|e| StatusError::Open(code(&e)))?;

    // Sizing call: fails with ERROR_INSUFFICIENT_BUFFER and reports the size.
    let mut needed: u32 = 0;
    // SAFETY: the handle is open; a null buffer of size 0 is the documented
    // sizing call; `needed` is a valid out pointer.
    unsafe {
        GetPrinterW(handle.raw(), 2, std::ptr::null_mut(), 0, &mut needed);
    }
    if (needed as usize) < size_of::<PRINTER_INFO_2W>() {
        return Err(StatusError::Get(last_error()));
    }

    for _ in 0..READ_ATTEMPTS {
        // u64 words: PRINTER_INFO_2W holds pointers, so the buffer must be
        // 8-byte aligned. Its strings follow the struct inside the buffer.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let size = needed;
        // SAFETY: `buf` holds at least `size` bytes and outlives the call;
        // `needed` is a valid out pointer.
        let ok =
            unsafe { GetPrinterW(handle.raw(), 2, buf.as_mut_ptr().cast(), size, &mut needed) };
        if ok != 0 {
            // SAFETY: GetPrinterW level 2 succeeded, so `buf` starts with a
            // PRINTER_INFO_2W; the buffer is 8-byte aligned and at least
            // size_of::<PRINTER_INFO_2W>() bytes (checked above). It is Copy
            // and read by value; its string pointers point into `buf` and
            // are not used.
            let info: PRINTER_INFO_2W = unsafe { buf.as_ptr().cast::<PRINTER_INFO_2W>().read() };
            return Ok(QueueState {
                status: info.Status,
                attributes: info.Attributes,
            });
        }
        let error = last_error();
        // Only "the data grew meanwhile" is worth another try.
        if error != ERROR_INSUFFICIENT_BUFFER || needed <= size {
            return Err(StatusError::Get(error));
        }
    }
    Err(StatusError::Get(ERROR_INSUFFICIENT_BUFFER))
}

/// The Win32 error of the winspool call that just failed (0 if unknown).
fn last_error() -> i32 {
    code(&std::io::Error::last_os_error())
}

/// The Win32 error code carried by `e` (0 if none).
fn code(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(0)
}
