//! Windows half of the LAN API key restriction (#101): the private key file
//! is readable by SYSTEM (the service) and Administrators only — the data
//! dir under ProgramData is readable by every local user by default.
//! Well-known SIDs, so it works on a Slovak Windows (localized group names).
//! In `.cargo/mutants.toml` `exclude_re`: the Linux mutation job never
//! compiles it.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};

pub fn restrict(path: &Path) -> Result<()> {
    let output = Command::new("icacls")
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:F",
            "*S-1-5-32-544:F",
        ])
        .output()
        .context("run icacls")?;
    ensure!(
        output.status.success(),
        "icacls could not restrict {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stdout).trim()
    );
    Ok(())
}
