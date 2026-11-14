//! The `file.write` payload (#130, contract odoo-erp `addons/
//! company_pokladna_api/docs/device_job_file_write.md`): a plain file NAME
//! and the exact bytes, never a folder.
//!
//! ```json
//! {"filename": "ORDER_SNV408_P00012_20261113141502.txt", "encoding": "cp1250",
//!  "content_b64": "<base64 of the exact bytes>", "size": 2321, "sha256": "<hex>"}
//! ```
//!
//! Everything here is checked BEFORE the job is journaled: a refused job never
//! touches the disk and is acked `failed bad_payload` ("nothing ran").

use base64::Engine as _;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The contract's `filename` pattern, verbatim. [`filename_problem`]
/// implements it (no regex dependency); the parity test compares this
/// string with the contract document.
pub const FILENAME_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9_.-]{0,99}$";
/// Longest file name the pattern allows.
pub const MAX_FILENAME_LEN: usize = 100;
/// Largest decoded file (an EDI ORDER is a few KiB): a bigger payload is
/// refused before anything is decoded into memory twice.
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
/// Payload keys the contract defines; `encoding` is optional.
pub const REQUIRED_KEYS: [&str; 4] = ["filename", "content_b64", "size", "sha256"];
pub const OPTIONAL_KEYS: [&str; 1] = ["encoding"];

/// Windows device names: `NUL.txt` opens the NUL device, not a file — the
/// contract's pattern alone lets them through.
const WINDOWS_DEVICE_NAMES: [&str; 24] = [
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A checked `file.write` payload.
#[derive(Clone, PartialEq, Eq)]
pub struct FilePayload {
    pub filename: String,
    pub bytes: Vec<u8>,
    /// Lower-case hex SHA-256 of `bytes`.
    pub sha256: String,
}

impl std::fmt::Debug for FilePayload {
    /// Never the content.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilePayload")
            .field("filename", &self.filename)
            .field("size", &self.bytes.len())
            .field("sha256", &self.sha256)
            .finish()
    }
}

/// Lower-case hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Why `name` is not a plain, safe file name (`None` = fine): the contract's
/// [`FILENAME_PATTERN`], plus what the pattern lets through but the folder
/// must never see — `..`, a trailing dot (Windows drops it, so `a.` would
/// write `a`) and a Windows device name (`NUL`, `COM1.txt`, …).
pub fn filename_problem(name: &str) -> Option<String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Some("filename is empty".into());
    };
    if name.chars().count() > MAX_FILENAME_LEN {
        return Some(format!(
            "filename is longer than {MAX_FILENAME_LEN} characters"
        ));
    }
    if !first.is_ascii_alphanumeric() {
        return Some(format!(
            "filename must start with a letter or digit ({FILENAME_PATTERN})"
        ));
    }
    if let Some(bad) = chars.find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
    {
        let what = if matches!(bad, '/' | '\\' | ':') {
            "a path separator - the payload names a FILE, the folder comes from the config only"
                .to_string()
        } else {
            format!("the character {bad:?}")
        };
        return Some(format!("filename contains {what} ({FILENAME_PATTERN})"));
    }
    if name.contains("..") {
        return Some("filename must not contain '..'".into());
    }
    if name.ends_with('.') {
        return Some("filename must not end with '.' (Windows drops it)".into());
    }
    let stem = name.split('.').next().unwrap_or_default();
    if WINDOWS_DEVICE_NAMES
        .iter()
        .any(|device| device.eq_ignore_ascii_case(stem))
    {
        return Some(format!(
            "filename '{name}' is a Windows device name, not a file"
        ));
    }
    None
}

/// Check a job's payload against the contract (keys, name, base64, size,
/// sha256). The error text names the problem, never the content.
pub fn parse_payload(payload: &Value) -> Result<FilePayload, String> {
    let obj = payload.as_object().ok_or("the payload is not an object")?;
    check_keys(obj)?;
    let filename = obj["filename"].as_str().ok_or("filename is not a string")?;
    if let Some(problem) = filename_problem(filename) {
        return Err(problem);
    }
    if let Some(encoding) = obj.get("encoding")
        && !encoding.is_string()
    {
        return Err("encoding is not a string".into());
    }
    let size = obj["size"]
        .as_u64()
        .ok_or("size is not a whole number >= 0")?;
    if size > MAX_FILE_BYTES as u64 {
        return Err(format!(
            "size {size} is over the {MAX_FILE_BYTES}-byte limit"
        ));
    }
    let want_sha = obj["sha256"]
        .as_str()
        .filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or("sha256 is not 64 hex characters")?
        .to_ascii_lowercase();
    let b64 = obj["content_b64"]
        .as_str()
        .ok_or("content_b64 is not a string")?;
    // base64 grows the data by 4/3: refuse an oversized text before decoding
    if b64.len() > MAX_FILE_BYTES / 3 * 4 + 4 {
        return Err(format!(
            "content_b64 is over the {MAX_FILE_BYTES}-byte limit"
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| "content_b64 is not valid base64".to_string())?;
    if bytes.len() as u64 != size {
        return Err(format!(
            "content_b64 decodes to {} bytes, size says {size}",
            bytes.len()
        ));
    }
    let sha256 = sha256_hex(&bytes);
    if sha256 != want_sha {
        return Err(format!(
            "the content's sha256 {sha256} does not match the payload's sha256 {want_sha}"
        ));
    }
    Ok(FilePayload {
        filename: filename.to_string(),
        bytes,
        sha256,
    })
}

/// Exactly the contract's keys: a missing one, or one the contract does not
/// define (a contract change devbridge has not followed), is refused.
fn check_keys(obj: &Map<String, Value>) -> Result<(), String> {
    if let Some(missing) = REQUIRED_KEYS.iter().find(|k| !obj.contains_key(**k)) {
        return Err(format!("the payload has no '{missing}'"));
    }
    let mut unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| !REQUIRED_KEYS.contains(k) && !OPTIONAL_KEYS.contains(k))
        .collect();
    unknown.sort_unstable();
    if !unknown.is_empty() {
        return Err(format!(
            "the payload has keys the file.write contract does not define: {}",
            unknown.join(", ")
        ));
    }
    Ok(())
}
