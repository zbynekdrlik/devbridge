//! The URF (Apple raster) a direct_ipp client sends to an HP LaserJet M110w
//! (`urfgray`), decoded for E2E step 38 (`direct_ipp.rs`, issues #111 /
//! #112): page sizes, the dark pixels of every page, and the #111 check that
//! every page is PORTRAIT and the landscape page was rotated at SCALE 1.

use anyhow::{Context, Result, ensure};

// ── URF (Apple raster) — what the HP LaserJet M110w gets ──────────────────

/// One URF page: its header and its dark pixels (8-bit gray `< 128`; 0 for
/// other pixel sizes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrfPage {
    pub bits_per_pixel: u8,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub dark: u64,
}

/// Header page count (0 = "not counted", what gs 10.04 writes) + every page,
/// found by decoding the compressed lines (a page header is only where the
/// previous page's data ends).
pub fn urf_pages(data: &[u8]) -> Result<(u32, Vec<UrfPage>)> {
    ensure!(
        data.len() >= 12 && data.starts_with(b"UNIRAST\0"),
        "not a URF document (starts with {:02x?})",
        &data[..data.len().min(8)]
    );
    let be = |at: usize| -> Result<u32> {
        let b = data.get(at..at + 4).context("truncated URF header")?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let count = be(8)?;
    let mut pos = 12;
    let mut pages = Vec::new();
    while pos < data.len() {
        let header = data
            .get(pos..pos + 32)
            .context("truncated URF page header")?;
        let mut page = UrfPage {
            bits_per_pixel: header[0],
            width: be(pos + 12)?,
            height: be(pos + 16)?,
            dpi: be(pos + 20)?,
            dark: 0,
        };
        ensure!(
            page.bits_per_pixel >= 8 && page.bits_per_pixel.is_multiple_of(8) && page.width > 0,
            "URF page {} header {page:?}",
            pages.len() + 1
        );
        pos += 32;
        let pixel = (page.bits_per_pixel / 8) as usize;
        let byte = |at: usize| -> Result<u8> {
            data.get(at).copied().context("URF page data ends mid-line")
        };
        // a pixel is dark when its (8-bit gray) value is < 128
        let is_dark = |at: usize| -> Result<bool> { Ok(pixel == 1 && byte(at)? < 128) };
        let mut y = 0u32;
        while y < page.height {
            let line_repeat = byte(pos)? as u32;
            pos += 1;
            let mut line_dark = 0u64;
            let mut x = 0u32;
            while x < page.width {
                let c = byte(pos)?;
                pos += 1;
                if c == 0x80 {
                    x = page.width; // rest of the line = white
                } else if c < 0x80 {
                    if is_dark(pos)? {
                        line_dark += c as u64 + 1;
                    }
                    pos += pixel;
                    x += c as u32 + 1;
                } else {
                    let n = 257 - c as u32;
                    for k in 0..n as usize {
                        if is_dark(pos + k * pixel)? {
                            line_dark += 1;
                        }
                    }
                    pos += n as usize * pixel;
                    x += n;
                }
            }
            ensure!(
                pos <= data.len(),
                "URF page {} data truncated",
                pages.len() + 1
            );
            page.dark += line_dark * (line_repeat as u64 + 1);
            y += line_repeat + 1;
        }
        pages.push(page);
    }
    Ok((count, pages))
}

/// The bar `direct_ipp::test_pdf` draws on a page `width_pt` wide:
/// (width - 40) x 30 pt.
pub fn bar_area_pt2(width_pt: u32) -> u64 {
    (width_pt as u64 - 40) * 30
}

/// Dark pixels a page should have when its only content, a bar of
/// `area_pt2`, is rendered at scale 1 and `dpi`.
pub fn expected_dark(area_pt2: u64, dpi: u32) -> u64 {
    area_pt2 * dpi as u64 * dpi as u64 / (72 * 72)
}

/// The #111 check of the URF the client sent for the P / L / A5 document
/// (`test_pdf(&[(595, 842), (842, 595), (420, 595)], ..)`).
pub fn check_portrait_pages(pages: &[UrfPage]) -> Result<()> {
    ensure!(pages.len() == 3, "expected 3 URF pages, got {pages:?}");
    for (i, p) in pages.iter().enumerate() {
        ensure!(
            p.width < p.height,
            "URF page {} is LANDSCAPE {}x{} — the HP M110w aborts it (#111): {pages:?}",
            i + 1,
            p.width,
            p.height
        );
        ensure!(
            p.bits_per_pixel == 8 && p.dpi == 600,
            "URF page {} is not urfgray 600 dpi: {p:?}",
            i + 1
        );
    }
    ensure!(
        (pages[1].width, pages[1].height) == (pages[0].width, pages[0].height),
        "the landscape A4 page must be rotated onto the A4 portrait size of page 1 (scale 1): {pages:?}"
    );
    ensure!(
        pages[2].width < pages[0].width && pages[2].height < pages[0].height,
        "the A5 page must stay A5 (not enlarged to A4): {pages:?}"
    );
    // Scale 1, not just the right size: the landscape page's bar keeps its
    // full area (clipped without -dPDFFitPage it loses ~30 %, shrunk to fit
    // about half), within 2 % for the edge pixels.
    for (i, width_pt) in [(1usize, 842u32), (2, 420)] {
        let want = expected_dark(bar_area_pt2(width_pt), pages[i].dpi);
        let got = pages[i].dark;
        ensure!(
            got.abs_diff(want) * 50 <= want,
            "URF page {} has {got} dark pixels, a bar at scale 1 has {want} — clipped or scaled (#111): {pages:?}",
            i + 1
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A URF page: header (bpp 8, `width` x `height`, 600 dpi) + `lines`.
    fn urf_page(width: u32, height: u32, lines: &[u8]) -> Vec<u8> {
        let mut p = vec![8u8, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        p.extend_from_slice(&width.to_be_bytes());
        p.extend_from_slice(&height.to_be_bytes());
        p.extend_from_slice(&600u32.to_be_bytes());
        p.extend_from_slice(&[0u8; 8]);
        p.extend_from_slice(lines);
        p
    }

    /// 4 x 3: two lines (repeat 1) of 2 repeated + 2 literal pixels, then one
    /// line filled white.
    const LINES_4X3: &[u8] = &[1, 0x01, 0xFF, 0xFF, 0x00, 0x7F, 0, 0x80];

    fn urf_doc(count: u32, pages: &[Vec<u8>]) -> Vec<u8> {
        let mut d = b"UNIRAST\0".to_vec();
        d.extend_from_slice(&count.to_be_bytes());
        for p in pages {
            d.extend_from_slice(p);
        }
        d
    }

    #[test]
    fn urf_pages_walks_every_page_header() {
        let doc = urf_doc(0, &[urf_page(4, 3, LINES_4X3), urf_page(4, 3, LINES_4X3)]);
        let (count, pages) = urf_pages(&doc).unwrap();
        assert_eq!(count, 0);
        // per page: 2 lines x 2 dark literal pixels (0x00, 0x7F); the 0xFF
        // run and the white fill are not dark
        assert_eq!(
            pages,
            vec![
                UrfPage {
                    bits_per_pixel: 8,
                    width: 4,
                    height: 3,
                    dpi: 600,
                    dark: 4
                };
                2
            ]
        );
    }

    #[test]
    fn urf_dark_pixels_count_repeated_runs_and_line_repeats() {
        // 3 x 4: one line group repeated 4 times: a run of 3 black pixels
        let doc = urf_doc(1, &[urf_page(3, 4, &[3, 0x02, 0x00])]);
        let (_, pages) = urf_pages(&doc).unwrap();
        assert_eq!(pages[0].dark, 12);
        // 0x80 = 128 is not dark, 0x7F is
        let doc = urf_doc(1, &[urf_page(2, 1, &[0, 0xFF, 0x80, 0x7F])]);
        assert_eq!(urf_pages(&doc).unwrap().1[0].dark, 1);
    }

    #[test]
    fn urf_pages_refuses_broken_documents() {
        assert!(urf_pages(b"PNG").is_err());
        assert!(urf_pages(b"UNIRAST\0\0\0").is_err());
        let truncated = urf_doc(1, &[urf_page(4, 3, &LINES_4X3[..4])]);
        assert!(urf_pages(&truncated).is_err());
        let short_header = urf_doc(1, &[vec![8u8; 20]]);
        assert!(urf_pages(&short_header).is_err());
    }

    fn page(width: u32, height: u32, dark: u64) -> UrfPage {
        UrfPage {
            bits_per_pixel: 8,
            width,
            height,
            dpi: 600,
            dark,
        }
    }

    /// The test PDF at 600 dpi on Windows gs 10.04 (CI run 39401975262): A4
    /// 4958 x 7017, A5 3500 x 4958; the bars at scale 1.
    const A4_BAR: u64 = 1_670_833; // 802 x 30 pt
    const A5_BAR: u64 = 791_666; // 380 x 30 pt

    #[test]
    fn expected_dark_is_the_bar_area_at_the_resolution() {
        assert_eq!(bar_area_pt2(842), 802 * 30);
        assert_eq!(bar_area_pt2(420), 380 * 30);
        assert_eq!(expected_dark(bar_area_pt2(842), 600), A4_BAR);
        assert_eq!(expected_dark(bar_area_pt2(420), 600), A5_BAR);
        assert_eq!(expected_dark(72 * 72, 300), 300 * 300);
    }

    #[test]
    fn portrait_check_accepts_the_fixed_render() {
        let p1 = page(4958, 7017, 1_300_000);
        assert!(
            check_portrait_pages(&[p1, page(4958, 7017, A4_BAR), page(3500, 4958, A5_BAR)]).is_ok()
        );
        // edge pixels: within 2 %
        assert!(
            check_portrait_pages(&[
                p1,
                page(4958, 7017, A4_BAR + A4_BAR / 50),
                page(3500, 4958, A5_BAR - A5_BAR / 50)
            ])
            .is_ok()
        );
    }

    #[test]
    fn portrait_check_refuses_what_the_hp_aborts_or_misprints() {
        let a4 = page(4958, 7017, A4_BAR);
        let a5 = page(3500, 4958, A5_BAR);
        // the pre-#111 render: page 2 landscape
        assert!(check_portrait_pages(&[a4, page(7017, 4958, A4_BAR), a5]).is_err());
        // A5 enlarged to A4
        assert!(check_portrait_pages(&[a4, a4, page(4958, 7017, A5_BAR)]).is_err());
        // landscape page shrunk instead of rotated at scale 1
        assert!(check_portrait_pages(&[a4, page(3508, 4958, A4_BAR / 2), a5]).is_err());
        // right size but clipped (-dFIXEDMEDIA without -dPDFFitPage): ~30 % lost
        assert!(check_portrait_pages(&[a4, page(4958, 7017, A4_BAR * 7 / 10), a5]).is_err());
        // right size but shrunk to fit: about half the bar
        assert!(check_portrait_pages(&[a4, page(4958, 7017, A4_BAR / 2), a5]).is_err());
        // just outside 2 %, either way
        assert!(check_portrait_pages(&[a4, page(4958, 7017, A4_BAR + A4_BAR / 40), a5]).is_err());
        assert!(check_portrait_pages(&[a4, a4, page(3500, 4958, A5_BAR - A5_BAR / 40)]).is_err());
        // pages missing / split into several jobs
        assert!(check_portrait_pages(&[a4, a4]).is_err());
        // not urfgray 600
        let mut low = a4;
        low.dpi = 300;
        assert!(check_portrait_pages(&[low, low, a5]).is_err());
        let mut rgb = a4;
        rgb.bits_per_pixel = 24;
        assert!(check_portrait_pages(&[rgb, rgb, a5]).is_err());
        // a square page is not portrait
        let square = page(5000, 5000, A4_BAR);
        assert!(check_portrait_pages(&[square, square, a5]).is_err());
    }
}
