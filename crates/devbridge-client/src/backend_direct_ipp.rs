use std::path::Path;

use anyhow::Result;
use devbridge_core::job_event::EventEmitter;

use crate::print_backend::{PrintBackend, PrintJobInfo};

pub struct DirectIpp {
    #[allow(dead_code)]
    address: String,
    #[allow(dead_code)]
    gs_device: String,
    #[allow(dead_code)]
    gs_resolution: u32,
}

impl DirectIpp {
    pub fn new(address: String, gs_device: String, gs_resolution: u32) -> Self {
        Self {
            address,
            gs_device,
            gs_resolution,
        }
    }
}

impl PrintBackend for DirectIpp {
    fn name(&self) -> &str {
        "direct_ipp"
    }

    fn print(&self, _job: &PrintJobInfo, _pdf_path: &Path, _events: &EventEmitter) -> Result<()> {
        anyhow::bail!("DirectIpp backend not yet implemented")
    }
}
