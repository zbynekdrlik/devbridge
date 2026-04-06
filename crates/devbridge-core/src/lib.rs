pub mod client_registration;
pub mod config;
pub mod error;
pub mod ipc;
pub mod job;
pub mod job_event;
pub mod proto;
pub mod virtual_printer;

pub use client_registration::ClientRegistration;
pub use config::Config;
pub use error::Error;
pub use virtual_printer::VirtualPrinter;

pub type Result<T> = std::result::Result<T, Error>;

/// Format a byte count as a human-readable size string.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Printer metadata returned by the `/api/printers` endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrinterInfo {
    pub name: String,
    pub driver: String,
    pub status: String,
    pub jobs: u64,
    pub is_target: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_zero_bytes() {
        // Kills: line 19 `<` to `==` (0 == 1024 is false, would skip B branch)
        // Kills: line 19 `<` to `>` (0 > 1024 is false, would skip B branch)
        assert_eq!(format_size(0), "0B");
    }

    #[test]
    fn format_size_small_bytes() {
        // Kills: line 19 `<` to `==` (500 == 1024 is false, would skip B branch)
        assert_eq!(format_size(500), "500B");
    }

    #[test]
    fn format_size_boundary_1024_is_kb() {
        // Kills: line 19 `<` to `<=` (1024 <= 1024 is true, would stay in B branch)
        assert_eq!(format_size(1024), "1.0KB");
    }

    #[test]
    fn format_size_typical_kb() {
        // Kills: line 21 `<` to `==` (2048 == 1048576 is false, would fall to MB)
        // Kills: line 21 `<` to `>` (2048 > 1048576 is false, would fall to MB)
        assert_eq!(format_size(2048), "2.0KB");
    }

    #[test]
    fn format_size_boundary_1mb_is_mb() {
        // Kills: line 21 `<` to `<=` (1048576 <= 1048576 true, would stay KB)
        assert_eq!(format_size(1024 * 1024), "1.0MB");
    }

    #[test]
    fn format_size_3000_bytes_is_kb() {
        // Kills: line 21 `*` to `+` (1024+1024=2048, 3000 < 2048 is false, falls to MB)
        assert_eq!(format_size(3000), "2.9KB");
    }

    #[test]
    fn format_size_1025_is_kb_not_mb() {
        // Kills: line 21 `*` to `/` (1024/1024=1, 1025 < 1 is false, falls to MB)
        assert_eq!(format_size(1025), "1.0KB");
    }

    #[test]
    fn format_size_large_mb() {
        assert_eq!(format_size(10 * 1024 * 1024), "10.0MB");
    }
}
