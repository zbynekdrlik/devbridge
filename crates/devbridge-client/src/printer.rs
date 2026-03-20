use std::path::Path;

use anyhow::Result;

/// List available printers on this system.
#[cfg(target_os = "windows")]
pub fn list_printers() -> Result<Vec<String>> {
    use windows::Win32::Graphics::Printing::{
        EnumPrintersW, PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL, PRINTER_INFO_2W,
    };
    use windows::core::PCWSTR;

    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let mut needed: u32 = 0;
    let mut returned: u32 = 0;

    // First call to get required buffer size
    unsafe {
        let _ = EnumPrintersW(
            flags,
            PCWSTR::null(),
            2,
            None,
            0,
            &mut needed,
            &mut returned,
        );
    }

    if needed == 0 {
        return Ok(vec![]);
    }

    let mut buffer = vec![0u8; needed as usize];

    unsafe {
        EnumPrintersW(
            flags,
            PCWSTR::null(),
            2,
            Some(buffer.as_mut_ptr()),
            needed,
            &mut needed,
            &mut returned,
        )
        .map_err(|e| anyhow::anyhow!("EnumPrintersW failed: {}", e))?;
    }

    let printers = unsafe {
        std::slice::from_raw_parts(buffer.as_ptr() as *const PRINTER_INFO_2W, returned as usize)
    };

    let names: Vec<String> = printers
        .iter()
        .filter_map(|p| unsafe { p.pPrinterName.to_string().ok() })
        .collect();

    Ok(names)
}

/// List available printers on this system (non-Windows stub).
#[cfg(not(target_os = "windows"))]
pub fn list_printers() -> Result<Vec<String>> {
    Ok(vec![])
}

/// Send a PDF file to the specified printer using ShellExecuteW "printto" verb.
#[cfg(target_os = "windows")]
pub fn print_pdf(printer: &str, pdf_path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::PCWSTR;

    let verb: Vec<u16> = "printto\0".encode_utf16().collect();
    let file: Vec<u16> = pdf_path.as_os_str().encode_wide().chain(Some(0)).collect();
    let printer_w: Vec<u16> = printer.encode_utf16().chain(Some(0)).collect();

    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(printer_w.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        )
    };

    // ShellExecuteW returns HINSTANCE > 32 on success
    if result.0 as usize <= 32 {
        anyhow::bail!(
            "ShellExecuteW printto failed with code {}",
            result.0 as usize
        );
    }

    Ok(())
}

/// Send a PDF file to the specified printer (non-Windows stub).
#[cfg(not(target_os = "windows"))]
pub fn print_pdf(_printer: &str, _pdf_path: &Path) -> Result<()> {
    anyhow::bail!("printing is only supported on Windows")
}

/// Query the print queue for a printer (for E2E verification).
#[cfg(target_os = "windows")]
pub fn get_print_queue(printer_name: &str) -> Result<Vec<String>> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Printing::{ClosePrinter, EnumJobsW, JOB_INFO_1W, OpenPrinterW};
    use windows::core::PCWSTR;

    let printer_w: Vec<u16> = printer_name.encode_utf16().chain(Some(0)).collect();
    let mut handle = HANDLE::default();

    unsafe {
        OpenPrinterW(PCWSTR(printer_w.as_ptr()), &mut handle, None)
            .map_err(|e| anyhow::anyhow!("OpenPrinterW failed: {}", e))?;
    }

    let result = (|| -> Result<Vec<String>> {
        let mut needed: u32 = 0;
        let mut returned: u32 = 0;

        unsafe {
            let _ = EnumJobsW(handle, 0, 100, 1, None, 0, &mut needed, &mut returned);
        }

        if needed == 0 {
            return Ok(vec![]);
        }

        let mut buffer = vec![0u8; needed as usize];
        unsafe {
            EnumJobsW(
                handle,
                0,
                100,
                1,
                Some(buffer.as_mut_ptr()),
                needed,
                &mut needed,
                &mut returned,
            )
            .map_err(|e| anyhow::anyhow!("EnumJobsW failed: {}", e))?;
        }

        let jobs = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr() as *const JOB_INFO_1W, returned as usize)
        };

        let names: Vec<String> = jobs
            .iter()
            .filter_map(|j| unsafe { j.pDocument.to_string().ok() })
            .collect();

        Ok(names)
    })();

    unsafe {
        let _ = ClosePrinter(handle);
    }

    result
}

/// Query print queue (non-Windows stub).
#[cfg(not(target_os = "windows"))]
pub fn get_print_queue(_printer_name: &str) -> Result<Vec<String>> {
    Ok(vec![])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_printers_returns_ok() {
        // On non-Windows, returns empty vec. On Windows, returns actual printers.
        let result = list_printers();
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn test_print_pdf_errors_on_non_windows() {
        let result = print_pdf("test", Path::new("/tmp/test.pdf"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("only supported on Windows")
        );
    }

    #[test]
    fn test_get_print_queue_returns_ok() {
        let result = get_print_queue("nonexistent");
        // On non-Windows: returns empty vec
        // On Windows: may error for nonexistent printer, which is fine
        #[cfg(not(target_os = "windows"))]
        assert!(result.is_ok());
        #[cfg(target_os = "windows")]
        {
            let _ = result;
        } // Just verify it doesn't panic
    }
}
