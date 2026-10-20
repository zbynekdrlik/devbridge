//! Merge the raster documents of several Ghostscript runs into ONE document
//! — one IPP Print-Job, so copies and collation stay the printer's (#111).
//!
//! Only formats whose files are a header followed by self-contained page
//! records can be merged by concatenation:
//!
//! - **URF** (Apple raster, `urfgray` / `urfrgb` / `urfcmyk`, what the HP
//!   LaserJet M110w prints): `UNIRAST\0` + page count (u32 big-endian) + page
//!   records (32-byte page header + compressed lines). Ghostscript 10.04.0
//!   writes the count as 0 ("not counted") even for a multi-page file — a 0 in
//!   any input keeps the merged count 0, exactly what one run produces.
//! - **PWG raster** (`pwgraster`, Linux / macOS Ghostscript only): sync word
//!   `RaS2` once + page records (1796-byte page header + compressed lines).
//!
//! Every other device keeps one Ghostscript run (no merge needed).

use std::io::{Read, Write};

use anyhow::{Context, Result, ensure};

const URF_MAGIC: &[u8] = b"UNIRAST\0";
const PWG_SYNC: &[u8] = b"RaS2";

/// A raster format whose documents can be concatenated page by page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RasterKind {
    Urf,
    Pwg,
}

impl RasterKind {
    /// The mergeable kind a Ghostscript device writes; `None` for every other
    /// device (per-page image files, PCLm, PDF, PostScript, PCL, …).
    pub fn for_device(device: &str) -> Option<Self> {
        match device {
            "urfgray" | "urfrgb" | "urfcmyk" => Some(RasterKind::Urf),
            "pwgraster" => Some(RasterKind::Pwg),
            _ => None,
        }
    }

    fn magic(self) -> &'static [u8] {
        match self {
            RasterKind::Urf => URF_MAGIC,
            RasterKind::Pwg => PWG_SYNC,
        }
    }

    /// File header length: the magic, plus the page count for URF.
    fn header_len(self) -> usize {
        match self {
            RasterKind::Urf => URF_MAGIC.len() + 4,
            RasterKind::Pwg => PWG_SYNC.len(),
        }
    }
}

/// Write the documents `inputs` (in order) as ONE `kind` document to `out`;
/// returns the number of bytes written. A single input is copied unchanged.
pub fn merge<R: Read, W: Write>(kind: RasterKind, inputs: &mut [R], out: &mut W) -> Result<u64> {
    ensure!(!inputs.is_empty(), "no raster document to merge");
    let magic = kind.magic();
    let mut counts = Vec::with_capacity(inputs.len());
    for (i, input) in inputs.iter_mut().enumerate() {
        let mut header = vec![0u8; kind.header_len()];
        input.read_exact(&mut header).with_context(|| {
            format!(
                "raster document {} is shorter than its {kind:?} header",
                i + 1
            )
        })?;
        ensure!(
            header.starts_with(magic),
            "raster document {} is not a {kind:?} document (starts with {:02x?})",
            i + 1,
            &header[..magic.len()]
        );
        if kind == RasterKind::Urf {
            let count = u32::from_be_bytes(
                header[magic.len()..]
                    .try_into()
                    .expect("URF header is magic + 4 bytes"),
            );
            counts.push(count);
        }
    }
    let mut written = 0u64;
    out.write_all(magic)?;
    written += magic.len() as u64;
    if kind == RasterKind::Urf {
        let count = merged_page_count(&counts).context("URF page count overflows u32")?;
        out.write_all(&count.to_be_bytes())?;
        written += 4;
    }
    for input in inputs.iter_mut() {
        written += std::io::copy(input, out)?;
    }
    out.flush()?;
    Ok(written)
}

/// Page count of the merged URF document: the sum, but 0 ("not counted")
/// as soon as any input did not count its pages. `None` = overflow.
fn merged_page_count(counts: &[u32]) -> Option<u32> {
    if counts.contains(&0) {
        return Some(0);
    }
    counts
        .iter()
        .try_fold(0u32, |sum, &count| sum.checked_add(count))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A URF document: header with `count`, then `pages` opaque page records.
    fn urf(count: u32, pages: &[&[u8]]) -> Vec<u8> {
        let mut d = URF_MAGIC.to_vec();
        d.extend_from_slice(&count.to_be_bytes());
        for p in pages {
            d.extend_from_slice(p);
        }
        d
    }

    fn pwg(pages: &[&[u8]]) -> Vec<u8> {
        let mut d = PWG_SYNC.to_vec();
        for p in pages {
            d.extend_from_slice(p);
        }
        d
    }

    fn run(kind: RasterKind, docs: &[Vec<u8>]) -> Result<Vec<u8>> {
        let mut inputs: Vec<&[u8]> = docs.iter().map(Vec::as_slice).collect();
        let mut out = Vec::new();
        let written = merge(kind, &mut inputs, &mut out)?;
        assert_eq!(written, out.len() as u64, "returned byte count");
        Ok(out)
    }

    #[test]
    fn devices_map_to_their_mergeable_kind() {
        assert_eq!(RasterKind::for_device("urfgray"), Some(RasterKind::Urf));
        assert_eq!(RasterKind::for_device("urfrgb"), Some(RasterKind::Urf));
        assert_eq!(RasterKind::for_device("urfcmyk"), Some(RasterKind::Urf));
        assert_eq!(RasterKind::for_device("pwgraster"), Some(RasterKind::Pwg));
        for other in [
            "jpeg", "pnggray", "png16m", "pclm", "pdfwrite", "ps2write", "pxlmono", "ljet4",
            "tiffg4", "urf", "", "URFGRAY",
        ] {
            assert_eq!(RasterKind::for_device(other), None, "{other}");
        }
    }

    #[test]
    fn a_single_document_is_copied_unchanged() {
        let doc = urf(0, &[b"page-one-record", b"page-two-record"]);
        assert_eq!(
            run(RasterKind::Urf, std::slice::from_ref(&doc)).unwrap(),
            doc
        );
        let doc = urf(2, &[b"p1", b"p2"]);
        assert_eq!(
            run(RasterKind::Urf, std::slice::from_ref(&doc)).unwrap(),
            doc
        );
        let doc = pwg(&[b"pwg-page"]);
        assert_eq!(
            run(RasterKind::Pwg, std::slice::from_ref(&doc)).unwrap(),
            doc
        );
    }

    #[test]
    fn urf_documents_concatenate_their_page_records_under_one_header() {
        let merged = run(
            RasterKind::Urf,
            &[
                urf(2, &[b"A1", b"A2"]),
                urf(1, &[b"B1"]),
                urf(3, &[b"C1", b"C2", b"C3"]),
            ],
        )
        .unwrap();
        assert_eq!(merged, urf(6, &[b"A1", b"A2", b"B1", b"C1", b"C2", b"C3"]));
    }

    #[test]
    fn urf_not_counted_by_ghostscript_stays_not_counted() {
        // gs 10.04.0 urfgray writes page count 0 even for 3 pages.
        let merged = run(
            RasterKind::Urf,
            &[urf(0, &[b"P1"]), urf(0, &[b"L2"]), urf(0, &[b"P3"])],
        )
        .unwrap();
        assert_eq!(merged, urf(0, &[b"P1", b"L2", b"P3"]));
        let merged = run(
            RasterKind::Urf,
            &[urf(2, &[b"P1", b"P2"]), urf(0, &[b"L3"])],
        )
        .unwrap();
        assert_eq!(
            &merged[8..12],
            &0u32.to_be_bytes(),
            "one uncounted input = uncounted"
        );
    }

    #[test]
    fn urf_page_count_overflow_is_an_error() {
        let docs = [urf(u32::MAX, &[b"x"]), urf(1, &[b"y"])];
        assert!(run(RasterKind::Urf, &docs).is_err());
        assert_eq!(merged_page_count(&[u32::MAX, 1]), None);
        assert_eq!(merged_page_count(&[u32::MAX - 1, 1]), Some(u32::MAX));
        assert_eq!(merged_page_count(&[4, 0, 5]), Some(0));
        assert_eq!(merged_page_count(&[4, 5]), Some(9));
    }

    #[test]
    fn pwg_documents_keep_one_sync_word() {
        let merged = run(
            RasterKind::Pwg,
            &[pwg(&[b"H1D1", b"H2D2"]), pwg(&[b"H3D3"])],
        )
        .unwrap();
        assert_eq!(merged, b"RaS2H1D1H2D2H3D3");
    }

    #[test]
    fn a_document_of_another_format_is_refused() {
        assert!(run(RasterKind::Urf, &[urf(1, &[b"a"]), pwg(&[b"b"])]).is_err());
        assert!(run(RasterKind::Pwg, &[pwg(&[b"a"]), urf(1, &[b"b"])]).is_err());
        let err = run(RasterKind::Urf, &[b"PNG-not-urf-data".to_vec()]).unwrap_err();
        assert!(
            format!("{err:#}").contains("raster document 1 is not a Urf document"),
            "{err:#}"
        );
    }

    #[test]
    fn a_truncated_header_is_refused() {
        let err = run(RasterKind::Urf, &[b"UNIRAST\0\0\0".to_vec()]).unwrap_err();
        assert!(
            format!("{err:#}").contains("raster document 1 is shorter than its Urf header"),
            "{err:#}"
        );
        let err = run(RasterKind::Pwg, &[b"RaS2".to_vec(), b"RaS".to_vec()]).unwrap_err();
        assert!(
            format!("{err:#}").contains("raster document 2 is shorter than its Pwg header"),
            "{err:#}"
        );
        assert!(run(RasterKind::Urf, &[]).is_err());
    }
}
