//! Page orientation of the direct_ipp raster render (#111) against the REAL
//! Ghostscript on the runner (the Test job installs it; a missing gs fails
//! these tests, it never skips them).
//!
//! Every printer DevBridge drives feeds paper portrait. A landscape PDF page
//! rendered at its own size became a raster wider than the paper (A4
//! landscape -> 7016 x 4961 at 600 dpi) and the HP LaserJet M110w aborted
//! the job (`document-format-error`). `ghostscript::render` must put every
//! page of a raster document on portrait-fed media at scale 1, keep portrait
//! pages exactly as before, and deliver ONE document (one IPP Print-Job).
//!
//! Linux Ghostscript has no `urfgray` (the stores' Windows gs has no
//! `pwgraster`), so these tests render through `pwgraster`, whose page
//! header carries the raster width / height; the URF merge is unit-tested in
//! `raster_merge.rs` and proven on Windows gs by the E2E direct_ipp step.

use std::path::{Path, PathBuf};

use devbridge_client::ghostscript;
use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};
use tokio_util::sync::CancellationToken;

/// A4 / A5 / A3 in PostScript points.
const A4: (u32, u32) = (595, 842);
const A5: (u32, u32) = (420, 595);
const A3: (u32, u32) = (842, 1191);
/// 50 dpi keeps every render fast; the geometry is resolution-independent.
const DPI: u32 = 50;

/// One synthetic PDF page: MediaBox width / height (points), `/Rotate`, and a
/// text label so the page is not blank.
struct Page {
    width: u32,
    height: u32,
    rotate: u32,
    label: &'static str,
}

fn page(size: (u32, u32), label: &'static str) -> Page {
    Page {
        width: size.0,
        height: size.1,
        rotate: 0,
        label,
    }
}

fn landscape(size: (u32, u32), label: &'static str) -> Page {
    page((size.1, size.0), label)
}

/// Write a minimal valid PDF (catalog, page tree, Helvetica, one content
/// stream per page, a correct xref table).
fn write_pdf(dir: &Path, name: &str, pages: &[Page]) -> PathBuf {
    let n = pages.len();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")).into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    for (i, p) in pages.iter().enumerate() {
        let content = format!(
            "BT /F1 36 Tf 40 {} Td ({}) Tj ET 20 20 {} 30 re f",
            p.height - 80,
            p.label,
            p.width - 40
        );
        let rotate = if p.rotate == 0 {
            String::new()
        } else {
            format!(" /Rotate {}", p.rotate)
        };
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}]{rotate} \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                p.width,
                p.height,
                5 + 2 * i
            )
            .into_bytes(),
        );
        objects.push(
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            )
            .into_bytes(),
        );
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(obj);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    let path = dir.join(name);
    std::fs::write(&path, out).expect("write test PDF");
    path
}

/// Width / height of every page of a PWG raster document (sync word `RaS2`
/// once, then per page a 1796-byte header + PackBits-style compressed lines).
fn pwg_pages(data: &[u8]) -> Vec<(u32, u32)> {
    assert_eq!(&data[..4], b"RaS2", "not ONE PWG raster document");
    let be = |at: usize| u32::from_be_bytes(data[at..at + 4].try_into().unwrap());
    let mut pos = 4;
    let mut pages = Vec::new();
    while pos < data.len() {
        assert_eq!(
            &data[pos..pos + 9],
            b"PwgRaster",
            "page header {} not where the previous page ended",
            pages.len() + 1
        );
        let (width, height) = (be(pos + 372), be(pos + 376));
        let bits_per_pixel = be(pos + 388) as usize;
        let bytes_per_line = be(pos + 392) as usize;
        pos += 1796;
        let unit = bits_per_pixel.div_ceil(8).max(1);
        let units_per_line = bytes_per_line / unit;
        let mut y = 0u32;
        while y < height {
            let repeat = data[pos] as u32;
            pos += 1;
            let mut x = 0usize;
            while x < units_per_line {
                let c = data[pos] as usize;
                pos += 1;
                if c < 128 {
                    pos += unit;
                    x += c + 1;
                } else {
                    pos += (257 - c) * unit;
                    x += 257 - c;
                }
            }
            y += repeat + 1;
        }
        pages.push((width, height));
    }
    pages
}

struct Rendered {
    data: Vec<u8>,
    events: Vec<PrintJobEvent>,
    page_files: usize,
}

fn render(pdf: &Path, device: &str) -> Rendered {
    let (tx, _) = tokio::sync::broadcast::channel(64);
    let emitter = EventEmitter::new(tx);
    let mut rx = emitter.subscribe();
    let output = pdf.with_extension("pwg");
    let result = ghostscript::render(
        pdf,
        &output,
        device,
        DPI,
        "job-111",
        &emitter,
        &CancellationToken::new(),
    )
    .expect("Ghostscript render (is Ghostscript installed on this runner?)");
    let data = std::fs::read(&result.page_files[0]).expect("read rendered raster");
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    Rendered {
        data,
        events,
        page_files: result.page_files.len(),
    }
}

/// The same Ghostscript call `render` made before #111 (no orientation
/// handling): the reference a portrait page must stay byte-identical to.
fn plain_render(pdf: &Path, device: &str, out: &Path) -> Vec<u8> {
    let gs = ghostscript::find_ghostscript().expect("Ghostscript on this runner");
    let status = std::process::Command::new(gs)
        .args([
            "-dNOPAUSE",
            "-dBATCH",
            "-dSAFER",
            &format!("-sDEVICE={device}"),
            &format!("-r{DPI}"),
            &format!("-sOutputFile={}", out.display()),
        ])
        .arg(pdf)
        .output()
        .expect("run Ghostscript");
    assert!(status.status.success(), "plain gs render failed");
    std::fs::read(out).expect("read plain raster")
}

/// Raster size of a page of `size` points rendered upright at [`DPI`].
fn upright(dir: &Path, size: (u32, u32)) -> (u32, u32) {
    let pdf = write_pdf(
        dir,
        &format!("ref-{}x{}.pdf", size.0, size.1),
        &[page(size, "R")],
    );
    let data = plain_render(&pdf, "pwgraster", &pdf.with_extension("ref.pwg"));
    pwg_pages(&data)[0]
}

#[test]
fn landscape_a4_page_renders_on_portrait_a4_at_scale_1() {
    let dir = tempfile::tempdir().unwrap();
    let a4_portrait = upright(dir.path(), A4);
    assert!(a4_portrait.0 < a4_portrait.1);

    let pdf = write_pdf(dir.path(), "a4l.pdf", &[landscape(A4, "L")]);
    let r = render(&pdf, "pwgraster");

    assert_eq!(
        pwg_pages(&r.data),
        vec![a4_portrait],
        "an A4 landscape page must come out as an A4 PORTRAIT raster (rotated 90°, scale 1)"
    );
}

#[test]
fn page_rotated_by_its_pdf_rotate_key_renders_portrait() {
    // MediaBox portrait + /Rotate 90 = a landscape page as displayed.
    let dir = tempfile::tempdir().unwrap();
    let a4_portrait = upright(dir.path(), A4);
    let mut rotated = page(A4, "R90");
    rotated.rotate = 90;
    let pdf = write_pdf(dir.path(), "rot90.pdf", &[rotated]);

    let r = render(&pdf, "pwgraster");

    assert_eq!(pwg_pages(&r.data), vec![a4_portrait]);
}

#[test]
fn mixed_document_is_one_portrait_raster_document() {
    let dir = tempfile::tempdir().unwrap();
    let a4_portrait = upright(dir.path(), A4);
    let a5_portrait = upright(dir.path(), A5);
    let pdf = write_pdf(
        dir.path(),
        "mixed.pdf",
        &[page(A4, "P1"), landscape(A4, "L2"), page(A5, "A5")],
    );

    let r = render(&pdf, "pwgraster");

    assert_eq!(r.page_files, 1, "one raster document = one IPP Print-Job");
    assert_eq!(
        pwg_pages(&r.data),
        vec![a4_portrait, a4_portrait, a5_portrait],
        "P / L / A5: the landscape page rotated onto A4 portrait, the A5 page NOT enlarged"
    );
}

#[test]
fn landscape_pages_of_different_sizes_each_keep_their_size() {
    let dir = tempfile::tempdir().unwrap();
    let a4_portrait = upright(dir.path(), A4);
    let a3_portrait = upright(dir.path(), A3);
    let pdf = write_pdf(
        dir.path(),
        "a4l-a3l.pdf",
        &[landscape(A4, "L-A4"), landscape(A3, "L-A3")],
    );

    let r = render(&pdf, "pwgraster");

    assert_eq!(pwg_pages(&r.data), vec![a4_portrait, a3_portrait]);
}

#[test]
fn portrait_pages_stay_byte_identical_to_the_plain_render() {
    let dir = tempfile::tempdir().unwrap();
    for (name, pages) in [
        ("a4p.pdf", vec![page(A4, "P")]),
        ("a5p.pdf", vec![page(A5, "A5")]),
        ("a4p-a5p.pdf", vec![page(A4, "P"), page(A5, "A5")]),
    ] {
        let pdf = write_pdf(dir.path(), name, &pages);
        let plain = plain_render(&pdf, "pwgraster", &pdf.with_extension("plain.pwg"));

        let r = render(&pdf, "pwgraster");

        assert!(
            r.data == plain,
            "{name}: a portrait-only document must render byte-identical to the pre-#111 call"
        );
    }
}

#[test]
fn rendered_event_reports_the_orientation_once_per_job() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = write_pdf(
        dir.path(),
        "mixed.pdf",
        &[page(A4, "P1"), landscape(A4, "L2"), page(A5, "A5")],
    );

    let r = render(&pdf, "pwgraster");

    let stages: Vec<PrintStage> = r.events.iter().map(|e| e.stage).collect();
    assert_eq!(
        stages,
        vec![PrintStage::Rendering, PrintStage::Rendered],
        "one rendering + one rendered event per job, whatever the number of Ghostscript runs"
    );
    let detail = &r.events[1].detail;
    assert!(
        detail.contains("3 pages")
            && detail.contains("orientation: 2 portrait, 1 landscape (rotated 90°)"),
        "rendered event must carry the orientation: {detail}"
    );
}

#[test]
fn per_page_image_devices_keep_their_own_page_size() {
    // jpeg / png devices (one file per page, one Print-Job per page) are out
    // of the #111 scope: they keep today's render.
    let dir = tempfile::tempdir().unwrap();
    let pdf = write_pdf(dir.path(), "a4l.pdf", &[landscape(A4, "L")]);

    let r = render(&pdf, "pnggray");

    let width = u32::from_be_bytes(r.data[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(r.data[20..24].try_into().unwrap());
    assert!(
        width > height,
        "pnggray stays at the page's own (landscape) size: {width}x{height}"
    );
}
