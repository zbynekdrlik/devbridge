//! The pure decisions of a Ghostscript render (#111): which Ghostscript
//! calls a document needs, its page count, and what Ghostscript's output
//! says. Kept out of `ghostscript.rs` — child processes and files, excluded
//! from mutation testing — so the mutation gate covers every decision.

use crate::pdf_pages::{self, OrientationSummary, RenderRun};
use crate::raster_merge::RasterKind;

/// Count the "Page N" progress lines Ghostscript prints for each rendered
/// page.
pub fn parse_page_count(output: &str) -> u32 {
    output
        .lines()
        .filter(|line| line.starts_with("Page "))
        .count() as u32
}

/// Rendered pages reported by one Ghostscript call. The "Page N" lines go
/// to STDOUT (before #111 only stderr was read, so every multi-page raster
/// was reported as "1 pages").
pub fn page_lines(stdout: &str, stderr: &str) -> u32 {
    parse_page_count(stdout) + parse_page_count(stderr)
}

/// The last non-empty line Ghostscript printed — stderr (its error channel)
/// first, then stdout.
pub fn last_message<'a>(stderr: &'a str, stdout: &'a str) -> &'a str {
    [stderr, stdout]
        .into_iter()
        .find_map(|s| s.lines().rev().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("unknown error")
}

/// The devices that write one image file per page (`page-001.jpg`, …): one
/// IPP Print-Job per page, never orientation-normalised (#111 scope).
pub fn is_single_page_device(device: &str) -> bool {
    matches!(
        device,
        "jpeg" | "jpeggray" | "jpegcmyk" | "png16m" | "pnggray" | "pngmono" | "pngalpha"
    )
}

/// File extension of a per-page image device.
pub fn page_file_ext(device: &str) -> &'static str {
    match device {
        "jpeg" | "jpeggray" | "jpegcmyk" => "jpg",
        _ => "png",
    }
}

/// Page orientation of one render.
#[derive(Debug, Clone, PartialEq)]
pub enum Orientation {
    /// The device is out of the #111 scope: the plain pre-#111 render.
    NotApplicable,
    /// `-dPDFINFO` failed or was unreadable: rendered as-is (the pre-#111
    /// behaviour); the reason is logged and shown in the `rendered` event.
    Unknown(String),
    /// Pages read; `runs` = the Ghostscript runs, merged as `kind`.
    Planned {
        kind: RasterKind,
        runs: Vec<RenderRun>,
        summary: OrientationSummary,
    },
}

/// The Ghostscript render calls of one document.
#[derive(Debug, PartialEq)]
pub enum GsCalls<'a> {
    /// ONE call with these extra arguments (none = the pre-#111 call).
    Single(Vec<String>),
    /// One call per run, merged into one `RasterKind` document.
    Runs(RasterKind, &'a [RenderRun]),
}

impl Orientation {
    /// From the finished `gs -dPDFINFO` call (`success` / `exit_code` of the
    /// process, what it printed).
    pub fn from_pdfinfo(
        kind: RasterKind,
        success: bool,
        exit_code: Option<i32>,
        stdout: &str,
        stderr: &str,
    ) -> Self {
        if !success {
            return Orientation::Unknown(format!(
                "gs -dPDFINFO exit code {}: {}",
                exit_code.unwrap_or(-1),
                last_message(stderr, stdout)
            ));
        }
        match pdf_pages::parse_pdfinfo(&format!("{stdout}\n{stderr}")) {
            Ok(pages) => Orientation::Planned {
                kind,
                runs: pdf_pages::plan_runs(&pages),
                summary: pdf_pages::summarize(&pages),
            },
            Err(e) => Orientation::Unknown(format!("{e:#}")),
        }
    }

    /// The Ghostscript calls this document needs.
    pub fn calls(&self) -> GsCalls<'_> {
        match self {
            Orientation::Planned { kind, runs, .. } if runs.len() > 1 => GsCalls::Runs(*kind, runs),
            Orientation::Planned { runs, .. } => GsCalls::Single(
                runs.first()
                    .map(|run| pdf_pages::run_args(run, true))
                    .unwrap_or_default(),
            ),
            _ => GsCalls::Single(Vec::new()),
        }
    }

    /// Pages of the document: the PDFINFO page list when there is one, else
    /// Ghostscript's "Page N" lines, else the number of output files.
    pub fn page_count(&self, page_lines: u32, page_files: usize) -> u32 {
        match self {
            Orientation::Planned { summary, .. } => summary.portrait + summary.landscape,
            _ if page_lines > 0 => page_lines,
            _ => page_files as u32,
        }
    }

    /// Appended to the `rendered` event detail (empty out of scope).
    pub fn event_suffix(&self) -> String {
        match self {
            Orientation::NotApplicable => String::new(),
            Orientation::Unknown(reason) => format!(
                ", orientation: unknown ({}), rendered as-is",
                reason.chars().take(160).collect::<String>()
            ),
            Orientation::Planned { summary, .. } => format!(", {summary}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf_pages::RunMedia;

    #[test]
    fn test_parse_page_count_multiple() {
        let stdout = "GPL Ghostscript 10.04.0 (2024-09-18)\n\
                      Processing pages 1 through 3.\n\
                      Page 1\n\
                      Loading font Helvetica\n\
                      Page 2\n\
                      Page 3\n";
        assert_eq!(parse_page_count(stdout), 3);
    }

    #[test]
    fn test_parse_page_count_single_and_none() {
        assert_eq!(parse_page_count("Page 1\n"), 1);
        assert_eq!(parse_page_count(""), 0);
        assert_eq!(parse_page_count("some error output\n"), 0);
    }

    #[test]
    fn test_page_lines_counts_both_streams() {
        assert_eq!(page_lines("Page 1\nPage 2\n", ""), 2);
        assert_eq!(page_lines("", "Page 1\n"), 1);
        assert_eq!(page_lines("Page 1\nPage 2\n", "Page 3\n"), 3);
    }

    #[test]
    fn test_last_message_prefers_stderr_then_stdout() {
        assert_eq!(
            last_message("**** Error: bad\nlast error line\n\n", "Page 1\n"),
            "last error line"
        );
        assert_eq!(
            last_message("", "Error: /undefinedfilename\n"),
            "Error: /undefinedfilename"
        );
        assert_eq!(last_message(" \n", ""), "unknown error");
    }

    #[test]
    fn test_single_page_devices() {
        for device in [
            "jpeg", "jpeggray", "jpegcmyk", "png16m", "pnggray", "pngmono", "pngalpha",
        ] {
            assert!(is_single_page_device(device), "{device}");
        }
        for device in ["urfgray", "pwgraster", "pclm", "pdfwrite", ""] {
            assert!(!is_single_page_device(device), "{device}");
        }
        assert_eq!(page_file_ext("jpeg"), "jpg");
        assert_eq!(page_file_ext("jpeggray"), "jpg");
        assert_eq!(page_file_ext("jpegcmyk"), "jpg");
        assert_eq!(page_file_ext("png16m"), "png");
        assert_eq!(page_file_ext("pnggray"), "png");
    }

    const MIXED_PDFINFO: &str = "        File has 3 pages\n\
        Page 1 MediaBox: [0 0 595 842]\nPage 2 MediaBox: [0 0 842 595]\nPage 3 MediaBox: [0 0 420 595]\n";

    fn summary(portrait: u32, landscape: u32) -> OrientationSummary {
        OrientationSummary {
            portrait,
            landscape,
        }
    }

    #[test]
    fn from_pdfinfo_plans_the_runs() {
        // PDFINFO prints on stderr
        let o = Orientation::from_pdfinfo(RasterKind::Urf, true, Some(0), "", MIXED_PDFINFO);
        let Orientation::Planned {
            kind,
            runs,
            summary: s,
        } = &o
        else {
            panic!("planned expected: {o:?}");
        };
        assert_eq!(*kind, RasterKind::Urf);
        assert_eq!(runs.len(), 3);
        assert_eq!(*s, summary(2, 1));
        // ... or on stdout
        let o = Orientation::from_pdfinfo(RasterKind::Pwg, true, Some(0), MIXED_PDFINFO, "");
        assert!(
            matches!(
                o,
                Orientation::Planned {
                    kind: RasterKind::Pwg,
                    ..
                }
            ),
            "{o:?}"
        );
    }

    #[test]
    fn from_pdfinfo_failure_is_unknown_with_the_reason() {
        let o =
            Orientation::from_pdfinfo(RasterKind::Urf, false, Some(1), "", "Error: /syntaxerror\n");
        assert_eq!(
            o,
            Orientation::Unknown("gs -dPDFINFO exit code 1: Error: /syntaxerror".into())
        );
        let o = Orientation::from_pdfinfo(RasterKind::Urf, false, None, MIXED_PDFINFO, "");
        assert!(
            matches!(&o, Orientation::Unknown(r) if r.starts_with("gs -dPDFINFO exit code -1: ")),
            "{o:?}"
        );
        let o = Orientation::from_pdfinfo(RasterKind::Urf, true, Some(0), "GS>", "");
        assert!(
            matches!(&o, Orientation::Unknown(r) if r.contains("no page")),
            "{o:?}"
        );
    }

    fn run(first: u32, last: u32, media: RunMedia) -> RenderRun {
        RenderRun {
            first_page: first,
            last_page: last,
            media,
        }
    }

    #[test]
    fn calls_one_plain_call_for_a_portrait_document() {
        let o = Orientation::Planned {
            kind: RasterKind::Urf,
            runs: vec![run(1, 3, RunMedia::AsIs)],
            summary: summary(3, 0),
        };
        assert_eq!(o.calls(), GsCalls::Single(Vec::new()));
        assert_eq!(
            Orientation::NotApplicable.calls(),
            GsCalls::Single(Vec::new())
        );
        assert_eq!(
            Orientation::Unknown("x".into()).calls(),
            GsCalls::Single(Vec::new())
        );
        let empty = Orientation::Planned {
            kind: RasterKind::Urf,
            runs: Vec::new(),
            summary: summary(0, 0),
        };
        assert_eq!(empty.calls(), GsCalls::Single(Vec::new()));
    }

    #[test]
    fn calls_one_media_call_for_a_landscape_only_document() {
        let a4 = RunMedia::Portrait {
            width: 595.0,
            height: 842.0,
        };
        let o = Orientation::Planned {
            kind: RasterKind::Urf,
            runs: vec![run(1, 2, a4)],
            summary: summary(0, 2),
        };
        assert_eq!(
            o.calls(),
            GsCalls::Single(vec![
                "-dDEVICEWIDTHPOINTS=595".to_string(),
                "-dDEVICEHEIGHTPOINTS=842".to_string(),
                "-dFIXEDMEDIA".to_string(),
                "-dPDFFitPage".to_string(),
            ])
        );
    }

    #[test]
    fn calls_several_runs_for_a_mixed_document() {
        let runs = vec![
            run(1, 1, RunMedia::AsIs),
            run(
                2,
                2,
                RunMedia::Portrait {
                    width: 595.0,
                    height: 842.0,
                },
            ),
        ];
        let o = Orientation::Planned {
            kind: RasterKind::Pwg,
            runs: runs.clone(),
            summary: summary(1, 1),
        };
        assert_eq!(o.calls(), GsCalls::Runs(RasterKind::Pwg, &runs));
    }

    #[test]
    fn page_count_prefers_pdfinfo_then_page_lines_then_files() {
        let planned = Orientation::Planned {
            kind: RasterKind::Urf,
            runs: Vec::new(),
            summary: summary(2, 1),
        };
        assert_eq!(planned.page_count(7, 1), 3);
        assert_eq!(Orientation::NotApplicable.page_count(4, 1), 4);
        assert_eq!(Orientation::NotApplicable.page_count(0, 5), 5);
        assert_eq!(Orientation::Unknown("x".into()).page_count(0, 2), 2);
        assert_eq!(Orientation::Unknown("x".into()).page_count(1, 2), 1);
    }

    #[test]
    fn event_suffix_names_the_orientation() {
        assert_eq!(Orientation::NotApplicable.event_suffix(), "");
        let planned = Orientation::Planned {
            kind: RasterKind::Urf,
            runs: Vec::new(),
            summary: summary(2, 1),
        };
        assert_eq!(
            planned.event_suffix(),
            ", orientation: 2 portrait, 1 landscape (rotated 90°)"
        );
        let unknown = Orientation::Unknown("x".repeat(500)).event_suffix();
        assert_eq!(
            unknown,
            format!(
                ", orientation: unknown ({}), rendered as-is",
                "x".repeat(160)
            )
        );
    }
}
