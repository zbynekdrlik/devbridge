//! PNG → 1-bit → TSPL encoder for Odoo labels (#90).
//!
//! Odoo sends each label as a final, portrait PNG rendered at the printer's
//! exact dot size (576 × 880 px for the Spišská 72.7 × 110.1 mm roll at
//! 203 dpi). The client turns it into TSPL so the printer language stays in
//! devbridge (a printer swap is a devbridge change, never an Odoo one).
//!
//! The document format mirrors, byte for byte, what the working TSC BarTender
//! driver sends through devbridge today (captured on pz-server, job
//! `015bc33d`): CRLF line ends, header
//!
//! ```text
//! SIZE 72.7 mm, 110.1 mm
//! DIRECTION 0,0
//! REFERENCE 0,0
//! OFFSET 0 mm
//! SET TEAR ON
//! ```
//!
//! and per label `CLS` / `BITMAP 0,0,<width bytes>,<height>,0,<data>` /
//! `PRINT 1,<copies>`. `GAP`, `DENSITY` and `SPEED` are deliberately NOT sent:
//! the printer uses its own stored calibration, exactly as with BarTender.
//!
//! BITMAP data: rows top to bottom, `width_bytes` per row, MSB = leftmost dot,
//! and a **1-bit is white (no dot), a 0-bit black** — the BarTender bitmaps
//! are ~80 % 0xFF on white labels (fixture test below). Pixels are thresholded
//! at 50 % with no dithering (barcodes must stay sharp); transparency is
//! composited over white; the padding dots up to the next multiple of 8 are
//! white.

use std::io::Cursor;

/// Label roll size + resolution (from `[client.odoo]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LabelGeometry {
    pub width_mm: f64,
    pub height_mm: f64,
    pub dpi: u32,
}

impl LabelGeometry {
    /// Printable dots of the roll: `round(mm × dpi / 25.4)`. Rounding (not
    /// flooring) matters: a "203 dpi" TSC head is really 8 dots/mm, so
    /// 110.1 mm is 880 dots (879.93 at 203.0 dpi) — Odoo's 880-px label fits.
    pub fn max_dots(&self) -> (u32, u32) {
        (
            mm_to_dots(self.width_mm, self.dpi),
            mm_to_dots(self.height_mm, self.dpi),
        )
    }
}

fn mm_to_dots(mm: f64, dpi: u32) -> u32 {
    (mm * f64::from(dpi) / 25.4).round().max(0.0) as u32
}

/// A label as TSPL BITMAP data (1 = white).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonoBitmap {
    pub width: u32,
    pub height: u32,
    pub width_bytes: u32,
    pub data: Vec<u8>,
}

/// Why a line's PNG cannot be printed. [`LabelError::ack_text`] is what Odoo
/// gets in the ack `error` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelError {
    /// `label_png_base64` empty / missing — never print a blank label.
    Empty,
    /// Not valid base64.
    Base64(String),
    /// Not a decodable PNG.
    Png(String),
    /// Larger than the roll — rejected, never cropped or scaled.
    Size {
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    },
}

impl LabelError {
    /// Contract texts agreed on #90: `empty png`, `size`, else a short cause.
    pub fn ack_text(&self) -> String {
        match self {
            Self::Empty => "empty png".into(),
            Self::Size { .. } => "size".into(),
            Self::Base64(e) => format!("invalid png base64: {e}"),
            Self::Png(e) => format!("invalid png: {e}"),
        }
    }
}

impl std::fmt::Display for LabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Size {
                width,
                height,
                max_width,
                max_height,
            } => write!(
                f,
                "size: PNG {width}x{height} px exceeds the label {max_width}x{max_height} dots"
            ),
            other => f.write_str(&other.ack_text()),
        }
    }
}

/// Decode Odoo's `label_png_base64` into BITMAP data, enforcing the roll size.
pub fn decode_label(png_base64: &str, geometry: &LabelGeometry) -> Result<MonoBitmap, LabelError> {
    use base64::Engine as _;
    let trimmed = png_base64.trim();
    if trimmed.is_empty() {
        return Err(LabelError::Empty);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|e| LabelError::Base64(e.to_string()))?;
    decode_png(&bytes, geometry.max_dots())
}

/// Decode PNG bytes into BITMAP data. The size is checked from the header
/// BEFORE any pixel is decoded.
pub fn decode_png(
    bytes: &[u8],
    (max_width, max_height): (u32, u32),
) -> Result<MonoBitmap, LabelError> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    // Palette → RGB, <8-bit gray → 8-bit, tRNS → alpha, 16-bit → 8-bit.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| LabelError::Png(e.to_string()))?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if width > max_width || height > max_height {
        return Err(LabelError::Size {
            width,
            height,
            max_width,
            max_height,
        });
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| LabelError::Png(e.to_string()))?;
    let channels = match frame.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => {
            return Err(LabelError::Png(
                "indexed PNG was not expanded to RGB".into(),
            ));
        }
    };
    if frame.bit_depth != png::BitDepth::Eight {
        return Err(LabelError::Png(format!(
            "unexpected bit depth {:?} after normalization",
            frame.bit_depth
        )));
    }
    let line_size = frame.line_size;
    let width_bytes = width.div_ceil(8);
    // All white (1s) — padding dots included — then clear the black ones.
    let mut data = vec![0xFFu8; width_bytes as usize * height as usize];
    for y in 0..height as usize {
        let row = &buf[y * line_size..y * line_size + width as usize * channels];
        for x in 0..width as usize {
            let px = &row[x * channels..(x + 1) * channels];
            if is_black(px) {
                let byte = y * width_bytes as usize + x / 8;
                data[byte] &= !(0x80u8 >> (x % 8));
            }
        }
    }
    Ok(MonoBitmap {
        width,
        height,
        width_bytes,
        data,
    })
}

/// 50 % threshold on luminance, alpha composited over white (a fully
/// transparent pixel is paper). No dithering.
fn is_black(px: &[u8]) -> bool {
    let (luma, alpha) = match *px {
        [g] => (u32::from(g), 255u32),
        [g, a] => (u32::from(g), u32::from(a)),
        [r, g, b] => (luminance(r, g, b), 255),
        [r, g, b, a] => (luminance(r, g, b), u32::from(a)),
        _ => return false,
    };
    // Over white: luma·a + 255·(255 − a), scaled by 255.
    let over_white = luma * alpha + 255 * (255 - alpha);
    over_white < 128 * 255
}

/// ITU-R BT.601 integer luma.
fn luminance(r: u8, g: u8, b: u8) -> u32 {
    (299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000
}

/// `72.7` → `72.7`, `110.0` → `110` (shortest exact decimal).
fn format_mm(mm: f64) -> String {
    format!("{mm}")
}

/// Document header — sent once per spooler document (one Odoo batch).
pub fn document_header(geometry: &LabelGeometry) -> Vec<u8> {
    format!(
        "SIZE {} mm, {} mm\r\nDIRECTION 0,0\r\nREFERENCE 0,0\r\nOFFSET 0 mm\r\nSET TEAR ON\r\n",
        format_mm(geometry.width_mm),
        format_mm(geometry.height_mm)
    )
    .into_bytes()
}

/// One label: clear the image buffer, place the bitmap at 0,0, print
/// `copies` copies of it.
pub fn label_commands(bitmap: &MonoBitmap, copies: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(bitmap.data.len() + 64);
    out.extend_from_slice(b"CLS\r\n");
    out.extend_from_slice(
        format!("BITMAP 0,0,{},{},0,", bitmap.width_bytes, bitmap.height).as_bytes(),
    );
    out.extend_from_slice(&bitmap.data);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(format!("PRINT 1,{copies}\r\n").as_bytes());
    out
}

/// The whole batch as ONE TSPL document: header, then each label in order.
pub fn build_document(geometry: &LabelGeometry, labels: &[(&MonoBitmap, u32)]) -> Vec<u8> {
    let mut doc = document_header(geometry);
    for (bitmap, copies) in labels {
        doc.extend_from_slice(&label_commands(bitmap, *copies));
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPISSKA: LabelGeometry = LabelGeometry {
        width_mm: 72.7,
        height_mm: 110.1,
        dpi: 203,
    };

    /// 7 × 80 BITMAP block taken verbatim from a real BarTender job for the
    /// Spišská TSC ML241P (pz-server spool `015bc33d…`, first `BITMAP 271,35,7,80,1,`).
    const BARTENDER_7X80: &[u8] = include_bytes!("fixtures/bartender_bitmap_7x80.bin");

    fn png_bytes(
        width: u32,
        height: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, width, height);
            enc.set_color(color);
            enc.set_depth(depth);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(data).unwrap();
        }
        out
    }

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn test_max_dots_rounds_so_odoo_576x880_fits_the_spisska_roll() {
        assert_eq!(SPISSKA.max_dots(), (581, 880));
        let g = LabelGeometry {
            width_mm: 50.0,
            height_mm: 30.0,
            dpi: 300,
        };
        assert_eq!(g.max_dots(), (591, 354));
    }

    #[test]
    fn test_document_header_is_the_bartender_header_without_gap_density_speed() {
        let h = String::from_utf8(document_header(&SPISSKA)).unwrap();
        assert_eq!(
            h,
            "SIZE 72.7 mm, 110.1 mm\r\nDIRECTION 0,0\r\nREFERENCE 0,0\r\nOFFSET 0 mm\r\nSET TEAR ON\r\n"
        );
        for banned in ["GAP", "DENSITY", "SPEED"] {
            assert!(
                !h.contains(banned),
                "{banned} would override the printer calibration"
            );
        }
        let g = LabelGeometry {
            width_mm: 100.0,
            height_mm: 50.5,
            dpi: 203,
        };
        assert!(
            String::from_utf8(document_header(&g))
                .unwrap()
                .starts_with("SIZE 100 mm, 50.5 mm\r\n")
        );
    }

    #[test]
    fn test_label_commands_exact_bytes() {
        let bmp = MonoBitmap {
            width: 10,
            height: 2,
            width_bytes: 2,
            data: vec![0x00, 0x3F, 0xFF, 0xFF],
        };
        let got = label_commands(&bmp, 40);
        let mut want = b"CLS\r\nBITMAP 0,0,2,2,0,".to_vec();
        want.extend_from_slice(&[0x00, 0x3F, 0xFF, 0xFF]);
        want.extend_from_slice(b"\r\nPRINT 1,40\r\n");
        assert_eq!(got, want);
    }

    #[test]
    fn test_build_document_is_header_then_each_label_in_order() {
        let a = MonoBitmap {
            width: 8,
            height: 1,
            width_bytes: 1,
            data: vec![0xAA],
        };
        let b = MonoBitmap {
            width: 8,
            height: 1,
            width_bytes: 1,
            data: vec![0x55],
        };
        let doc = build_document(&SPISSKA, &[(&a, 2), (&b, 1)]);
        let mut want = document_header(&SPISSKA);
        want.extend(label_commands(&a, 2));
        want.extend(label_commands(&b, 1));
        assert_eq!(doc, want);
        assert_eq!(build_document(&SPISSKA, &[]), document_header(&SPISSKA));
    }

    #[test]
    fn test_polarity_white_is_one_black_is_zero_and_padding_is_white() {
        // 10 px wide: black, white, black, then white; 2nd row all black.
        let mut row1 = vec![255u8; 10];
        row1[0] = 0;
        row1[2] = 0;
        let row2 = vec![0u8; 10];
        let png = png_bytes(
            10,
            2,
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            &[row1, row2].concat(),
        );
        let bmp = decode_png(&png, (581, 880)).unwrap();
        assert_eq!((bmp.width, bmp.height, bmp.width_bytes), (10, 2, 2));
        // row 1: 0b0101_1111, 0b1111_1111 ; row 2: 0x00, then pad bits 2..8 white
        assert_eq!(bmp.data, vec![0x5F, 0xFF, 0x00, 0x3F]);
    }

    #[test]
    fn test_threshold_is_50_percent_without_dithering() {
        let png = png_bytes(
            4,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            &[127, 128, 0, 255],
        );
        let bmp = decode_png(&png, (581, 880)).unwrap();
        // 127 black, 128 white, 0 black, 255 white; then 4 white pad bits.
        assert_eq!(bmp.data, vec![0b0101_1111]);
    }

    #[test]
    fn test_one_bit_png_as_odoo_renders_it() {
        // Odoo: 1 bit/px black-and-white. In a 1-bit gray PNG 0 = black.
        let png = png_bytes(
            16,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::One,
            &[0b0000_1111, 0b1111_0000],
        );
        let bmp = decode_png(&png, (581, 880)).unwrap();
        assert_eq!(bmp.data, vec![0b0000_1111, 0b1111_0000]);
    }

    #[test]
    fn test_rgb_rgba_and_transparency_composite_over_white() {
        let rgb = png_bytes(
            3,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &[0, 0, 0, 255, 255, 255, 200, 20, 20],
        );
        // black, white, dark-red (luma ~83) → black
        assert_eq!(
            decode_png(&rgb, (581, 880)).unwrap().data,
            vec![0b0101_1111]
        );
        // opaque black, fully transparent black (= paper), half-transparent black
        let rgba = png_bytes(
            3,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[0, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 200],
        );
        assert_eq!(
            decode_png(&rgba, (581, 880)).unwrap().data,
            vec![0b0101_1111]
        );
    }

    #[test]
    fn test_luminance_weights_decide_the_threshold() {
        // Each colour sits on a side of 50 % only because of its channel weight.
        let rgb = png_bytes(
            4,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &[255, 90, 90, 0, 255, 0, 100, 150, 255, 255, 0, 0],
        );
        // light red 139 white, green 149 white, light blue 147 white, red 76 black
        assert_eq!(
            decode_png(&rgb, (581, 880)).unwrap().data,
            vec![0b1110_1111]
        );
        assert_eq!(luminance(255, 90, 90), 139);
        assert_eq!(luminance(0, 255, 0), 149);
        assert_eq!(luminance(100, 150, 255), 147);
    }

    #[test]
    fn test_gray_alpha_png() {
        // opaque black, transparent black, opaque white
        let png = png_bytes(
            3,
            1,
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            &[0, 255, 0, 0, 255, 255],
        );
        assert_eq!(
            decode_png(&png, (581, 880)).unwrap().data,
            vec![0b0111_1111]
        );
    }

    #[test]
    fn test_bartender_fixture_polarity_and_packing_round_trip() {
        // Real driver output: a label is mostly paper, so if 1 = white the
        // bitmap is mostly 1-bits.
        let ones: u32 = BARTENDER_7X80.iter().map(|b| b.count_ones()).sum();
        let total = BARTENDER_7X80.len() as u32 * 8;
        assert!(
            ones * 100 / total >= 70,
            "only {ones}/{total} bits set — polarity would be inverted"
        );
        // Render it as an image with that polarity (1 → white 255, 0 → black 0)
        // and encode it back: our bytes must equal BarTender's exactly.
        let (w, h) = (56u32, 80u32);
        let mut gray = Vec::with_capacity((w * h) as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let bit = BARTENDER_7X80[y * 7 + x / 8] & (0x80 >> (x % 8));
                gray.push(if bit != 0 { 255 } else { 0 });
            }
        }
        let png = png_bytes(w, h, png::ColorType::Grayscale, png::BitDepth::Eight, &gray);
        let bmp = decode_png(&png, SPISSKA.max_dots()).unwrap();
        assert_eq!(bmp.width_bytes, 7);
        assert_eq!(bmp.data, BARTENDER_7X80);
    }

    #[test]
    fn test_full_size_label_accepted_and_one_dot_more_rejected() {
        let ok = png_bytes(
            576,
            880,
            png::ColorType::Grayscale,
            png::BitDepth::One,
            &vec![0xFF; 72 * 880],
        );
        let bmp = decode_label(&b64(&ok), &SPISSKA).unwrap();
        assert_eq!(
            (bmp.width, bmp.height, bmp.width_bytes, bmp.data.len()),
            (576, 880, 72, 63_360)
        );
        assert!(
            bmp.data.iter().all(|b| *b == 0xFF),
            "white PNG → all white bits"
        );

        let tall = png_bytes(
            8,
            881,
            png::ColorType::Grayscale,
            png::BitDepth::One,
            &vec![0xFF; 881],
        );
        let err = decode_label(&b64(&tall), &SPISSKA).unwrap_err();
        assert_eq!(
            err,
            LabelError::Size {
                width: 8,
                height: 881,
                max_width: 581,
                max_height: 880
            }
        );
        assert_eq!(err.ack_text(), "size");
        assert!(err.to_string().contains("8x881"), "{err}");

        let wide = png_bytes(
            584,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::One,
            &[0xFF; 73],
        );
        assert_eq!(
            decode_label(&b64(&wide), &SPISSKA).unwrap_err().ack_text(),
            "size"
        );
        // exactly the roll width (581) still fits
        let edge = png_bytes(
            581,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::One,
            &[0xFF; 73],
        );
        assert_eq!(decode_label(&b64(&edge), &SPISSKA).unwrap().width_bytes, 73);
    }

    #[test]
    fn test_empty_and_invalid_inputs() {
        assert_eq!(
            decode_label("", &SPISSKA).unwrap_err().ack_text(),
            "empty png"
        );
        assert_eq!(
            decode_label("  \n", &SPISSKA).unwrap_err(),
            LabelError::Empty
        );
        assert!(matches!(
            decode_label("!!notbase64", &SPISSKA),
            Err(LabelError::Base64(_))
        ));
        let not_png = decode_label(&b64(b"hello, not a png"), &SPISSKA).unwrap_err();
        assert!(matches!(not_png, LabelError::Png(_)), "{not_png:?}");
        assert!(
            not_png.ack_text().starts_with("invalid png: "),
            "{}",
            not_png.ack_text()
        );
    }
}
