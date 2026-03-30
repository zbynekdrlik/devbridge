use std::path::Path;

use anyhow::Result;
use devbridge_core::job_event::EventEmitter;

use crate::print_backend::{PrintBackend, PrintJobInfo};

pub struct DirectRaw {
    #[allow(dead_code)]
    address: String,
    #[allow(dead_code)]
    gs_device: String,
    #[allow(dead_code)]
    gs_resolution: u32,
}

impl DirectRaw {
    pub fn new(address: String, gs_device: String, gs_resolution: u32) -> Self {
        Self {
            address,
            gs_device,
            gs_resolution,
        }
    }
}

impl PrintBackend for DirectRaw {
    fn name(&self) -> &str {
        "direct_raw"
    }

    fn print(&self, _job: &PrintJobInfo, _pdf_path: &Path, _events: &EventEmitter) -> Result<()> {
        anyhow::bail!("DirectRaw backend not yet implemented")
    }
}
