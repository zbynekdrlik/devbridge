//! Windows half of [`super::restrict`] (#101): SYSTEM (the service) and
//! Administrators only, inheritance removed — well-known SIDs, so it works
//! on a Slovak Windows (localized group names). In `.cargo/mutants.toml`
//! `exclude_re`: the Linux mutation job never compiles it.

use std::path::Path;
use std::process::Command;

pub fn restrict(path: &Path) -> std::io::Result<()> {
    let output = Command::new("icacls")
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:F",
            "*S-1-5-32-544:F",
        ])
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "icacls could not restrict {} (exit {:?}): {} {}",
        path.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}
