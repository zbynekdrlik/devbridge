/// IPP 1.1 binary codec for Print-Job (0x0002) and Get-Job-Attributes (0x0009).
/// Reference: RFC 8010
use anyhow::{Result, anyhow};

// IPP version
const IPP_VERSION_MAJOR: u8 = 1;
const IPP_VERSION_MINOR: u8 = 1;

// Operation IDs
pub const PRINT_JOB: u16 = 0x0002;
pub const GET_JOB_ATTRIBUTES: u16 = 0x0009;

// Attribute group tags
const OPERATION_ATTRIBUTES_TAG: u8 = 0x01;
#[allow(dead_code)]
const JOB_ATTRIBUTES_TAG: u8 = 0x02;
const END_OF_ATTRIBUTES_TAG: u8 = 0x03;

// Value tags
const CHARSET_TAG: u8 = 0x47;
const NATURAL_LANGUAGE_TAG: u8 = 0x48;
const URI_TAG: u8 = 0x45;
const MIME_MEDIA_TYPE_TAG: u8 = 0x49;
const NAME_WITHOUT_LANGUAGE_TAG: u8 = 0x42;
const INTEGER_TAG: u8 = 0x21;
#[allow(dead_code)]
const ENUM_TAG: u8 = 0x23;

/// An individual IPP attribute parsed from a response.
#[derive(Debug, Clone)]
pub struct IppAttribute {
    pub name: String,
    pub tag: u8,
    pub value: Vec<u8>,
}

impl IppAttribute {
    /// Interpret the value as a UTF-8 string, if possible.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.value).ok()
    }

    /// Interpret the value as a big-endian i32 (requires exactly 4 bytes).
    pub fn as_i32(&self) -> Option<i32> {
        if self.value.len() == 4 {
            Some(i32::from_be_bytes([
                self.value[0],
                self.value[1],
                self.value[2],
                self.value[3],
            ]))
        } else {
            None
        }
    }
}

/// A parsed IPP response.
#[derive(Debug)]
pub struct IppResponse {
    pub status_code: u16,
    pub request_id: u32,
    pub attributes: Vec<IppAttribute>,
}

impl IppResponse {
    /// Return the first attribute with the given name, if any.
    pub fn get(&self, name: &str) -> Option<&IppAttribute> {
        self.attributes.iter().find(|a| a.name == name)
    }

    /// Returns true when the status code indicates success (0x0000–0x00FF).
    pub fn is_success(&self) -> bool {
        self.status_code <= 0x00FF
    }
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

/// Write a single IPP attribute into `buf`.
///
/// Wire format:
/// ```text
/// value-tag    (1 byte)
/// name-length  (2 bytes, big-endian)
/// name         (N bytes)
/// value-length (2 bytes, big-endian)
/// value        (M bytes)
/// ```
pub fn write_attribute(buf: &mut Vec<u8>, tag: u8, name: &str, value_bytes: &[u8]) {
    buf.push(tag);
    let name_bytes = name.as_bytes();
    buf.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(name_bytes);
    buf.extend_from_slice(&(value_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(value_bytes);
}

/// Write an integer (i32) IPP attribute. The value is always 4 bytes big-endian.
pub fn write_integer_attribute(buf: &mut Vec<u8>, tag: u8, name: &str, value: i32) {
    write_attribute(buf, tag, name, &value.to_be_bytes());
}

// ---------------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------------

/// Build an IPP Print-Job request (operation 0x0002).
///
/// The caller appends the document data after the returned bytes.
pub fn build_print_job_request(
    printer_uri: &str,
    document_format: &str,
    job_name: &str,
    request_id: u32,
) -> Vec<u8> {
    let mut buf = Vec::new();

    // IPP header
    buf.push(IPP_VERSION_MAJOR);
    buf.push(IPP_VERSION_MINOR);
    buf.extend_from_slice(&PRINT_JOB.to_be_bytes());
    buf.extend_from_slice(&request_id.to_be_bytes());

    // Operation attributes group
    buf.push(OPERATION_ATTRIBUTES_TAG);

    write_attribute(&mut buf, CHARSET_TAG, "attributes-charset", b"utf-8");
    write_attribute(
        &mut buf,
        NATURAL_LANGUAGE_TAG,
        "attributes-natural-language",
        b"en-us",
    );
    write_attribute(
        &mut buf,
        URI_TAG,
        "printer-uri",
        printer_uri.as_bytes(),
    );
    write_attribute(
        &mut buf,
        MIME_MEDIA_TYPE_TAG,
        "document-format",
        document_format.as_bytes(),
    );
    write_attribute(
        &mut buf,
        NAME_WITHOUT_LANGUAGE_TAG,
        "job-name",
        job_name.as_bytes(),
    );

    buf.push(END_OF_ATTRIBUTES_TAG);
    buf
}

/// Build an IPP Get-Job-Attributes request (operation 0x0009).
pub fn build_get_job_attributes_request(
    printer_uri: &str,
    job_id: i32,
    request_id: u32,
) -> Vec<u8> {
    let mut buf = Vec::new();

    // IPP header
    buf.push(IPP_VERSION_MAJOR);
    buf.push(IPP_VERSION_MINOR);
    buf.extend_from_slice(&GET_JOB_ATTRIBUTES.to_be_bytes());
    buf.extend_from_slice(&request_id.to_be_bytes());

    // Operation attributes group
    buf.push(OPERATION_ATTRIBUTES_TAG);

    write_attribute(&mut buf, CHARSET_TAG, "attributes-charset", b"utf-8");
    write_attribute(
        &mut buf,
        NATURAL_LANGUAGE_TAG,
        "attributes-natural-language",
        b"en-us",
    );
    write_attribute(
        &mut buf,
        URI_TAG,
        "printer-uri",
        printer_uri.as_bytes(),
    );
    write_integer_attribute(&mut buf, INTEGER_TAG, "job-id", job_id);

    buf.push(END_OF_ATTRIBUTES_TAG);
    buf
}

// ---------------------------------------------------------------------------
// Response parser
// ---------------------------------------------------------------------------

/// Parse a raw IPP response into an [`IppResponse`].
///
/// Returns an error if the data is too short to be a valid IPP response.
pub fn parse_response(data: &[u8]) -> Result<IppResponse> {
    if data.len() < 8 {
        return Err(anyhow!("IPP response too short: {} bytes", data.len()));
    }

    // Bytes 0-1: version (ignored for now)
    let status_code = u16::from_be_bytes([data[2], data[3]]);
    let request_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);

    let mut attributes: Vec<IppAttribute> = Vec::new();
    let mut pos = 8usize;
    let mut last_name = String::new();

    while pos < data.len() {
        let tag = data[pos];
        pos += 1;

        // Group delimiter tags (0x00–0x0F)
        if tag <= 0x0F {
            if tag == END_OF_ATTRIBUTES_TAG {
                break;
            }
            // Other group delimiters (operation-attributes, job-attributes, …)
            // just continue reading attributes within the new group.
            continue;
        }

        // Value attribute
        if pos + 2 > data.len() {
            return Err(anyhow!("IPP response truncated at name-length"));
        }
        let name_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        let name = if name_len == 0 {
            // Additional value for the previous attribute — reuse last name
            last_name.clone()
        } else {
            if pos + name_len > data.len() {
                return Err(anyhow!("IPP response truncated at name"));
            }
            let n = std::str::from_utf8(&data[pos..pos + name_len])
                .map_err(|e| anyhow!("Invalid UTF-8 in attribute name: {e}"))?
                .to_string();
            pos += name_len;
            last_name = n.clone();
            n
        };

        if pos + 2 > data.len() {
            return Err(anyhow!("IPP response truncated at value-length"));
        }
        let value_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        if pos + value_len > data.len() {
            return Err(anyhow!("IPP response truncated at value"));
        }
        let value = data[pos..pos + value_len].to_vec();
        pos += value_len;

        attributes.push(IppAttribute { name, tag, value });
    }

    Ok(IppResponse {
        status_code,
        request_id,
        attributes,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_print_job_request_header() {
        let req = build_print_job_request(
            "ipp://printer.local/ipp/print",
            "application/pdf",
            "TestJob",
            42,
        );

        // Version 1.1
        assert_eq!(req[0], 1, "major version");
        assert_eq!(req[1], 1, "minor version");

        // Operation 0x0002
        assert_eq!(req[2], 0x00, "operation high byte");
        assert_eq!(req[3], 0x02, "operation low byte");

        // Request-id = 42 (big-endian u32)
        assert_eq!(
            u32::from_be_bytes([req[4], req[5], req[6], req[7]]),
            42,
            "request-id"
        );
    }

    #[test]
    fn test_build_print_job_contains_required_attributes() {
        let printer_uri = "ipp://printer.local/ipp/print";
        let req = build_print_job_request(printer_uri, "application/pdf", "TestJob", 1);

        // Check the serialised bytes contain the charset value
        assert!(
            req.windows(5).any(|w| w == b"utf-8"),
            "should contain charset utf-8"
        );

        // Check the printer URI is present in the serialised bytes
        assert!(
            req.windows(printer_uri.len())
                .any(|w| w == printer_uri.as_bytes()),
            "should contain printer-uri"
        );
    }

    #[test]
    fn test_parse_success_response() {
        let mut data = Vec::new();
        data.push(1);
        data.push(1); // version 1.1
        data.extend_from_slice(&0x0000u16.to_be_bytes()); // success
        data.extend_from_slice(&1u32.to_be_bytes()); // request-id = 1

        data.push(0x01); // operation-attributes-tag

        // attributes-charset = utf-8
        data.push(CHARSET_TAG);
        data.extend_from_slice(&18u16.to_be_bytes()); // "attributes-charset"
        data.extend_from_slice(b"attributes-charset");
        data.extend_from_slice(&5u16.to_be_bytes());
        data.extend_from_slice(b"utf-8");

        data.push(0x02); // job-attributes-tag

        // job-id = 42
        data.push(INTEGER_TAG);
        data.extend_from_slice(&6u16.to_be_bytes()); // "job-id"
        data.extend_from_slice(b"job-id");
        data.extend_from_slice(&4u16.to_be_bytes());
        data.extend_from_slice(&42i32.to_be_bytes());

        // job-state = 3
        data.push(ENUM_TAG);
        data.extend_from_slice(&9u16.to_be_bytes()); // "job-state"
        data.extend_from_slice(b"job-state");
        data.extend_from_slice(&4u16.to_be_bytes());
        data.extend_from_slice(&3i32.to_be_bytes());

        data.push(END_OF_ATTRIBUTES_TAG);

        let resp = parse_response(&data).expect("parse should succeed");

        assert!(resp.is_success(), "status should be success");
        assert_eq!(resp.status_code, 0x0000);
        assert_eq!(resp.request_id, 1);

        let job_id = resp.get("job-id").expect("job-id attribute");
        assert_eq!(job_id.as_i32(), Some(42));

        let job_state = resp.get("job-state").expect("job-state attribute");
        assert_eq!(job_state.as_i32(), Some(3));
    }

    #[test]
    fn test_parse_error_response() {
        let mut data = Vec::new();
        data.push(1);
        data.push(1); // version 1.1
        data.extend_from_slice(&0x0400u16.to_be_bytes()); // client-error
        data.extend_from_slice(&2u32.to_be_bytes()); // request-id = 2

        data.push(0x01); // operation-attributes-tag

        // attributes-charset = utf-8
        data.push(CHARSET_TAG);
        data.extend_from_slice(&18u16.to_be_bytes());
        data.extend_from_slice(b"attributes-charset");
        data.extend_from_slice(&5u16.to_be_bytes());
        data.extend_from_slice(b"utf-8");

        data.push(END_OF_ATTRIBUTES_TAG);

        let resp = parse_response(&data).expect("parse should succeed");

        assert!(!resp.is_success(), "status 0x0400 should not be success");
        assert_eq!(resp.status_code, 0x0400);
    }

    #[test]
    fn test_build_get_job_attributes_request() {
        let req = build_get_job_attributes_request("ipp://printer.local/ipp/print", 7, 5);

        // Version 1.1
        assert_eq!(req[0], 1);
        assert_eq!(req[1], 1);

        // Operation 0x0009
        assert_eq!(
            u16::from_be_bytes([req[2], req[3]]),
            GET_JOB_ATTRIBUTES,
            "operation should be GET_JOB_ATTRIBUTES"
        );

        // request-id = 5
        assert_eq!(u32::from_be_bytes([req[4], req[5], req[6], req[7]]), 5);
    }
}
