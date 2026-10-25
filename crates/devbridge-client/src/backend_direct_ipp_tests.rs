//! Unit tests of the direct_ipp backend against mock printers: Title-Case
//! headers (#71) and the send / poll failure events (#112).

use super::*;

/// Accept ONE connection on `listener` within 10 s (then panic) and give
/// the stream 10 s read / write timeouts: a mock printer must never hang
/// the test binary when the client under test never connects or stalls
/// (every wait in a test is bounded, `.claude/rules/ci.md`).
fn accept_bounded(listener: &std::net::TcpListener) -> std::net::TcpStream {
    let limit = std::time::Duration::from_secs(10);
    listener
        .set_nonblocking(true)
        .expect("non-blocking mock listener");
    let deadline = std::time::Instant::now() + limit;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the client under test never connected to the mock printer within {limit:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("accept on the mock printer: {e}"),
        }
    };
    stream.set_nonblocking(false).expect("blocking mock stream");
    stream
        .set_read_timeout(Some(limit))
        .expect("mock stream read timeout");
    stream
        .set_write_timeout(Some(limit))
        .expect("mock stream write timeout");
    stream
}

/// Regression test for #71 (HP LaserJet M110w, store pjzav): the
/// printer's embedded HTTP parser treats header *names*
/// case-sensitively. hyper (reqwest's HTTP/1.1 engine) lowercases
/// header names by default, so `content-length:` reaches the printer
/// instead of `Content-Length:` — the HP accepts the Print-Job
/// (job-id assigned, job-state 3 pending) but never sees the body
/// length and aborts the job (job-state 8, aborted-by-system).
///
/// This spins up a raw `TcpListener` mock IPP server, sends one
/// Print-Job request through [`http_client`] + [`post_ipp`] — the
/// exact helpers [`DirectIpp::send_ipp_job`] and
/// [`DirectIpp::poll_job_completion`] both use — and asserts the raw
/// request head carries Title-Case header names.
///
/// One test covers both call sites: they share `http_client` and
/// `post_ipp`, so there is nothing left that could differ between
/// the Print-Job send and the Get-Job-Attributes poll (that is the
/// whole point of extracting the shared helpers).
#[test]
fn test_http_client_sends_title_case_headers() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock IPP server");
    let addr = listener.local_addr().expect("mock server local_addr");
    let (tx, rx) = mpsc::channel();

    let server = std::thread::spawn(move || {
        let mut stream = accept_bounded(&listener);

        // Read the raw request head (method/headers) up to the blank line.
        let mut head_buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read request byte");
            head_buf.push(byte[0]);
            if head_buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&head_buf).to_string();

        // Case-insensitive Content-Length lookup so this drains the
        // body correctly whether the header name is lower- or
        // Title-Case — that casing is exactly what this test checks.
        let content_length = head
            .lines()
            .find_map(|l| {
                let lower = l.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        if content_length > 0 {
            let mut body_buf = vec![0u8; content_length];
            stream
                .read_exact(&mut body_buf)
                .expect("drain mock request body");
        }

        // Minimal valid IPP 1.1 successful-ok response: job-id=1, job-state=9 (completed).
        let mut ipp_body = Vec::new();
        ipp_body.push(1u8); // version-major
        ipp_body.push(1u8); // version-minor
        ipp_body.extend_from_slice(&0x0000u16.to_be_bytes()); // status-code: successful-ok
        ipp_body.extend_from_slice(&1u32.to_be_bytes()); // request-id
        ipp_body.push(0x01); // operation-attributes-tag
        ipp_codec::write_attribute(&mut ipp_body, 0x47, "attributes-charset", b"utf-8");
        ipp_codec::write_attribute(&mut ipp_body, 0x48, "attributes-natural-language", b"en-us");
        ipp_body.push(0x02); // job-attributes-tag
        ipp_codec::write_integer_attribute(&mut ipp_body, 0x21, "job-id", 1); // integer
        ipp_codec::write_integer_attribute(&mut ipp_body, 0x23, "job-state", 9); // enum
        ipp_body.push(0x03); // end-of-attributes-tag

        let response_head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            ipp_body.len()
        );
        stream
            .write_all(response_head.as_bytes())
            .expect("write mock response head");
        stream
            .write_all(&ipp_body)
            .expect("write mock response body");
        stream.flush().ok();

        tx.send(head)
            .expect("send recorded request head to test thread");
    });

    let client = http_client(false, std::time::Duration::from_secs(5)).expect("build http_client");
    let url = format!("http://{}/ipp/print", addr);
    let body = ipp_codec::build_print_job_request(
        "ipp://127.0.0.1/ipp/print",
        "application/PCLm",
        "regression-test-71",
        1,
        1,
    );

    let resp_bytes = post_ipp(&client, &url, body).expect("post_ipp to mock server");
    let ipp_resp = ipp_codec::parse_response(&resp_bytes).expect("parse mock IPP response");
    assert!(
        ipp_resp.is_success(),
        "mock server response must parse as success"
    );

    server.join().expect("join mock server thread");
    let head = rx.recv().expect("recv recorded request head");

    assert!(
        head.contains("Content-Length:"),
        "expected Title-Case 'Content-Length:' header, got request head:\n{head}"
    );
    assert!(
        head.contains("Content-Type: application/ipp"),
        "expected Title-Case 'Content-Type:' header, got request head:\n{head}"
    );
    assert!(
        !head.contains("content-length:"),
        "HP LaserJet M110w (#71) treats header names case-sensitively and \
             silently aborts the job on a lowercase content-length: header \
             — got request head:\n{head}"
    );
}

/// A local address nothing listens on (bound, then released).
fn closed_local_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port");
    listener.local_addr().expect("probe local_addr").port()
}

fn test_job() -> PrintJobInfo {
    PrintJobInfo {
        job_id: "job-112".into(),
        document_name: "Prevodka".into(),
        copies: 1,
        duplex: false,
        color: false,
        printer_name: "pjkes-printer".into(),
        printer_display_name: None,
    }
}

/// Run `send_ipp_job` against `address` with a tiny raster file and
/// return its result plus every event it emitted.
fn send_and_collect(address: &str) -> (Result<()>, Vec<PrintJobEvent>) {
    let backend = DirectIpp::new(address.into(), "urfgray".into(), 600, false);
    let dir = tempfile::tempdir().expect("tempdir");
    let raster = dir.path().join("job-112.urf");
    std::fs::write(&raster, b"UNIRAST\0\0\0\0\0").expect("write raster");
    let (tx, _) = tokio::sync::broadcast::channel(16);
    let events = EventEmitter::new(tx);
    let mut rx = events.subscribe();
    let result = backend.send_ipp_job(
        &test_job(),
        &raster,
        "pjkes-printer",
        &events,
        &CancellationToken::new(),
    );
    let mut seen = Vec::new();
    while let Ok(e) = rx.try_recv() {
        seen.push(e);
    }
    (result, seen)
}

fn failed_events(events: &[PrintJobEvent]) -> Vec<&PrintJobEvent> {
    events
        .iter()
        .filter(|e| e.stage == PrintStage::Failed)
        .collect()
}

/// Regression test for #112 (pjkes job 2ca39272, printer asleep): a
/// Print-Job that cannot reach the printer must end in ONE `Failed`
/// event that names the cause from the error's `source()` chain — not a
/// job stuck at `sending` with the reason only on pz-server.
#[test]
fn test_print_job_send_failure_emits_failed_with_the_cause() {
    let port = closed_local_port();

    let (result, events) = send_and_collect(&format!("127.0.0.1:{port}"));

    let err = result.expect_err("nothing listens on the port");
    assert!(
        format!("{err:#}")
            .to_lowercase()
            .contains("connection refused"),
        "returned error must carry the cause chain: {err:#}"
    );
    let failed = failed_events(&events);
    assert_eq!(failed.len(), 1, "exactly one Failed event: {events:?}");
    let detail = &failed[0].detail;
    assert!(!failed[0].success);
    assert!(
        detail.starts_with("IPP Print-Job send failed"),
        "Failed event detail: {detail}"
    );
    assert!(
        detail.contains(&format!("127.0.0.1:{port}")),
        "Failed event names the printer address: {detail}"
    );
    assert!(
        detail.to_lowercase().contains("connection refused"),
        "Failed event carries the cause (source chain), not only \
             'error sending request': {detail}"
    );
}

/// The Get-Job-Attributes poll (#112): the printer accepts the Print-Job,
/// then becomes unreachable — the poll error must end in ONE `Failed`
/// event with its cause too.
#[test]
fn test_job_state_poll_failure_emits_failed_with_the_cause() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock printer");
    let addr = listener.local_addr().expect("mock printer local_addr");
    let printer = std::thread::spawn(move || {
        let mut stream = accept_bounded(&listener);
        // Unreachable from now on: the poll's connect is refused.
        drop(listener);
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("read request head");
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).expect("read request body");

        let mut ipp = vec![1u8, 1, 0, 0];
        ipp.extend_from_slice(&1u32.to_be_bytes());
        ipp.push(0x01);
        ipp_codec::write_attribute(&mut ipp, 0x47, "attributes-charset", b"utf-8");
        ipp_codec::write_attribute(&mut ipp, 0x48, "attributes-natural-language", b"en-us");
        ipp.push(0x02);
        ipp_codec::write_integer_attribute(&mut ipp, 0x21, "job-id", 7);
        ipp_codec::write_integer_attribute(&mut ipp, 0x23, "job-state", 3);
        ipp.push(0x03);
        let response_head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            ipp.len()
        );
        stream
            .write_all(response_head.as_bytes())
            .expect("write response head");
        stream.write_all(&ipp).expect("write response body");
        stream.flush().ok();
    });

    let (result, events) = send_and_collect(&addr.to_string());
    printer.join().expect("join mock printer");

    let err = result.expect_err("the poll cannot reach the printer");
    assert!(
        format!("{err:#}")
            .to_lowercase()
            .contains("connection refused"),
        "returned error must carry the cause chain: {err:#}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.stage == PrintStage::Acknowledged && e.detail.contains("job-id=7")),
        "the Print-Job itself was accepted: {events:?}"
    );
    let failed = failed_events(&events);
    assert_eq!(failed.len(), 1, "exactly one Failed event: {events:?}");
    let detail = &failed[0].detail;
    assert!(
        detail.starts_with("IPP Get-Job-Attributes poll failed"),
        "Failed event detail: {detail}"
    );
    assert!(
        detail.to_lowercase().contains("connection refused"),
        "Failed event carries the cause: {detail}"
    );
}

#[test]
fn test_direct_ipp_name() {
    let backend = DirectIpp::new("10.78.2.9:631".into(), "pwgraster".into(), 600, false);
    assert_eq!(backend.name(), "direct_ipp");
}

#[test]
fn test_ipp_url_without_path() {
    let backend = DirectIpp::new("10.78.2.9:631".into(), "pwgraster".into(), 600, false);
    assert_eq!(backend.ipp_url(), "http://10.78.2.9:631/ipp/print");
}

#[test]
fn test_ipp_url_with_path() {
    let backend = DirectIpp::new(
        "10.78.2.9:631/ipp/print".into(),
        "pwgraster".into(),
        600,
        false,
    );
    assert_eq!(backend.ipp_url(), "http://10.78.2.9:631/ipp/print");
}

#[test]
fn test_printer_uri() {
    let backend = DirectIpp::new("10.78.2.9:631".into(), "pwgraster".into(), 600, false);
    assert_eq!(backend.printer_uri(), "ipp://10.78.2.9:631/ipp/print");
}

#[test]
fn test_tls_url_uses_https() {
    let backend = DirectIpp::new("10.78.5.9:631".into(), "jpeg".into(), 360, true);
    assert_eq!(backend.ipp_url(), "https://10.78.5.9:631/ipp/print");
    assert_eq!(backend.printer_uri(), "ipps://10.78.5.9:631/ipp/print");
}

#[test]
fn test_normalized_address_appends_default_port() {
    let backend = DirectIpp::new("10.78.2.9".into(), "jpeg".into(), 360, false);
    assert_eq!(backend.normalized_address(), "10.78.2.9:631");
    assert_eq!(backend.ipp_url(), "http://10.78.2.9:631/ipp/print");
    assert_eq!(backend.printer_uri(), "ipp://10.78.2.9:631/ipp/print");
}

#[test]
fn test_normalized_address_keeps_explicit_port() {
    let backend = DirectIpp::new("10.78.2.9:9100".into(), "jpeg".into(), 360, false);
    assert_eq!(backend.normalized_address(), "10.78.2.9:9100");
}

#[test]
fn test_normalized_address_keeps_path() {
    let backend = DirectIpp::new("10.78.2.9/printers/foo".into(), "jpeg".into(), 360, false);
    assert_eq!(backend.normalized_address(), "10.78.2.9/printers/foo");
}

#[test]
fn test_normalized_address_with_tls_uses_https() {
    let backend = DirectIpp::new("10.78.5.9".into(), "jpeg".into(), 360, true);
    assert_eq!(backend.ipp_url(), "https://10.78.5.9:631/ipp/print");
}

#[test]
fn test_gs_device_to_ipp_mime_raster() {
    assert_eq!(gs_device_to_ipp_mime("urfrgb"), "image/urf");
    assert_eq!(gs_device_to_ipp_mime("urfgray"), "image/urf");
    assert_eq!(gs_device_to_ipp_mime("pclm"), "application/PCLm");
    assert_eq!(gs_device_to_ipp_mime("jpeg"), "image/jpeg");
    assert_eq!(gs_device_to_ipp_mime("jpeggray"), "image/jpeg");
    assert_eq!(gs_device_to_ipp_mime("png16m"), "image/png");
    assert_eq!(gs_device_to_ipp_mime("pngmono"), "image/png");
    assert_eq!(gs_device_to_ipp_mime("tiffg4"), "image/tiff");
    assert_eq!(gs_device_to_ipp_mime("bmp256"), "image/bmp");
}

#[test]
fn test_gs_device_to_ipp_mime_page_description_languages() {
    // The exact bug this function exists to fix: pxlmono and ps2write
    // must NOT fall through to image/pwg-raster. Xerox Phaser 3020
    // and other strict printers reject 0x040a otherwise.
    assert_eq!(gs_device_to_ipp_mime("ps2write"), "application/postscript");
    assert_eq!(gs_device_to_ipp_mime("pswrite"), "application/postscript");
    assert_eq!(gs_device_to_ipp_mime("pdfwrite"), "application/pdf");
    // IANA-registered MIME for PCL XL has no hyphen between PCL and XL.
    assert_eq!(gs_device_to_ipp_mime("pxlmono"), "application/vnd.hp-PCLXL");
    assert_eq!(
        gs_device_to_ipp_mime("pxlcolor"),
        "application/vnd.hp-PCLXL"
    );
    assert_eq!(gs_device_to_ipp_mime("ljet4"), "application/vnd.hp-PCL");
    assert_eq!(gs_device_to_ipp_mime("pcl5e"), "application/vnd.hp-PCL");
}

#[test]
fn test_gs_device_to_ipp_mime_unknown_falls_back_to_pwg_raster() {
    assert_eq!(gs_device_to_ipp_mime(""), "image/pwg-raster");
    assert_eq!(gs_device_to_ipp_mime("pwgraster"), "image/pwg-raster");
    assert_eq!(
        gs_device_to_ipp_mime("whatever-new-device"),
        "image/pwg-raster"
    );
}
