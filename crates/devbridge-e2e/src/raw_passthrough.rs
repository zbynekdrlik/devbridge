//! E2E coverage for RAW passthrough (issue #88 — label printers).
//!
//! `deploy/e2e-setup-client-local.ps1` starts a SECOND isolated client on the
//! client runner (`e2e-raw-client`, data dir `C:\ProgramData\DevBridge-E2E-Raw`,
//! dashboard 9222) whose config is written by the REAL installer function
//! `Get-DevBridgeClientConfigExtras` with `print_backend = "windows_spooler_raw"`
//! and `virtual_printer_driver = "Generic / Text Only"`, targeting the local
//! `DevBridge-E2E-Raw` printer (Generic / Text Only on `NUL:`).
//! `deploy/e2e-wait-ready.ps1` deliberately leaves it PENDING.
//!
//! This step, running on the server runner, proves the whole lane:
//! 1. the client's driver override reaches the server (`/api/clients`);
//! 2. approval auto-creates the virtual printer WITH the override, and the
//!    reconciler registers the server-side Windows printer with THAT driver
//!    (not the IPP Class Driver) on the E2E IPP port;
//! 3. bytes written RAW into that Windows printer (what a vendor driver like
//!    TSC ML241P spools) travel inetpp → IPP → server byte-identical (SHA256);
//! 4. the RAW client spools them unchanged: EventID 307 on the client with a
//!    byte count equal to the input size.
//!
//! It finally REJECTS the RAW client so it can never pick up a default-queue
//! job of the remaining (retry) test.

use anyhow::{Context, Result, bail, ensure};
use std::time::{Duration, Instant};

/// machine_id (`client_id`) of the RAW E2E client. Must match
/// `deploy/e2e-setup-client-local.ps1` and `deploy/e2e-wait-ready.ps1`.
pub const E2E_RAW_CLIENT_ID: &str = "e2e-raw-client";

/// Virtual printer name the RAW client asks for (→ Windows printer name on
/// the server). Must match the E2E client setup.
pub const E2E_RAW_VP_NAME: &str = "E2E Raw";

/// Driver override the RAW client asks for. Inbox v3 driver present on
/// pz-server; stands in for the vendor driver (TSC ML241P) in production.
pub const E2E_RAW_DRIVER: &str = "Generic / Text Only";

const CLIENT_WAIT: Duration = Duration::from_secs(90);
/// Reconciler: 500 ms debounce + IPP probe + printui + up to 15 s verify.
const WINDOWS_PRINTER_WAIT: Duration = Duration::from_secs(90);
const JOB_ARRIVE_WAIT: Duration = Duration::from_secs(60);
/// Client RAW verify window is 60 s; leave room for dispatch + download.
const JOB_COMPLETE_WAIT: Duration = Duration::from_secs(150);

/// Deterministic label-like payload: a TSPL job plus every byte value
/// 0x00..=0xFF and a unique marker so this run's job is unambiguous.
pub fn raw_payload(marker: &str) -> Vec<u8> {
    let mut p = format!(
        "SIZE 50 mm,30 mm\r\nGAP 2 mm,0 mm\r\nCLS\r\nTEXT 10,10,\"3\",0,1,1,\"{marker}\"\r\n"
    )
    .into_bytes();
    p.extend(0u8..=255);
    p.extend_from_slice(b"\r\nPRINT 1,1\r\n");
    p
}

/// The ipp_name the server derives from a display name (mirror of
/// `devbridge_core::virtual_printer::slugify`).
pub fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Windows printer port URL the reconciler registers for `ipp_name`.
pub fn expected_port_url(ipp_port: &str, ipp_name: &str) -> String {
    format!("http://127.0.0.1:{ipp_port}/printers/{ipp_name}")
}

/// Parse `Get-Printer` output `<DriverName>|<PortName>`.
pub fn parse_driver_port(stdout: &str) -> Option<(String, String)> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    let (d, p) = line.split_once('|')?;
    Some((d.trim().to_string(), p.trim().to_string()))
}

/// Parse the submit script's `JOB=<id> LEN=<n> SHA=<hex>` line.
pub fn parse_submit_line(stdout: &str) -> Option<(u32, u64, String)> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("JOB="))?;
    let mut job = None;
    let mut len = None;
    let mut sha = None;
    for part in line.split_whitespace() {
        if let Some(v) = part.strip_prefix("JOB=") {
            job = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("LEN=") {
            len = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("SHA=") {
            sha = Some(v.to_ascii_lowercase());
        }
    }
    Some((job?, len?, sha?))
}

/// The client's EventID 307 evidence must report exactly `len` bytes
/// (`… , <n> bytes (expected <n>)` — backend_windows_spooler_raw format).
pub fn evidence_has_byte_count(evidence: &str, len: u64) -> bool {
    evidence.starts_with("EventID 307:")
        && evidence.contains(&format!(", {len} bytes (expected {len})"))
}

/// PowerShell that writes the file at `path` RAW into Windows printer
/// `printer` via winspool (the same call path a vendor driver's spooled
/// output takes) and prints `JOB=<id> LEN=<n> SHA=<sha256>`.
pub fn submit_script(printer: &str, path: &str) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class DbE2eRaw {{
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
  public class DOCINFOW {{ [MarshalAs(UnmanagedType.LPWStr)] public string pDocName; [MarshalAs(UnmanagedType.LPWStr)] public string pOutputFile; [MarshalAs(UnmanagedType.LPWStr)] public string pDatatype; }}
  [DllImport("winspool.drv", CharSet=CharSet.Unicode, SetLastError=true)] public static extern bool OpenPrinterW(string n, out IntPtr h, IntPtr d);
  [DllImport("winspool.drv", SetLastError=true)] public static extern bool ClosePrinter(IntPtr h);
  [DllImport("winspool.drv", CharSet=CharSet.Unicode, SetLastError=true)] public static extern int StartDocPrinterW(IntPtr h, int level, [In] DOCINFOW di);
  [DllImport("winspool.drv", SetLastError=true)] public static extern bool EndDocPrinter(IntPtr h);
  [DllImport("winspool.drv", SetLastError=true)] public static extern bool WritePrinter(IntPtr h, byte[] b, int n, out int w);
}}
'@
$bytes = [System.IO.File]::ReadAllBytes('{path}')
$sha = ([BitConverter]::ToString([System.Security.Cryptography.SHA256]::Create().ComputeHash($bytes))).Replace('-', '').ToLower()
$h = [IntPtr]::Zero
if (-not [DbE2eRaw]::OpenPrinterW('{printer}', [ref]$h, [IntPtr]::Zero)) {{ throw "OpenPrinterW failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }}
try {{
  $di = New-Object DbE2eRaw+DOCINFOW
  $di.pDocName = 'devbridge-e2e-raw'
  $di.pDatatype = 'RAW'
  $job = [DbE2eRaw]::StartDocPrinterW($h, 1, $di)
  if ($job -eq 0) {{ throw "StartDocPrinterW failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }}
  $w = 0
  if (-not [DbE2eRaw]::WritePrinter($h, $bytes, $bytes.Length, [ref]$w)) {{ throw "WritePrinter failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }}
  if ($w -ne $bytes.Length) {{ throw "WritePrinter wrote $w of $($bytes.Length) bytes" }}
  if (-not [DbE2eRaw]::EndDocPrinter($h)) {{ throw "EndDocPrinter failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }}
}} finally {{
  [DbE2eRaw]::ClosePrinter($h) | Out-Null
}}
"JOB=$job LEN=$($bytes.Length) SHA=$sha"
"#,
        path = path.replace('\'', "''"),
        printer = printer.replace('\'', "''"),
    )
}

fn powershell(script: &str) -> Result<std::process::Output> {
    std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output()
        .context("failed to run powershell")
}

/// Run a multi-line script (here-strings, double quotes) from a .ps1 file —
/// `-Command` on the command line mangles both.
fn powershell_file(tag: &str, script: &str) -> Result<std::process::Output> {
    let path = std::env::temp_dir().join(format!("{tag}.ps1"));
    std::fs::write(&path, script).with_context(|| format!("write {}", path.display()))?;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&path)
        .output()
        .context("failed to run powershell -File");
    let _ = std::fs::remove_file(&path);
    out
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    ensure!(resp.status().is_success(), "GET {url} -> {}", resp.status());
    Ok(resp.json().await?)
}

async fn post_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    let resp = client
        .post(url)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    ensure!(
        resp.status().is_success(),
        "POST {url} -> {}",
        resp.status()
    );
    Ok(resp.json().await?)
}

/// Step body — see the module docs.
pub async fn test_raw_passthrough(
    client: &reqwest::Client,
    server_base: &str,
    ipp_port: &str,
) -> Result<()> {
    // 1. RAW client registered, still pending, carrying its driver override.
    let start = Instant::now();
    let raw_client = loop {
        let clients = get_json(client, &format!("{server_base}/api/clients")).await?;
        if let Some(c) = clients
            .as_array()
            .and_then(|a| a.iter().find(|c| c["machine_id"] == E2E_RAW_CLIENT_ID))
        {
            break c.clone();
        }
        if start.elapsed() > CLIENT_WAIT {
            bail!(
                "{E2E_RAW_CLIENT_ID} never registered with the server within {}s (clients: {clients})",
                CLIENT_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    ensure!(
        raw_client["virtual_printer_driver"] == E2E_RAW_DRIVER,
        "client did not send its virtual_printer_driver: {raw_client}"
    );
    ensure!(
        raw_client["virtual_printer_name"] == E2E_RAW_VP_NAME,
        "client did not send its virtual_printer_name: {raw_client}"
    );
    ensure!(
        raw_client["pairing_state"] == "pending",
        "{E2E_RAW_CLIENT_ID} must still be pending (e2e-wait-ready must not auto-approve it): {raw_client}"
    );

    // 2. Approve → VP with the driver override. From here on the RAW client
    // is approved; it is rejected again on EVERY path (success or failure),
    // so it can never be left approved and connected for test 35.
    let approved = post_json(
        client,
        &format!("{server_base}/api/clients/{E2E_RAW_CLIENT_ID}/approve"),
    )
    .await?;
    let result = verify_approved_raw_lane(client, server_base, ipp_port, &approved).await;
    let rejected = post_json(
        client,
        &format!("{server_base}/api/clients/{E2E_RAW_CLIENT_ID}/reject"),
    )
    .await;
    result?;
    rejected.context("could not reject the RAW E2E client after the RAW step")?;
    println!("PASS");
    Ok(())
}

/// Steps 2-6 of [`test_raw_passthrough`], run while the RAW client is
/// approved (`approved` = the approval response).
async fn verify_approved_raw_lane(
    client: &reqwest::Client,
    server_base: &str,
    ipp_port: &str,
    approved: &serde_json::Value,
) -> Result<()> {
    let vp = &approved["virtual_printer"];
    ensure!(
        vp["driver"] == E2E_RAW_DRIVER,
        "approval did not create the VP with the driver override: {approved}"
    );
    let ipp_name = vp["ipp_name"]
        .as_str()
        .context("approved VP has no ipp_name")?;
    ensure!(
        ipp_name == slugify(E2E_RAW_VP_NAME),
        "unexpected ipp_name {ipp_name}"
    );
    let vps = get_json(client, &format!("{server_base}/api/virtual-printers")).await?;
    let listed = vps
        .as_array()
        .and_then(|a| a.iter().find(|v| v["ipp_name"] == ipp_name))
        .context("RAW VP missing from /api/virtual-printers")?;
    ensure!(
        listed["effective_driver"] == E2E_RAW_DRIVER
            && listed["paired_client_id"] == E2E_RAW_CLIENT_ID,
        "RAW VP listed wrong: {listed}"
    );

    // 3. Reconciler registered the server Windows printer with THAT driver.
    let want_port = expected_port_url(ipp_port, ipp_name);
    let get_printer = format!(
        "Get-Printer -Name '{}' -ErrorAction SilentlyContinue | ForEach-Object {{ '{{0}}|{{1}}' -f $_.DriverName, $_.PortName }}",
        E2E_RAW_VP_NAME.replace('\'', "''")
    );
    let start = Instant::now();
    loop {
        let out = powershell(&get_printer)?;
        let seen = parse_driver_port(&String::from_utf8_lossy(&out.stdout));
        if let Some((driver, port)) = &seen
            && driver == E2E_RAW_DRIVER
            && *port == want_port
        {
            println!("  Windows printer '{E2E_RAW_VP_NAME}': driver={driver} port={port}");
            break;
        }
        if start.elapsed() > WINDOWS_PRINTER_WAIT {
            bail!(
                "Windows printer '{E2E_RAW_VP_NAME}' not registered with driver '{E2E_RAW_DRIVER}' on {want_port} within {}s (seen: {seen:?})",
                WINDOWS_PRINTER_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // 4. Spool RAW bytes into that Windows printer (driver output path).
    let marker = format!(
        "devbridge-e2e-raw-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let payload = raw_payload(&marker);
    let file = std::env::temp_dir().join(format!("{marker}.bin"));
    std::fs::write(&file, &payload).context("write RAW payload file")?;
    let out = powershell_file(
        &marker,
        &submit_script(E2E_RAW_VP_NAME, &file.to_string_lossy()),
    );
    let _ = std::fs::remove_file(&file);
    let out = out?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (spool_job, len, sha) = parse_submit_line(&stdout).with_context(|| {
        format!(
            "RAW submit to '{E2E_RAW_VP_NAME}' failed: stdout={stdout} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        )
    })?;
    ensure!(
        len == payload.len() as u64,
        "submit wrote {len} of {} bytes",
        payload.len()
    );
    println!(
        "  RAW submitted: server spooler job {spool_job}, {len} B, sha256 {}",
        sha.get(..16).unwrap_or(&sha)
    );

    // 5. Server job byte-identical (inetpp → IPP → spool).
    let start = Instant::now();
    let job = loop {
        let jobs = get_json(client, &format!("{server_base}/api/jobs")).await?;
        if let Some(j) = jobs.as_array().and_then(|a| {
            a.iter()
                .find(|j| j["printer"] == ipp_name && j["payload_size"].as_u64() == Some(len))
        }) {
            break j.clone();
        }
        if start.elapsed() > JOB_ARRIVE_WAIT {
            bail!(
                "no {len}-byte job on '{ipp_name}' reached the server within {}s",
                JOB_ARRIVE_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    ensure!(
        job["payload_sha256"].as_str() == Some(sha.as_str()),
        "server payload is NOT byte-identical: sha256 {} != submitted {sha}",
        job["payload_sha256"]
    );
    let job_id = job["id"].as_str().context("job has no id")?.to_string();
    println!(
        "  server job {} byte-identical (sha256 match)",
        &job_id[..8]
    );

    // 6. RAW client printed it: completed + EventID 307 with the byte count.
    let start = Instant::now();
    loop {
        let jobs = get_json(client, &format!("{server_base}/api/jobs")).await?;
        let status = jobs
            .as_array()
            .and_then(|a| a.iter().find(|j| j["id"] == job_id.as_str()))
            .and_then(|j| j["status"].as_str().map(String::from))
            .unwrap_or_default();
        if status == "completed" {
            break;
        }
        if status == "failed" || start.elapsed() > JOB_COMPLETE_WAIT {
            let events =
                get_json(client, &format!("{server_base}/api/jobs/{job_id}/events")).await?;
            bail!("RAW job {job_id} status '{status}' (events: {events})");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let events = get_json(client, &format!("{server_base}/api/jobs/{job_id}/events")).await?;
    let evidence = events
        .as_array()
        .and_then(|a| a.iter().find(|e| e["verification_method"] == "eventid_307"))
        .and_then(|e| e["verification_evidence"].as_str())
        .with_context(|| format!("no eventid_307 verification on job {job_id}: {events}"))?
        .to_string();
    ensure!(
        evidence_has_byte_count(&evidence, len),
        "EventID 307 byte count does not match the {len}-byte input: {evidence}"
    );
    println!("  client: {evidence}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_payload_contains_marker_every_byte_and_print() {
        let p = raw_payload("m-1");
        let text = String::from_utf8_lossy(&p);
        assert!(text.contains("\"m-1\""));
        assert!(text.ends_with("PRINT 1,1\r\n"));
        for b in 0u8..=255 {
            assert!(p.contains(&b), "byte {b:#04x} missing");
        }
    }

    #[test]
    fn test_slugify_matches_server() {
        assert_eq!(slugify("E2E Raw"), "e2e-raw");
        assert_eq!(slugify("spisska stitky"), "spisska-stitky");
    }

    #[test]
    fn test_expected_port_url() {
        assert_eq!(
            expected_port_url("1631", "e2e-raw"),
            "http://127.0.0.1:1631/printers/e2e-raw"
        );
    }

    #[test]
    fn test_parse_driver_port() {
        assert_eq!(
            parse_driver_port("Generic / Text Only|http://127.0.0.1:1631/printers/e2e-raw\r\n"),
            Some((
                "Generic / Text Only".to_string(),
                "http://127.0.0.1:1631/printers/e2e-raw".to_string()
            ))
        );
        assert_eq!(parse_driver_port(""), None);
        assert_eq!(parse_driver_port("no separator"), None);
    }

    #[test]
    fn test_parse_submit_line() {
        assert_eq!(
            parse_submit_line("noise\r\nJOB=41 LEN=332 SHA=E94AE3\r\n"),
            Some((41, 332, "e94ae3".to_string()))
        );
        assert_eq!(parse_submit_line("JOB=x LEN=1 SHA=a"), None);
        assert_eq!(parse_submit_line("LEN=1"), None);
        assert_eq!(parse_submit_line(""), None);
    }

    #[test]
    fn test_evidence_has_byte_count() {
        let ok = "EventID 307: spooler job 7 on DevBridge-E2E-Raw via port NUL:, 332 bytes (expected 332)";
        assert!(evidence_has_byte_count(ok, 332));
        assert!(!evidence_has_byte_count(ok, 331));
        assert!(!evidence_has_byte_count(
            "EventID 307: spooler job 7 on X via port NUL:, 331 bytes (expected 332)",
            332
        ));
        assert!(!evidence_has_byte_count("Virtual printer X", 332));
    }

    #[test]
    fn test_submit_script_escapes_and_uses_raw() {
        let s = submit_script("O'Brien", r"C:\t\x'y.bin");
        assert!(s.contains("OpenPrinterW('O''Brien'"), "{s}");
        assert!(s.contains(r"ReadAllBytes('C:\t\x''y.bin')"), "{s}");
        assert!(s.contains("$di.pDatatype = 'RAW'"), "{s}");
        assert!(
            s.contains("\"JOB=$job LEN=$($bytes.Length) SHA=$sha\""),
            "{s}"
        );
    }
}
