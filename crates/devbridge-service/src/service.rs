use anyhow::Result;
use tracing::warn;

/// Run DevBridge as a Windows service.
///
/// This is a placeholder that will be implemented with the `windows-service` crate
/// when building for the Windows target.
pub fn run_as_service() -> Result<()> {
    warn!("Windows service mode is not yet implemented on this platform");
    Ok(())
}
