//! Page geometry of a PDF as Ghostscript reads it, and the render runs that
//! put every page on portrait-fed paper at scale 1 (#111).
//!
//! Every printer DevBridge drives feeds paper portrait. A landscape page
//! rendered at its own size becomes a raster wider than the paper (A4
//! landscape → 7016 × 4961 at 600 dpi); the HP LaserJet M110w aborts such a
//! job (`document-format-error`). A printer driver would rotate the page by
//! 90°; [`plan_runs`] does the same: a landscape page is rendered onto a
//! portrait medium of exactly its own size swapped (`-dFIXEDMEDIA
//! -dPDFFitPage`), so Ghostscript rotates it at scale 1. Portrait pages keep
//! the plain render, byte for byte.
//!
//! The geometry comes from the SAME Ghostscript (`-dPDFINFO`) — no second PDF
//! parser that could disagree with what Ghostscript renders. Its output
//! (identical on gs 10.02.1 and the stores' 10.04.0, on stderr):
//!
//! ```text
//!         File has 3 pages
//! Page 1 MediaBox: [0 0 595 842]
//! Page 2 MediaBox: [0.000000 0.000000 841.920044 595.320007] CropBox: [...]    Rotate = 0
//! Page 3 MediaBox: [0 0 595 842]    Rotate = 90
//! ```
//!
//! Ghostscript renders the MediaBox, turned by `/Rotate` (printed only when
//! the PDF has the key).

use anyhow::{Context, Result, bail, ensure};

/// Two landscape pages share one fixed medium only when their sizes differ
/// by less than this (points); a page of another size gets its own run, so
/// `-dPDFFitPage` never scales it.
const SAME_SIZE_TOLERANCE_PT: f64 = 0.5;

/// One page as `gs -dPDFINFO` reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageGeometry {
    /// 1-based page number.
    pub number: u32,
    /// MediaBox width in points.
    pub media_width: f64,
    /// MediaBox height in points.
    pub media_height: f64,
    /// `/Rotate`, normalised to 0, 90, 180 or 270.
    pub rotate: u32,
}

impl PageGeometry {
    /// Width × height of the page as Ghostscript renders it (after `/Rotate`).
    pub fn rendered_size(&self) -> (f64, f64) {
        if self.rotate % 180 == 90 {
            (self.media_height, self.media_width)
        } else {
            (self.media_width, self.media_height)
        }
    }

    /// Wider than tall as rendered (a square page counts as portrait).
    pub fn is_landscape(&self) -> bool {
        let (width, height) = self.rendered_size();
        width > height
    }
}

/// The paper a render run draws on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunMedia {
    /// Each page at its own size — the plain pre-#111 render.
    AsIs,
    /// Landscape pages on a fixed portrait medium of this size (points):
    /// the page's own size swapped, so the 90° fit is at scale 1.
    Portrait { width: f64, height: f64 },
}

impl RunMedia {
    fn for_page(page: &PageGeometry) -> Self {
        if page.is_landscape() {
            let (width, height) = page.rendered_size();
            RunMedia::Portrait {
                width: height,
                height: width,
            }
        } else {
            RunMedia::AsIs
        }
    }

    fn shares_run_with(&self, other: &RunMedia) -> bool {
        match (self, other) {
            (RunMedia::AsIs, RunMedia::AsIs) => true,
            (
                RunMedia::Portrait { width, height },
                RunMedia::Portrait {
                    width: other_width,
                    height: other_height,
                },
            ) => {
                (width - other_width).abs() < SAME_SIZE_TOLERANCE_PT
                    && (height - other_height).abs() < SAME_SIZE_TOLERANCE_PT
            }
            _ => false,
        }
    }
}

/// Consecutive pages rendered by ONE Ghostscript call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderRun {
    pub first_page: u32,
    pub last_page: u32,
    pub media: RunMedia,
}

/// Portrait / landscape page counts of a document, for the log and the
/// `rendered` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrientationSummary {
    pub portrait: u32,
    pub landscape: u32,
}

impl std::fmt::Display for OrientationSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "orientation: {} portrait, {} landscape",
            self.portrait, self.landscape
        )?;
        if self.landscape > 0 {
            write!(f, " (rotated 90°)")?;
        }
        Ok(())
    }
}

/// Parse `gs -dPDFINFO` output (stdout + stderr) into the page list.
///
/// Errors when no page line is found, a page line is malformed, pages are
/// not numbered 1..n, or the `File has N pages` line disagrees — the caller
/// then renders the document as-is (the pre-#111 behaviour).
pub fn parse_pdfinfo(output: &str) -> Result<Vec<PageGeometry>> {
    let mut pages: Vec<PageGeometry> = Vec::new();
    let mut declared: Option<u32> = None;
    for line in output.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("File has ") {
            let count = rest.split_whitespace().next().unwrap_or("");
            declared = Some(
                count
                    .parse()
                    .with_context(|| format!("PDFINFO page count in {line:?}"))?,
            );
            continue;
        }
        let Some(rest) = line.strip_prefix("Page ") else {
            continue;
        };
        let Some((number, after)) = rest.split_once(" MediaBox: [") else {
            continue;
        };
        let page = parse_page_line(number, after).with_context(|| format!("PDFINFO {line:?}"))?;
        let expected = pages.len() as u32 + 1;
        ensure!(
            page.number == expected,
            "PDFINFO lists page {} where page {expected} was expected",
            page.number
        );
        pages.push(page);
    }
    ensure!(!pages.is_empty(), "PDFINFO output lists no page");
    if let Some(count) = declared {
        ensure!(
            count == pages.len() as u32,
            "PDFINFO says the file has {count} pages but lists {}",
            pages.len()
        );
    }
    Ok(pages)
}

/// `number` = the text between `Page ` and ` MediaBox: [`; `after` = the rest
/// of the line after the `[`.
fn parse_page_line(number: &str, after: &str) -> Result<PageGeometry> {
    let number: u32 = number.trim().parse().context("page number")?;
    let (media_box, tail) = after.split_once(']').context("unterminated MediaBox")?;
    let values = media_box
        .split_whitespace()
        .map(|v| {
            v.parse::<f64>()
                .with_context(|| format!("MediaBox value {v:?}"))
        })
        .collect::<Result<Vec<f64>>>()?;
    ensure!(
        values.len() == 4,
        "MediaBox has {} values, expected 4",
        values.len()
    );
    let media_width = (values[2] - values[0]).abs();
    let media_height = (values[3] - values[1]).abs();
    ensure!(
        media_width.is_finite()
            && media_height.is_finite()
            && media_width > 0.0
            && media_height > 0.0,
        "MediaBox has no area: {media_width} x {media_height}"
    );
    Ok(PageGeometry {
        number,
        media_width,
        media_height,
        rotate: parse_rotate(tail)?,
    })
}

/// `Rotate = R` after the MediaBox (and an optional CropBox); absent = 0.
fn parse_rotate(tail: &str) -> Result<u32> {
    let Some((_, value)) = tail.split_once("Rotate =") else {
        return Ok(0);
    };
    let value = value.split_whitespace().next().unwrap_or("");
    let degrees: i64 = value
        .parse()
        .with_context(|| format!("Rotate value {value:?}"))?;
    if degrees % 90 != 0 {
        bail!("Rotate {degrees} is not a multiple of 90");
    }
    Ok(degrees.rem_euclid(360) as u32)
}

/// Group pages into Ghostscript runs: consecutive portrait pages share one
/// plain run; consecutive landscape pages share a run when they have the
/// same size (the fixed medium is THAT size swapped).
pub fn plan_runs(pages: &[PageGeometry]) -> Vec<RenderRun> {
    let mut runs: Vec<RenderRun> = Vec::new();
    for page in pages {
        let media = RunMedia::for_page(page);
        match runs.last_mut() {
            Some(run) if run.media.shares_run_with(&media) => run.last_page = page.number,
            _ => runs.push(RenderRun {
                first_page: page.number,
                last_page: page.number,
                media,
            }),
        }
    }
    runs
}

/// The extra Ghostscript arguments of `run`. A document rendered in ONE run
/// needs no page range, so a portrait-only document gets NO extra argument —
/// exactly the pre-#111 call, byte-identical output.
pub fn run_args(run: &RenderRun, single_run: bool) -> Vec<String> {
    let mut args = Vec::new();
    if !single_run {
        args.push(format!("-dFirstPage={}", run.first_page));
        args.push(format!("-dLastPage={}", run.last_page));
    }
    if let RunMedia::Portrait { width, height } = run.media {
        args.push(format!("-dDEVICEWIDTHPOINTS={width}"));
        args.push(format!("-dDEVICEHEIGHTPOINTS={height}"));
        args.push("-dFIXEDMEDIA".to_string());
        args.push("-dPDFFitPage".to_string());
    }
    args
}

/// Portrait / landscape page counts.
pub fn summarize(pages: &[PageGeometry]) -> OrientationSummary {
    let landscape = pages.iter().filter(|p| p.is_landscape()).count() as u32;
    OrientationSummary {
        portrait: pages.len() as u32 - landscape,
        landscape,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(number: u32, w: f64, h: f64, rotate: u32) -> PageGeometry {
        PageGeometry {
            number,
            media_width: w,
            media_height: h,
            rotate,
        }
    }

    /// Verbatim `gs -dPDFINFO` output of the real failing pjkes payload
    /// (job 818b395d, Microsoft Print to PDF, A4 landscape), fonts trimmed.
    const PJKES_LANDSCAPE: &str = "\n        File has 1 page.\n\nTitle: Microsoft Word - Dokument1\n\
        Producer: Microsoft: Print To PDF\n\nPage 1 MediaBox: [0.000000 0.000000 841.920044 595.320007] \
        CropBox: [0.000000 0.000000 841.920044 595.320007]    Rotate = 0 \n    Fonts used: \n";

    /// gs 10.04.0 (Windows) / 10.02.1 output for a P / L / A5 / Rotate-90 file.
    const MIXED: &str = "\n        File has 4 pages\n\n\nPage 1 MediaBox: [0 0 595 842]\n    Fonts used: \n\
        Page 2 MediaBox: [0 0 842 595]\n    Fonts used: \n\nPage 3 MediaBox: [0 0 420 595]\n\
        Page 4 MediaBox: [0 0 595 842]    Rotate = 90 \n";

    #[test]
    fn parses_the_real_microsoft_print_to_pdf_landscape_page() {
        let pages = parse_pdfinfo(PJKES_LANDSCAPE).unwrap();
        assert_eq!(pages.len(), 1);
        let p = pages[0];
        assert_eq!(p.number, 1);
        assert!((p.media_width - 841.920044).abs() < 1e-9, "{p:?}");
        assert!((p.media_height - 595.320007).abs() < 1e-9, "{p:?}");
        assert_eq!(p.rotate, 0);
        assert!(p.is_landscape());
    }

    #[test]
    fn parses_every_page_with_its_rotate() {
        let pages = parse_pdfinfo(MIXED).unwrap();
        assert_eq!(
            pages,
            vec![
                geo(1, 595.0, 842.0, 0),
                geo(2, 842.0, 595.0, 0),
                geo(3, 420.0, 595.0, 0),
                geo(4, 595.0, 842.0, 90),
            ]
        );
    }

    #[test]
    fn media_box_with_an_offset_origin_uses_its_extent() {
        let pages = parse_pdfinfo("Page 1 MediaBox: [10 20 605 862]\n").unwrap();
        assert_eq!(pages, vec![geo(1, 595.0, 842.0, 0)]);
        let pages = parse_pdfinfo("Page 1 MediaBox: [605 862 10 20]\n").unwrap();
        assert_eq!(pages, vec![geo(1, 595.0, 842.0, 0)]);
    }

    #[test]
    fn rotate_is_normalised() {
        let parse = |r: &str| {
            parse_pdfinfo(&format!(
                "Page 1 MediaBox: [0 0 595 842]    Rotate = {r} \n"
            ))
            .map(|p| p[0].rotate)
        };
        assert_eq!(parse("0").unwrap(), 0);
        assert_eq!(parse("90").unwrap(), 90);
        assert_eq!(parse("180").unwrap(), 180);
        assert_eq!(parse("270").unwrap(), 270);
        assert_eq!(parse("-90").unwrap(), 270);
        assert_eq!(parse("450").unwrap(), 90);
        assert_eq!(parse("360").unwrap(), 0);
        assert!(parse("45").is_err());
        assert!(parse("x").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn progress_lines_and_other_text_are_not_pages() {
        // "Page N" without a MediaBox is a render progress line.
        let out = "Processing pages 1 through 1.\nPage 1\nTitle: Page 9 MediaBox: [0 0 1 1]\n\
                   Page 1 MediaBox: [0 0 595 842]\n";
        assert_eq!(parse_pdfinfo(out).unwrap(), vec![geo(1, 595.0, 842.0, 0)]);
    }

    #[test]
    fn malformed_output_is_an_error() {
        for bad in [
            "",
            "GS>",
            "Page 1 MediaBox: [0 0 595]\n",
            "Page 1 MediaBox: [0 0 595 842 1]\n",
            "Page 1 MediaBox: [0 0 595 x]\n",
            "Page 1 MediaBox: [0 0 595 842\n",
            "Page x MediaBox: [0 0 595 842]\n",
            "Page 1 MediaBox: [0 0 0 842]\n",
            "Page 1 MediaBox: [0 0 595 0]\n",
            "Page 1 MediaBox: [0 0 inf 842]\n",
            "Page 1 MediaBox: [0 0 595 NaN]\n",
            "Page 2 MediaBox: [0 0 595 842]\n",
            "Page 1 MediaBox: [0 0 595 842]\nPage 1 MediaBox: [0 0 595 842]\n",
            "Page 1 MediaBox: [0 0 595 842]\nPage 3 MediaBox: [0 0 595 842]\n",
            "File has 2 pages\nPage 1 MediaBox: [0 0 595 842]\n",
            "File has many pages\nPage 1 MediaBox: [0 0 595 842]\n",
        ] {
            assert!(parse_pdfinfo(bad).is_err(), "must be refused: {bad:?}");
        }
    }

    #[test]
    fn declared_count_that_matches_is_accepted() {
        let out = "File has 2 pages\nPage 1 MediaBox: [0 0 1 2]\nPage 2 MediaBox: [0 0 1 2]\n";
        assert_eq!(parse_pdfinfo(out).unwrap().len(), 2);
    }

    #[test]
    fn orientation_follows_the_rendered_size() {
        assert!(!geo(1, 595.0, 842.0, 0).is_landscape());
        assert!(geo(1, 842.0, 595.0, 0).is_landscape());
        assert!(geo(1, 595.0, 842.0, 90).is_landscape());
        assert!(geo(1, 595.0, 842.0, 270).is_landscape());
        assert!(!geo(1, 595.0, 842.0, 180).is_landscape());
        assert!(!geo(1, 842.0, 595.0, 90).is_landscape());
        assert!(geo(1, 842.0, 595.0, 180).is_landscape());
        // square = portrait (fits the feed as is)
        assert!(!geo(1, 500.0, 500.0, 0).is_landscape());
        assert_eq!(geo(1, 595.0, 842.0, 90).rendered_size(), (842.0, 595.0));
        assert_eq!(geo(1, 595.0, 842.0, 0).rendered_size(), (595.0, 842.0));
    }

    #[test]
    fn landscape_page_gets_its_own_size_swapped() {
        let runs = plan_runs(&[geo(1, 841.920044, 595.320007, 0)]);
        assert_eq!(
            runs,
            vec![RenderRun {
                first_page: 1,
                last_page: 1,
                media: RunMedia::Portrait {
                    width: 595.320007,
                    height: 841.920044
                },
            }]
        );
        // MediaBox portrait turned by /Rotate 90: medium = the MediaBox itself.
        let runs = plan_runs(&[geo(1, 595.0, 842.0, 90)]);
        assert_eq!(
            runs[0].media,
            RunMedia::Portrait {
                width: 595.0,
                height: 842.0
            }
        );
    }

    #[test]
    fn consecutive_pages_of_one_kind_share_a_run() {
        // P P L L P -> 3 runs
        let pages = [
            geo(1, 595.0, 842.0, 0),
            geo(2, 420.0, 595.0, 0),
            geo(3, 842.0, 595.0, 0),
            geo(4, 842.0, 595.0, 0),
            geo(5, 595.0, 842.0, 0),
        ];
        let a4 = RunMedia::Portrait {
            width: 595.0,
            height: 842.0,
        };
        assert_eq!(
            plan_runs(&pages),
            vec![
                RenderRun {
                    first_page: 1,
                    last_page: 2,
                    media: RunMedia::AsIs
                },
                RenderRun {
                    first_page: 3,
                    last_page: 4,
                    media: a4
                },
                RenderRun {
                    first_page: 5,
                    last_page: 5,
                    media: RunMedia::AsIs
                },
            ]
        );
    }

    #[test]
    fn landscape_pages_of_another_size_get_their_own_run() {
        let pages = [
            geo(1, 842.0, 595.0, 0),
            geo(2, 842.4, 595.4, 0), // < 0.5 pt apart: same medium
            geo(3, 1191.0, 842.0, 0),
            geo(4, 842.0, 595.0, 0),
            geo(5, 842.0, 595.6, 0), // 0.6 pt taller: own run
            geo(6, 842.6, 595.6, 0), // 0.6 pt wider than page 5: own run
        ];
        let runs = plan_runs(&pages);
        let ranges: Vec<(u32, u32)> = runs.iter().map(|r| (r.first_page, r.last_page)).collect();
        assert_eq!(ranges, vec![(1, 2), (3, 3), (4, 4), (5, 5), (6, 6)]);
        assert_eq!(
            runs[1].media,
            RunMedia::Portrait {
                width: 842.0,
                height: 1191.0
            }
        );
    }

    #[test]
    fn exactly_half_a_point_apart_is_another_size() {
        let runs = plan_runs(&[geo(1, 842.0, 595.0, 0), geo(2, 842.5, 595.0, 0)]);
        assert_eq!(runs.len(), 2);
        let runs = plan_runs(&[geo(1, 842.0, 595.0, 0), geo(2, 842.0, 595.5, 0)]);
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn portrait_pages_of_any_size_share_one_plain_run() {
        let runs = plan_runs(&[geo(1, 595.0, 842.0, 0), geo(2, 420.0, 595.0, 0)]);
        assert_eq!(
            runs,
            vec![RenderRun {
                first_page: 1,
                last_page: 2,
                media: RunMedia::AsIs
            }]
        );
        assert!(plan_runs(&[]).is_empty());
    }

    #[test]
    fn single_portrait_run_adds_no_argument() {
        let run = RenderRun {
            first_page: 1,
            last_page: 7,
            media: RunMedia::AsIs,
        };
        assert!(run_args(&run, true).is_empty());
        assert_eq!(run_args(&run, false), vec!["-dFirstPage=1", "-dLastPage=7"]);
    }

    #[test]
    fn landscape_run_gets_the_fixed_portrait_medium() {
        let run = RenderRun {
            first_page: 2,
            last_page: 3,
            media: RunMedia::Portrait {
                width: 595.320007,
                height: 841.920044,
            },
        };
        assert_eq!(
            run_args(&run, false),
            vec![
                "-dFirstPage=2",
                "-dLastPage=3",
                "-dDEVICEWIDTHPOINTS=595.320007",
                "-dDEVICEHEIGHTPOINTS=841.920044",
                "-dFIXEDMEDIA",
                "-dPDFFitPage",
            ]
        );
        assert_eq!(
            run_args(&run, true),
            vec![
                "-dDEVICEWIDTHPOINTS=595.320007",
                "-dDEVICEHEIGHTPOINTS=841.920044",
                "-dFIXEDMEDIA",
                "-dPDFFitPage",
            ]
        );
    }

    #[test]
    fn summary_counts_and_reads_well() {
        let s = summarize(&[
            geo(1, 595.0, 842.0, 0),
            geo(2, 842.0, 595.0, 0),
            geo(3, 420.0, 595.0, 0),
        ]);
        assert_eq!(
            s,
            OrientationSummary {
                portrait: 2,
                landscape: 1
            }
        );
        assert_eq!(
            s.to_string(),
            "orientation: 2 portrait, 1 landscape (rotated 90°)"
        );
        assert_eq!(
            summarize(&[]),
            OrientationSummary {
                portrait: 0,
                landscape: 0
            }
        );
        // nothing was rotated: no "(rotated 90°)"
        let portrait_only = summarize(&[geo(1, 595.0, 842.0, 0), geo(2, 420.0, 595.0, 0)]);
        assert_eq!(
            portrait_only.to_string(),
            "orientation: 2 portrait, 0 landscape"
        );
        let landscape_only = summarize(&[geo(1, 842.0, 595.0, 0)]);
        assert_eq!(
            landscape_only.to_string(),
            "orientation: 0 portrait, 1 landscape (rotated 90°)"
        );
    }
}
