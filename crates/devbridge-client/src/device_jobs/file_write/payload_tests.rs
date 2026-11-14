//! `file.write` payload checks (#130): the contract's file name pattern plus
//! the names it lets through but a folder must never see, and the content's
//! base64 / size / sha256.

use base64::Engine as _;
use serde_json::{Value, json};

use super::payload::{
    MAX_FILE_BYTES, MAX_FILENAME_LEN, filename_problem, parse_payload, sha256_hex,
};

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn payload_for(filename: &str, bytes: &[u8]) -> Value {
    json!({
        "filename": filename,
        "encoding": "cp1250",
        "content_b64": b64(bytes),
        "size": bytes.len(),
        "sha256": sha256_hex(bytes),
    })
}

#[test]
fn test_filename_accepts_the_contract_pattern() {
    let longest = format!("A{}", "b".repeat(MAX_FILENAME_LEN - 1));
    for ok in [
        "ORDER_SNV408_P00012_20261113141502.txt",
        "a",
        "0",
        "x.y.z",
        "a-b_c.TXT",
        longest.as_str(),
        "CONSOLE.txt",
        "nul2.txt",
        "COM10.txt",
    ] {
        assert_eq!(filename_problem(ok), None, "{ok}");
    }
}

#[test]
fn test_filename_refuses_paths_traversal_and_odd_characters() {
    let too_long = format!("A{}", "b".repeat(MAX_FILENAME_LEN));
    let cases: [(&str, &str); 17] = [
        ("", "empty"),
        (too_long.as_str(), "longer than 100"),
        (".hidden", "start with a letter or digit"),
        ("_x", "start with a letter or digit"),
        ("-x", "start with a letter or digit"),
        ("..\\evil.txt", "start with a letter or digit"),
        ("a\\b.txt", "path separator"),
        ("a/b.txt", "path separator"),
        ("C:evil.txt", "path separator"),
        ("a..b", "'..'"),
        ("ORDER..txt", "'..'"),
        ("a.", "end with '.'"),
        ("a b.txt", "' '"),
        ("objednávka.txt", "'á'"),
        ("a\u{0}.txt", "'\\0'"),
        ("a*.txt", "'*'"),
        ("a\n", "'\\n'"),
    ];
    for (name, want) in cases {
        let problem = filename_problem(name).unwrap_or_else(|| panic!("{name:?} accepted"));
        assert!(problem.contains(want), "{name:?}: {problem}");
    }
}

#[test]
fn test_filename_refuses_windows_device_names_in_any_case() {
    for name in [
        "CON", "con.txt", "NUL.txt", "nul", "Prn.edi", "AUX.a.b", "COM0", "COM1", "com9.txt",
        "LPT0.txt", "LPT1.txt", "lpt9",
    ] {
        let problem = filename_problem(name).unwrap_or_else(|| panic!("{name:?} accepted"));
        assert!(problem.contains("Windows device name"), "{name}: {problem}");
    }
}

#[test]
fn test_parse_payload_accepts_a_valid_payload() {
    let bytes = b"HDR line\r\nPOS line\r\n";
    let parsed = parse_payload(&payload_for("ORDER_1.txt", bytes)).unwrap();
    assert_eq!(parsed.filename, "ORDER_1.txt");
    assert_eq!(parsed.bytes, bytes);
    assert_eq!(parsed.sha256, sha256_hex(bytes));
    // encoding is optional, an upper-case sha256 is the same digest
    let mut p = payload_for("ORDER_2.txt", bytes);
    p.as_object_mut().unwrap().remove("encoding");
    p["sha256"] = json!(sha256_hex(bytes).to_uppercase());
    assert_eq!(parse_payload(&p).unwrap().sha256, sha256_hex(bytes));
    // an empty file is a file
    let empty = parse_payload(&payload_for("EMPTY.txt", b"")).unwrap();
    assert!(empty.bytes.is_empty());
    // its Debug never shows the content
    let dbg = format!("{parsed:?}");
    assert!(dbg.contains("size: 20"), "{dbg}");
    assert!(!dbg.contains("HDR"), "{dbg}");
}

#[test]
fn test_parse_payload_refuses_what_breaks_the_contract() {
    let bytes = b"HDR line\r\n";
    let good = payload_for("ORDER_1.txt", bytes);
    let with = |key: &str, value: Value| {
        let mut p = good.clone();
        p[key] = value;
        p
    };
    let without = |key: &str| {
        let mut p = good.clone();
        p.as_object_mut().unwrap().remove(key);
        p
    };
    let cases: Vec<(Value, &str)> = vec![
        (json!([]), "not an object"),
        (json!(null), "not an object"),
        (without("filename"), "no 'filename'"),
        (without("content_b64"), "no 'content_b64'"),
        (without("size"), "no 'size'"),
        (without("sha256"), "no 'sha256'"),
        (with("folder", json!("C:\\x")), "does not define: folder"),
        (with("path", json!("x")), "does not define: path"),
        (with("filename", json!(7)), "filename is not a string"),
        (with("filename", json!("..\\x.txt")), "start with a letter"),
        (with("filename", json!("in/x.txt")), "path separator"),
        (with("encoding", json!(1250)), "encoding is not a string"),
        (with("size", json!(-1)), "size is not a whole number"),
        (with("size", json!(10.5)), "size is not a whole number"),
        (with("size", json!("10")), "size is not a whole number"),
        (with("size", json!(11)), "decodes to 10 bytes, size says 11"),
        (
            with("size", json!(MAX_FILE_BYTES + 1)),
            "over the 4194304-byte limit",
        ),
        (with("sha256", json!("abc")), "64 hex characters"),
        (with("sha256", json!("g".repeat(64))), "64 hex characters"),
        (
            with("sha256", json!(sha256_hex(b"other"))),
            "does not match the payload's sha256",
        ),
        (with("content_b64", json!(5)), "content_b64 is not a string"),
        (
            with("content_b64", json!("not base64!")),
            "not valid base64",
        ),
        (
            with("content_b64", json!("A".repeat(MAX_FILE_BYTES / 3 * 4 + 8))),
            "content_b64 is over",
        ),
    ];
    for (payload, want) in cases {
        let problem = parse_payload(&payload).expect_err(want);
        assert!(problem.contains(want), "{want}: {problem}");
        assert!(
            !problem.contains("HDR"),
            "the content never appears: {problem}"
        );
    }
}

#[test]
fn test_parse_payload_accepts_exactly_the_size_limit() {
    let bytes = vec![b'x'; MAX_FILE_BYTES];
    let parsed = parse_payload(&payload_for("BIG.txt", &bytes)).unwrap();
    assert_eq!(parsed.bytes.len(), MAX_FILE_BYTES);
}

#[test]
fn test_sha256_hex_known_vector() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}
