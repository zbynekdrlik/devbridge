use anyhow::Result;

/// List available printers on this system.
pub fn list_printers() -> Result<Vec<String>> {
    // Placeholder
    Ok(vec![])
}

/// Send a PDF file to the specified printer.
pub fn print_pdf(_printer: &str, _data: &[u8]) -> Result<()> {
    // Placeholder
    Ok(())
}
