//! Test-only TSPL renderer and the golden verification of the Odoo label
//! encoder against real printer jobs (#95) — no paper, no store tester.
//!
//! The renderer interprets exactly the TSPL subset that BarTender and
//! devbridge send to the Spišská TSC ML241P and returns what each `PRINT`
//! puts on the label as a dot raster in **printer coordinates** (x across the
//! head, y in feed order, (0,0) = the first dot of the first printed line):
//!
//! - `SIZE <w> mm, <h> mm` — the canvas, 8 dots/mm (a "203 dpi" TSC head);
//! - `DIRECTION 0,0` — anything else is an ERROR: the firmware semantics of
//!   `DIRECTION 1` are exactly the untested assumption the encoder avoids;
//! - `REFERENCE x,y` — origin offset added to every later object;
//! - `CLS` — clear the image buffer;
//! - `BITMAP x,y,<width bytes>,<height>,<mode>,<data>` — modes 0 OVERWRITE,
//!   1 OR, 2 XOR; a **0-bit is a black dot** (MSB = leftmost);
//! - `BAR x,y,w,h` — a black rectangle;
//! - `BARCODE …` — RECORDED, not drawn (the printer's firmware draws it):
//!   [`Printed::not_drawn`] lists it so a test sees it, never silently;
//! - `PRINT m[,n]` — snapshot of the buffer (m sets, n copies).
//!
//! `<xpml>…</xpml>` page tags and the non-drawing setup commands (`OFFSET`,
//! `SET …`, `GAP`, `DENSITY`, `SPEED`, `CODEPAGE`) are skipped; any OTHER
//! command is an error, so a drawing command the renderer does not know can
//! never silently vanish from a golden comparison. Dots outside the canvas
//! are clipped, as the printer does.
//!
//! **Convention (from the golden BarTender job):** the BarTender job that
//! prints correctly on the Spišská roll renders as the readable label
//! ROTATED 180°, so `render(doc).rotated_180()` is the label as a person
//! reads it. An Odoo PNG is delivered in that reading orientation. The job's
//! one firmware object, its EAN13 `BARCODE`, carries rotation `180` itself —
//! independent evidence for the same convention. The reference raster (and
//! every measured number below) is the BITMAPs + BARs, without that barcode.

use std::fmt;
use std::io::Cursor;

use super::layout;
use super::tspl::{self, LabelGeometry};

/// Real BarTender job for the Spišská TSC ML241P, printed correctly
/// (pz-server spool `468e3256-e67a-4040-aa4b-e34a901dc2f2`, 2026-09-28,
/// 41 072 B, SHA-256 2C3E68F7…77BDD99). TSPL despite the `.pdf` spool name.
const BARTENDER_468E3256: &[u8] = include_bytes!("fixtures/bartender_label_468e3256.tspl");

/// The Odoo PNG of label 4 (Ciabatta 400g) that devbridge 0.8.42 printed
/// upside down at Spišská (batch 11, line 548, 2026-09-30): 576 × 879, 1-bit.
const ODOO_LABEL4_PNG: &[u8] = include_bytes!("fixtures/odoo_label4_ciabatta_576x879.png");

/// Ink bounding boxes are `(x0, y0, x1, y1)`: x0/y0 inclusive, x1/y1
/// exclusive (width = x1 − x0).
type BBox = (u32, u32, u32, u32);

/// The golden reference, measured: printer-coordinate ink bbox…
const REFERENCE_INK: BBox = (29, 17, 558, 833);
/// …and the same ink in reading orientation (rotated 180° on 581 × 880).
const REFERENCE_INK_READING: BBox = (23, 47, 552, 863);

/// A label as dots. `black[y * width + x]`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Raster {
    pub width: u32,
    pub height: u32,
    black: Vec<bool>,
}

impl fmt::Debug for Raster {
    // Never dump half a million dots into an assertion message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Raster {}x{}, {} black dots, ink {:?}",
            self.width,
            self.height,
            self.black_dots(),
            self.ink_bbox()
        )
    }
}

impl Raster {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            black: vec![false; width as usize * height as usize],
        }
    }

    fn idx(&self, x: u32, y: u32) -> usize {
        y as usize * self.width as usize + x as usize
    }

    pub fn is_black(&self, x: u32, y: u32) -> bool {
        self.black[self.idx(x, y)]
    }

    pub fn set(&mut self, x: u32, y: u32, black: bool) {
        let i = self.idx(x, y);
        self.black[i] = black;
    }

    pub fn black_dots(&self) -> usize {
        self.black.iter().filter(|b| **b).count()
    }

    pub fn ink_bbox(&self) -> Option<BBox> {
        let mut bbox: Option<BBox> = None;
        for y in 0..self.height {
            for x in 0..self.width {
                if self.is_black(x, y) {
                    let (x0, y0, x1, y1) = bbox.unwrap_or((x, y, x + 1, y + 1));
                    bbox = Some((x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1)));
                }
            }
        }
        bbox
    }

    pub fn rotated_180(&self) -> Self {
        let mut out = Self::new(self.width, self.height);
        for y in 0..self.height {
            for x in 0..self.width {
                out.set(self.width - 1 - x, self.height - 1 - y, self.is_black(x, y));
            }
        }
        out
    }

    /// The `width × height` part starting at `(x, y)` (must fit).
    pub fn crop(&self, x: u32, y: u32, width: u32, height: u32) -> Self {
        assert!(x + width <= self.width && y + height <= self.height);
        let mut out = Self::new(width, height);
        for cy in 0..height {
            for cx in 0..width {
                out.set(cx, cy, self.is_black(x + cx, y + cy));
            }
        }
        out
    }

    /// Same canvas, every dot moved by `(dx, dy)`; dots moved off it are lost.
    pub fn shifted(&self, dx: i64, dy: i64) -> Self {
        let mut out = Self::new(self.width, self.height);
        for y in 0..self.height {
            for x in 0..self.width {
                let (nx, ny) = (i64::from(x) + dx, i64::from(y) + dy);
                if self.is_black(x, y)
                    && (0..i64::from(self.width)).contains(&nx)
                    && (0..i64::from(self.height)).contains(&ny)
                {
                    out.set(nx as u32, ny as u32, true);
                }
            }
        }
        out
    }

    /// Copy `other`'s black dots onto this raster with its top-left at `(x, y)`.
    pub fn paste(&mut self, other: &Raster, x: u32, y: u32) {
        for oy in 0..other.height {
            for ox in 0..other.width {
                if other.is_black(ox, oy) {
                    self.set(x + ox, y + oy, true);
                }
            }
        }
    }

    /// Number of dots that differ (both rasters must have the same size).
    pub fn diff_dots(&self, other: &Raster) -> usize {
        assert_eq!(
            (self.width, self.height),
            (other.width, other.height),
            "raster sizes differ"
        );
        self.black
            .iter()
            .zip(&other.black)
            .filter(|(a, b)| a != b)
            .count()
    }

    /// Encode as a 1-bit grayscale PNG (bit 0 = black) — what Odoo sends.
    pub fn to_png(&self) -> Vec<u8> {
        let row_bytes = self.width.div_ceil(8) as usize;
        let mut data = vec![0xFFu8; row_bytes * self.height as usize];
        for y in 0..self.height {
            for x in 0..self.width {
                if self.is_black(x, y) {
                    data[y as usize * row_bytes + x as usize / 8] &= !(0x80u8 >> (x % 8));
                }
            }
        }
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, self.width, self.height);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::One);
            let mut w = enc.write_header().expect("png header");
            w.write_image_data(&data).expect("png data");
        }
        out
    }

    /// Decode a PNG by its FIRST channel (< 128 = black; the grayscale
    /// fixtures and `to_png` output) — written here, independently of the
    /// encoder under test.
    pub fn from_png(bytes: &[u8]) -> Self {
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().expect("png info");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut buf).expect("png frame");
        let channels = match frame.color_type {
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            png::ColorType::Indexed => panic!("palette not expanded"),
        };
        let mut out = Self::new(frame.width, frame.height);
        for y in 0..frame.height {
            for x in 0..frame.width {
                let at = y as usize * frame.line_size + x as usize * channels;
                out.set(x, y, buf[at] < 128);
            }
        }
        out
    }
}

/// What one `PRINT m,n` put on the label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Printed {
    pub raster: Raster,
    pub sets: u32,
    pub copies: u32,
    /// Firmware-drawn commands on this label that are NOT in `raster`.
    pub not_drawn: Vec<String>,
}

/// Render a TSPL document (see the module doc for the supported subset).
pub(crate) fn render(doc: &[u8]) -> Result<Vec<Printed>, String> {
    let mut r = Renderer::default();
    let mut i = 0;
    while i < doc.len() {
        let rest = &doc[i..];
        if matches!(rest[0], b'\r' | b'\n' | b' ') {
            i += 1;
        } else if rest.starts_with(b"<xpml>") {
            let end = find(rest, b"</xpml>").ok_or("unterminated <xpml> tag")?;
            i += end + b"</xpml>".len();
        } else if rest.starts_with(b"BITMAP ") {
            i += r.bitmap(rest)?;
        } else {
            let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
            let line = std::str::from_utf8(&rest[..end])
                .map_err(|e| format!("non-UTF-8 command line: {e}"))?;
            r.command(line.trim())?;
            i += end;
        }
    }
    Ok(r.printed)
}

/// Render a document that must print exactly one label.
pub(crate) fn render_one(doc: &[u8]) -> Raster {
    let mut printed = render(doc).expect("renderable TSPL");
    assert_eq!(printed.len(), 1, "expected ONE printed label");
    printed.remove(0).raster
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn ints(args: &str, what: &str) -> Result<Vec<i64>, String> {
    args.split(',')
        .map(|a| {
            a.trim()
                .parse::<i64>()
                .map_err(|_| format!("{what}: bad number {a:?}"))
        })
        .collect()
}

#[derive(Default)]
struct Renderer {
    canvas: Option<Raster>,
    reference: (i64, i64),
    /// Firmware-drawn commands since the last `CLS`.
    not_drawn: Vec<String>,
    printed: Vec<Printed>,
}

impl Renderer {
    fn canvas(&mut self, what: &str) -> Result<&mut Raster, String> {
        self.canvas
            .as_mut()
            .ok_or_else(|| format!("{what} before SIZE"))
    }

    /// Paint one dot at object coordinates (REFERENCE applied, clipped).
    fn paint(&mut self, x: i64, y: i64, op: impl Fn(bool) -> bool) {
        let (rx, ry) = self.reference;
        let Some(c) = self.canvas.as_mut() else {
            return;
        };
        let (px, py) = (x + rx, y + ry);
        if (0..i64::from(c.width)).contains(&px) && (0..i64::from(c.height)).contains(&py) {
            let (px, py) = (px as u32, py as u32);
            let now = op(c.is_black(px, py));
            c.set(px, py, now);
        }
    }

    fn command(&mut self, line: &str) -> Result<(), String> {
        let (name, args) = line.split_once(' ').unwrap_or((line, ""));
        match name {
            "SIZE" => {
                let mut dots = [0u32; 2];
                let parts: Vec<&str> = args.split(',').collect();
                if parts.len() != 2 {
                    return Err(format!("SIZE needs width and height: {line:?}"));
                }
                for (d, part) in dots.iter_mut().zip(parts) {
                    let mm = part
                        .trim()
                        .strip_suffix("mm")
                        .ok_or_else(|| format!("SIZE only in mm: {line:?}"))?
                        .trim()
                        .parse::<f64>()
                        .map_err(|_| format!("SIZE: bad number in {line:?}"))?;
                    *d = (mm * 8.0).floor() as u32;
                }
                self.canvas = Some(Raster::new(dots[0], dots[1]));
            }
            "DIRECTION" => {
                let v = ints(args, "DIRECTION")?;
                if v != [0] && v != [0, 0] {
                    return Err(format!("unsupported {line:?} (only DIRECTION 0,0)"));
                }
            }
            "REFERENCE" => match ints(args, "REFERENCE")?[..] {
                [x, y] => self.reference = (x, y),
                _ => return Err(format!("REFERENCE needs x,y: {line:?}")),
            },
            "CLS" => {
                let c = self.canvas("CLS")?;
                *c = Raster::new(c.width, c.height);
                self.not_drawn.clear();
            }
            "BARCODE" => {
                self.canvas("BARCODE")?;
                self.not_drawn.push(line.to_string());
            }
            "BAR" => match ints(args, "BAR")?[..] {
                [x, y, w, h] => {
                    self.canvas("BAR")?;
                    for dy in 0..h {
                        for dx in 0..w {
                            self.paint(x + dx, y + dy, |_| true);
                        }
                    }
                }
                _ => return Err(format!("BAR needs x,y,w,h: {line:?}")),
            },
            "PRINT" => {
                let v = ints(args, "PRINT")?;
                let (sets, copies) = match v[..] {
                    [m] => (m, 1),
                    [m, n] => (m, n),
                    _ => return Err(format!("PRINT needs m[,n]: {line:?}")),
                };
                let raster = self.canvas("PRINT")?.clone();
                self.printed.push(Printed {
                    raster,
                    sets: u32::try_from(sets).map_err(|_| format!("bad {line:?}"))?,
                    copies: u32::try_from(copies).map_err(|_| format!("bad {line:?}"))?,
                    not_drawn: self.not_drawn.clone(),
                });
            }
            "OFFSET" | "SET" | "GAP" | "DENSITY" | "SPEED" | "CODEPAGE" => {}
            _ => return Err(format!("unsupported TSPL command {line:?}")),
        }
        Ok(())
    }

    /// `BITMAP x,y,wb,h,mode,<wb·h bytes>`; returns the bytes consumed.
    fn bitmap(&mut self, rest: &[u8]) -> Result<usize, String> {
        // The 5 numeric parameters end at the 5th comma; binary data follows.
        let mut commas = 0;
        let mut header_len = None;
        for (i, b) in rest.iter().enumerate().take(64) {
            if *b == b',' {
                commas += 1;
                if commas == 5 {
                    header_len = Some(i + 1);
                    break;
                }
            }
        }
        let header_len = header_len.ok_or("BITMAP header without 5 parameters")?;
        let header = std::str::from_utf8(&rest["BITMAP ".len()..header_len - 1])
            .map_err(|e| format!("BITMAP header: {e}"))?;
        let params = ints(header, "BITMAP")?;
        let [x, y, wb, h, mode] = params[..] else {
            return Err(format!("BITMAP needs 5 parameters: {header:?}"));
        };
        if !(0..=2).contains(&mode) {
            return Err(format!("unsupported BITMAP mode {mode}"));
        }
        self.canvas("BITMAP")?;
        let len = usize::try_from(wb * h).map_err(|_| "negative BITMAP size")?;
        let data = rest
            .get(header_len..header_len + len)
            .ok_or_else(|| format!("BITMAP data truncated: need {len} bytes"))?;
        for row in 0..h {
            for col in 0..wb * 8 {
                let byte = data[(row * wb + col / 8) as usize];
                let black = byte & (0x80u8 >> (col % 8)) == 0;
                match mode {
                    0 => self.paint(x + col, y + row, |_| black),
                    1 if black => self.paint(x + col, y + row, |_| true),
                    2 if black => self.paint(x + col, y + row, |was| !was),
                    _ => {}
                }
            }
        }
        Ok(header_len + len)
    }
}

// ── the encoder under test ────────────────────────────────────────────────

fn spisska() -> LabelGeometry {
    LabelGeometry {
        width_mm: 72.7,
        height_mm: 110.1,
        dpi: 203,
        rotate_180: true,
        x_offset_dots: 0,
        y_offset_dots: 0,
    }
}

/// Encode one label PNG exactly as the Odoo source does (decode → document).
fn devbridge_document(png: &[u8], copies: u32) -> Vec<u8> {
    document_with(&spisska(), png, copies)
}

/// The same with any geometry (rotation / offsets).
fn document_with(geometry: &LabelGeometry, png: &[u8], copies: u32) -> Vec<u8> {
    let bitmap = tspl::decode_png(png, geometry.max_dots()).expect("devbridge decodes the PNG");
    let label = layout::place_label(&bitmap, geometry);
    tspl::build_document(geometry, &[(&label, copies)])
}

fn within_one_dot(a: BBox, b: BBox) -> bool {
    let close = |p: u32, q: u32| p.abs_diff(q) <= 1;
    close(a.0, b.0) && close(a.1, b.1) && close(a.2, b.2) && close(a.3, b.3)
}

// ── tests ─────────────────────────────────────────────────────────────────

#[test]
fn test_renderer_reads_the_bartender_reference() {
    let printed = render(BARTENDER_468E3256).expect("the golden job renders");
    assert_eq!(printed.len(), 1);
    assert_eq!((printed[0].sets, printed[0].copies), (1, 3));
    // The one firmware object: an EAN13 drawn with rotation 180 (6th
    // parameter) — the job's own statement of the 180° convention.
    assert_eq!(
        printed[0].not_drawn,
        vec![r#"BARCODE 511,120,"EAN13",67,1,180,2,4,"858800180513""#]
    );
    assert_eq!(printed[0].not_drawn[0].split(',').nth(5), Some("180"));
    let reference = &printed[0].raster;
    assert_eq!((reference.width, reference.height), (581, 880));
    assert_eq!(reference.ink_bbox(), Some(REFERENCE_INK));
    // BITMAPs + BARs (no barcode): the same dot count as the independent
    // Python prototype renderer, which skips BARCODE too.
    assert_eq!(reference.black_dots(), 77_887);
    assert_eq!(
        reference.rotated_180().ink_bbox(),
        Some(REFERENCE_INK_READING)
    );
}

#[test]
fn test_renderer_bitmap_modes_bar_cls_reference_and_print() {
    let mut doc = b"SIZE 2 mm, 1 mm\r\nDIRECTION 0,0\r\nREFERENCE 0,0\r\nCLS\r\n".to_vec();
    doc.extend_from_slice(b"BAR 0,0, 3, 2\r\n");
    // OVERWRITE: x 8..12 black, x 12..16 white on row 0
    doc.extend_from_slice(b"BITMAP 8,0,1,1,0,\x0F\r\n");
    // OVERWRITE with all-white: clears the bar's row 0
    doc.extend_from_slice(b"BITMAP 0,0,1,1,0,\xFF\r\n");
    // XOR: bits 0,1 black -> toggles (0,1),(1,1) of the bar back to white
    doc.extend_from_slice(b"BITMAP 0,1,1,1,2,\x3F\r\n");
    // OR: only bit 0 black -> (0,3)
    doc.extend_from_slice(b"BITMAP 0,3,1,1,1,\x7F\r\n");
    doc.extend_from_slice(b"REFERENCE 4,4\r\nBAR 0,0,1,1\r\n");
    doc.extend_from_slice(b"BARCODE 9,5,\"EAN13\",2,0,180,2,4,\"123456789012\"\r\n");
    doc.extend_from_slice(b"PRINT 1,2\r\nCLS\r\nPRINT 1\r\n");
    let printed = render(&doc).unwrap();
    assert_eq!(printed.len(), 2);
    let r = &printed[0].raster;
    assert_eq!((r.width, r.height), (16, 8));
    let mut black = Vec::new();
    for y in 0..r.height {
        for x in 0..r.width {
            if r.is_black(x, y) {
                black.push((x, y));
            }
        }
    }
    assert_eq!(
        black,
        vec![(8, 0), (9, 0), (10, 0), (11, 0), (2, 1), (0, 3), (4, 4)]
    );
    assert_eq!((printed[0].sets, printed[0].copies), (1, 2));
    assert_eq!(
        printed[0].not_drawn,
        vec![r#"BARCODE 9,5,"EAN13",2,0,180,2,4,"123456789012""#],
        "a barcode is recorded, never drawn"
    );
    assert_eq!(printed[1].raster.black_dots(), 0, "CLS clears");
    assert!(
        printed[1].not_drawn.is_empty(),
        "CLS clears the barcode too"
    );
    assert_eq!((printed[1].sets, printed[1].copies), (1, 1));
}

#[test]
fn test_renderer_rejects_what_it_does_not_understand() {
    let head = "SIZE 2 mm, 1 mm\r\n";
    for bad in [
        format!("{head}DIRECTION 1,0\r\n"),
        format!("{head}DIRECTION 0,1\r\n"),
        format!("{head}TEXT 1,1,\"3\",0,1,1,\"x\"\r\n"),
        format!("{head}BITMAP 0,0,1,1,3,\x00\r\n"),
        format!("{head}BITMAP 0,0,2,2,0,\x00\r\n"),
        "PRINT 1\r\n".to_string(),
        "SIZE 2, 1\r\n".to_string(),
        "<xpml>never closed".to_string(),
    ] {
        assert!(render(bad.as_bytes()).is_err(), "must reject {bad:?}");
    }
}

#[test]
fn test_raster_rotation_and_bbox() {
    let mut r = Raster::new(3, 2);
    r.set(0, 0, true);
    assert_eq!(r.ink_bbox(), Some((0, 0, 1, 1)));
    let rot = r.rotated_180();
    assert!(rot.is_black(2, 1));
    assert_eq!(rot.black_dots(), 1);
    assert_eq!(rot.ink_bbox(), Some((2, 1, 3, 2)));
    assert_eq!(Raster::new(3, 2).ink_bbox(), None);
    assert_eq!(r.shifted(1, 1).ink_bbox(), Some((1, 1, 2, 2)));
    assert_eq!(Raster::from_png(&rot.to_png()), rot);
}

/// RED on 0.8.42: the readable label, encoded by devbridge at the full
/// canvas size, must print dot-for-dot like the BarTender job.
#[test]
fn test_golden_round_trip_full_canvas_matches_bartender_dot_for_dot() {
    let reference = render_one(BARTENDER_468E3256);
    let readable = reference.rotated_180();
    let ours = render_one(&devbridge_document(&readable.to_png(), 3));
    assert_eq!((ours.width, ours.height), (581, 880));
    assert_eq!(ours.ink_bbox(), Some(REFERENCE_INK), "{ours:?}");
    assert_eq!(ours.diff_dots(&reference), 0, "{ours:?} vs {reference:?}");
}

/// RED on 0.8.42: the same label cut to Odoo's PNG size (576 × 879,
/// centred in reading orientation) prints like BarTender within ±1 dot of
/// centring rounding — and dot-for-dot once that shift is undone.
#[test]
fn test_golden_round_trip_at_odoo_png_size_within_one_dot() {
    let reference = render_one(BARTENDER_468E3256);
    let odoo_png = reference.rotated_180().crop(2, 0, 576, 879);
    let ours = render_one(&devbridge_document(&odoo_png.to_png(), 1));
    let bbox = ours.ink_bbox().expect("ink printed");
    assert!(
        within_one_dot(bbox, REFERENCE_INK),
        "ink {bbox:?} vs BarTender {REFERENCE_INK:?}"
    );
    let (dx, dy) = (
        i64::from(REFERENCE_INK.0) - i64::from(bbox.0),
        i64::from(REFERENCE_INK.1) - i64::from(bbox.1),
    );
    assert_eq!(ours.shifted(dx, dy).diff_dots(&reference), 0, "{ours:?}");
}

/// RED on 0.8.42: the real Odoo label 4 that came out upside down. Same
/// document size as the production job (63 406 B) and, in the BarTender
/// convention, it reads upright: the reading-orientation render IS the PNG,
/// placed at (3,1) by the centring (BITMAP 2,0 in printer coordinates).
#[test]
fn test_golden_odoo_label4_reads_upright() {
    let doc = devbridge_document(ODOO_LABEL4_PNG, 1);
    assert_eq!(doc.len(), 63_406, "the batch-11 job size");
    let reading = render_one(&doc).rotated_180();
    assert_eq!(reading.ink_bbox(), Some((26, 29, 549, 752)), "{reading:?}");
    let png = Raster::from_png(ODOO_LABEL4_PNG);
    assert_eq!(
        (png.width, png.height, png.black_dots()),
        (576, 879, 36_414)
    );
    let mut want = Raster::new(581, 880);
    want.paste(&png, 3, 1);
    assert_eq!(reading.diff_dots(&want), 0, "{reading:?} vs {want:?}");
}

/// RED on 0.8.42: a block in the PNG's top-left corner is in the top-left
/// corner of the label as read — not mirrored, not flipped.
#[test]
fn test_orientation_marker_block_stays_top_left() {
    let mut png = Raster::new(576, 879);
    for y in 60..84 {
        for x in 10..50 {
            png.set(x, y, true);
        }
    }
    let reading = render_one(&devbridge_document(&png.to_png(), 1)).rotated_180();
    assert_eq!(reading.ink_bbox(), Some((13, 61, 53, 85)), "{reading:?}");
    assert_eq!(reading.black_dots(), 40 * 24);
}

/// Offsets move the real label by exactly that many dots as it is read.
#[test]
fn test_golden_offsets_shift_the_real_label_exactly() {
    let reference = render_one(BARTENDER_468E3256);
    let png = reference.rotated_180().crop(2, 0, 576, 879).to_png();
    let base = render_one(&devbridge_document(&png, 1)).rotated_180();
    for (dx, dy) in [(5, -1), (-7, 0), (0, 3)] {
        let g = LabelGeometry {
            x_offset_dots: dx,
            y_offset_dots: dy,
            ..spisska()
        };
        let moved = render_one(&document_with(&g, &png, 1)).rotated_180();
        assert_eq!(
            moved.diff_dots(&base.shifted(i64::from(dx), i64::from(dy))),
            0,
            "offset {dx},{dy}: {moved:?} vs {base:?}"
        );
    }
}

/// `rotate_180 = false` keeps the 0.8.42 orientation: the PNG prints as it
/// is — for a full-width PNG byte for byte the 0.8.42 document.
#[test]
fn test_golden_rotate_180_false_prints_the_png_as_it_is() {
    let g = LabelGeometry {
        rotate_180: false,
        ..spisska()
    };
    let doc = document_with(&g, ODOO_LABEL4_PNG, 1);
    assert_eq!(doc.len(), 63_406);
    let mut want = Raster::new(581, 880);
    want.paste(&Raster::from_png(ODOO_LABEL4_PNG), 2, 0);
    assert_eq!(render_one(&doc).diff_dots(&want), 0);

    let full = render_one(BARTENDER_468E3256).rotated_180().to_png();
    let bitmap = tspl::decode_png(&full, g.max_dots()).unwrap();
    let mut old = tspl::document_header(&g);
    old.extend_from_slice(b"CLS\r\nBITMAP 0,0,73,880,0,");
    old.extend_from_slice(&bitmap.data);
    old.extend_from_slice(b"\r\nPRINT 1,1\r\n");
    assert_eq!(document_with(&g, &full, 1), old);
}

/// The per-label log line and the WARN for the real Odoo label 4: its top
/// margin (29 dots) is inside BarTender's 47 — Odoo's layout, reported, not
/// blocked. The BarTender label itself is exactly on the safe margins.
#[test]
fn test_golden_layout_diagnostics_on_real_labels() {
    let g = spisska();
    let label4 = tspl::decode_png(ODOO_LABEL4_PNG, g.max_dots()).unwrap();
    let placed = layout::place_label(&label4, &g);
    assert_eq!(
        placed.layout.to_string(),
        "png 576x879 -> BITMAP 2,0 rotated 180, ink (26,29)-(549,752) as read, margins L26 T29 R32 B128"
    );
    assert_eq!(
        placed.layout.warnings(),
        vec!["ink 29 dots from the top edge, BarTender keeps >= 47"]
    );
    let bartender = render_one(BARTENDER_468E3256).rotated_180().to_png();
    let bartender = tspl::decode_png(&bartender, g.max_dots()).unwrap();
    let placed = layout::place_label(&bartender, &g);
    assert_eq!(
        placed.layout.to_string(),
        "png 581x880 -> BITMAP 0,0 rotated 180, ink (23,47)-(552,863) as read, margins L23 T47 R29 B17"
    );
    assert!(placed.layout.warnings().is_empty());
}

/// A batch = ONE document: each label cleared, placed and printed in order
/// with its own copies.
#[test]
fn test_golden_batch_document_prints_each_label_in_order() {
    let g = spisska();
    let mut marker = Raster::new(576, 879);
    marker.set(0, 0, true);
    let a = layout::place_label(
        &tspl::decode_png(ODOO_LABEL4_PNG, g.max_dots()).unwrap(),
        &g,
    );
    let b = layout::place_label(
        &tspl::decode_png(&marker.to_png(), g.max_dots()).unwrap(),
        &g,
    );
    let printed = render(&tspl::build_document(&g, &[(&a, 2), (&b, 1)])).unwrap();
    assert_eq!(printed.len(), 2);
    assert_eq!((printed[0].sets, printed[0].copies), (1, 2));
    assert_eq!((printed[1].sets, printed[1].copies), (1, 1));
    let mut want = Raster::new(581, 880);
    want.paste(&Raster::from_png(ODOO_LABEL4_PNG), 3, 1);
    assert_eq!(printed[0].raster.rotated_180().diff_dots(&want), 0);
    assert_eq!(
        printed[1].raster.rotated_180().ink_bbox(),
        Some((3, 1, 4, 2)),
        "CLS cleared label 4 before the marker"
    );
}
