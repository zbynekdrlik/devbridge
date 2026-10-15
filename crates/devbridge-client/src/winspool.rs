//! Shared winspool plumbing (Windows only): a NUL-terminated UTF-16 name and
//! a printer handle that is closed on every exit path. Used by the RAW
//! spooler backend (#88) and the Odoo heartbeat's queue-state read (#97).
//! Compiled only on Windows, so `.cargo/mutants.toml` excludes it.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Graphics::Printing::{ClosePrinter, OpenPrinterW, PRINTER_HANDLE};

/// `s` as a NUL-terminated UTF-16 string for a `PCWSTR` argument.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// An open printer (`OpenPrinterW` with default access), closed on drop.
pub(crate) struct PrinterHandle(PRINTER_HANDLE);

impl PrinterHandle {
    /// Open `printer`. The error is the Win32 error (`raw_os_error`; 1801 =
    /// ERROR_INVALID_PRINTER_NAME).
    pub(crate) fn open(printer: &str) -> io::Result<Self> {
        let name = wide(printer);
        let mut raw = PRINTER_HANDLE::default();
        // SAFETY: `name` is NUL-terminated and outlives the call; `raw` is a
        // valid out pointer; null defaults are allowed (default access).
        let opened = unsafe { OpenPrinterW(name.as_ptr(), &mut raw, std::ptr::null()) };
        if opened == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(raw))
    }

    pub(crate) fn raw(&self) -> PRINTER_HANDLE {
        self.0
    }
}

impl Drop for PrinterHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful OpenPrinterW and is
        // closed exactly once, here.
        unsafe {
            ClosePrinter(self.0);
        }
    }
}
