---
paths:
  - "crates/devbridge-client/src/ghostscript.rs"
  - "crates/devbridge-client/src/gs_plan.rs"
  - "crates/devbridge-client/src/pdf_pages.rs"
  - "crates/devbridge-client/src/raster_merge.rs"
  - "crates/devbridge-client/src/backend_direct_ipp.rs"
  - "crates/devbridge-client/tests/ghostscript_orientation.rs"
  - "crates/devbridge-e2e/src/direct_ipp.rs"
---

# direct_ipp render + send (#111, #112) — invariants and gotchas

- **Every printer feeds paper PORTRAIT.** A PDF page rendered at its own size makes an A4 landscape page a 297 mm wide raster (7016 × 4961 @ 600 dpi). The HP LaserJet M110w (`urfgray`) accepts that Print-Job and aborts it ~0.3 s later: `job-state=8 … document-format-error` (57 store jobs, pjzav/pjkes/pjpop, #111). A driver would rotate the page; `ghostscript::render` now does it.
- **How (`pdf_pages.rs`, `raster_merge.rs`, `gs_plan.rs`), scoped by DEVICE — `urfgray` / `urfrgb` / `urfcmyk` / `pwgraster` — so `ghostscript::render` does it for direct_ipp AND for `backend_direct_raw` should one ever be set to these devices:**
  - Page geometry from the SAME Ghostscript: `gs -dNODISPLAY -dNOPAUSE -dBATCH -dSAFER -q -dPDFINFO x.pdf`. Output on STDERR, identical on gs 10.02.1 (Linux CI) and 10.04.0 (stores): `Page N MediaBox: [x0 y0 x1 y1]`, optional `CropBox: [...]`, `Rotate = R` only when the PDF has `/Rotate`. Without `-dBATCH` it waits at a `GS>` prompt.
  - Rendered size = MediaBox turned by `/Rotate` 90/270. Square = portrait.
  - Landscape page → `-dDEVICEWIDTHPOINTS=<its height> -dDEVICEHEIGHTPOINTS=<its width> -dFIXEDMEDIA -dPDFFitPage`: rotated 90° at scale 1. NEVER a fixed A4 medium for every page: `-dPDFFitPage` also ENLARGES (the stores' A5 „šablona ramček zľavy“ would print at 141 %).
  - Runs: consecutive portrait pages = one plain run; consecutive landscape pages share a run only at the same size (< 0.5 pt apart). One run = no `-dFirstPage/-dLastPage`.
  - A portrait-only document = EXACTLY the pre-#111 call (byte-identical; `portrait_pages_stay_byte_identical_to_the_plain_render` guards it).
  - Several runs are merged into ONE document (one IPP Print-Job, copies/collation stay the printer's). URF = `UNIRAST\0` + u32 BE page count + page records; **gs 10.04 `urfgray` writes the count as 0** even for 3 pages, so a 0 in any input keeps the merged count 0. PWG = `RaS2` once + page records.
  - `-dPDFINFO` failing / unreadable (e.g. a non-PDF payload) → WARN + plain render (never worse than before); only a cancellation propagates.
  - ONE `rendering` + ONE `rendered` event per job whatever the run count (E2E #59 counts cycles by stages); `rendered` carries `orientation: N portrait, M landscape` (+ ` (rotated 90°)` when M > 0).
  - Every other device (jpeg/png per-page files, pclm, pdfwrite, pcl, …) keeps the single plain call.
- **Ghostscript prints `Page N` on STDOUT**, not stderr. Before #111 `parse_page_count(stderr)` was always 0, so every multi-page raster reported `1 pages`.
- **Windows gs 10.04.0 has NO `pwgraster`** (`Unknown device`); Linux gs has `pwgraster` and a cups `urf` device (1 bit/pixel, not the stores' format) but no `urfgray`. So the Linux tests (`tests/ghostscript_orientation.rs`, real gs installed by the Test job and the mutation shards) render through `pwgraster` (page header: width @372, height @376, bits/pixel @388, bytes/line @392, 1796 bytes), and `urfgray` is proven on Windows by E2E step 38.
- **Probing the stores' Ghostscript without a store:** pz-server has the same bundled `C:\Program Files\DevBridge\ghostscript\bin\gswin64c.exe`. Upload a synthetic PDF base64 into `C:\Windows\Temp\<dir>` via `mcpx.py win-pz-server`, run gs there, read the result base64, delete the dir. Never test-print at a store.
- **A send / poll failure is ONE `Failed` event with its cause (#112).** `fail_with_cause` in `backend_direct_ipp.rs` wraps the Print-Job POST, the Get-Job-Attributes POST and both response parses: event + WARN `IPP Print-Job send failed (<printer>): <{e:#}>`. reqwest's top-level message (`error sending request for url (…)`) names no cause; the `source()` chain does (`… (os error 10060)` timed out, `10061` refused). The receiver sends `error_detail = {e:#}` (pz-server's retry log prints it) and logs it on `job completed`. No client-side retry: the server retries 3× (#51).
- `ghostscript.rs` (child processes + files only), `backend_direct_ipp.rs` and `receiver.rs` are excluded from mutation testing; every render decision lives in `gs_plan.rs` (calls, page count, event text, gs output), `pdf_pages.rs` and `raster_merge.rs`, which are mutated. The Test job, the mutation shards AND `mutation-full.yml` install Ghostscript — without it the gs tests fail for every client mutant and count it "caught".
- **The tests prove SCALE 1, not just the size:** a clipped (`-dFIXEDMEDIA` without `-dPDFFitPage`) or shrunk landscape page has the right width × height. `tests/ghostscript_orientation.rs` compares each page's ink with the plain render's (±10 %; measured: rotated = plain exactly, clipped −31 %); the E2E URF check compares the bar-only page 2's dark pixels with the bar's area at 600 dpi (±2 %).
- **Mock printers in tests wait bounded** (`accept_bounded` in `backend_direct_ipp.rs` tests: 10 s accept + read/write timeouts).
- **Open risk (pre-existing, not #111/#112):** a Get-Job-Attributes poll that fails AFTER the printer accepted the Print-Job fails the job, and the server's 3 retries can print it again.
