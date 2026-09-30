//! Where an Odoo label lands on the roll (#95): 180° rotation, centring on
//! the `SIZE` canvas, fine-tuning offsets, and the ink diagnostics.
//!
//! **Orientation.** The BarTender job that prints correctly on the Spišská
//! roll (`fixtures/bartender_label_468e3256.tspl`, verified by the golden
//! tests in `tspl_golden.rs`) sends its content ROTATED 180° in printer
//! coordinates, with the same header devbridge sends (`DIRECTION 0,0`,
//! `REFERENCE 0,0`). Odoo's PNG is in reading orientation, so with
//! `rotate_180` (the default) the encoder rotates the bitmap itself — rows in
//! reverse order, dots within each row in reverse order — and the header stays
//! byte-identical to BarTender's. (`DIRECTION 1` was rejected: its firmware
//! semantics would be an untested assumption.)
//!
//! **Placement.** The bitmap is centred on the canvas in printer coordinates,
//! `x = (canvas_w − w) / 2`, `y = (canvas_h − h) / 2` — Odoo's 576 × 879 PNG
//! on the 581 × 880 roll is `BITMAP 2,0`. `x_offset_dots` / `y_offset_dots`
//! then move the label in its READING orientation (the PNG's own axes: + is
//! right / down as a person reads the label), whatever `rotate_180` is. Dots
//! an offset pushes past the canvas edge are cropped (TSPL has no negative
//! coordinates; the printer would clip them anyway) and reported.
//!
//! **Diagnostics.** Every label reports its printed ink bounding box as read,
//! and [`LabelLayout::warnings`] flags (WARN, never a reject) ink cut off by an
//! offset and ink closer to an edge than BarTender ever prints on this roll
//! ([`BARTENDER_SAFE_MARGINS`]) — a visible signal of Odoo layout drift that
//! never blocks printing.

use std::borrow::Cow;
use std::fmt;

use super::tspl::{LabelGeometry, MonoBitmap};

/// Distances in dots from each edge of the label as it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Margins {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

/// The closest BarTender ever prints to each edge of the Spišská roll, as
/// read (L/T/R/B: job `015bc33d` 13/56/15/35, job `468e3256` 23/47/29/17).
pub const BARTENDER_SAFE_MARGINS: Margins = Margins {
    left: 13,
    top: 47,
    right: 13,
    bottom: 17,
};

/// A rectangle of dots: `x0`/`y0` inclusive, `x1`/`y1` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DotRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl DotRect {
    fn contains(&self, other: &DotRect) -> bool {
        self.x0 <= other.x0 && self.y0 <= other.y0 && other.x1 <= self.x1 && other.y1 <= self.y1
    }
}

impl fmt::Display for DotRect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({},{})-({},{})", self.x0, self.y0, self.x1, self.y1)
    }
}

impl MonoBitmap {
    /// An all-white bitmap (padding dots included).
    pub fn blank(width: u32, height: u32) -> Self {
        let width_bytes = width.div_ceil(8);
        Self {
            width,
            height,
            width_bytes,
            data: vec![0xFF; width_bytes as usize * height as usize],
        }
    }

    fn byte_and_mask(&self, x: u32, y: u32) -> (usize, u8) {
        (
            y as usize * self.width_bytes as usize + x as usize / 8,
            0x80u8 >> (x % 8),
        )
    }

    /// Whether dot `(x, y)` is black (a 0-bit).
    pub fn is_black(&self, x: u32, y: u32) -> bool {
        let (i, mask) = self.byte_and_mask(x, y);
        self.data[i] & mask == 0
    }

    fn set_black(&mut self, x: u32, y: u32) {
        let (i, mask) = self.byte_and_mask(x, y);
        self.data[i] &= !mask;
    }

    /// Rotated 180°: rows in reverse order and the dots of each row in
    /// reverse order — dot `(x, y)` moves to `(w−1−x, h−1−y)`. The padding
    /// dots up to the next whole byte stay at the RIGHT end of each row,
    /// white (a byte-wise bit reversal would move them to the left edge).
    pub fn rotated_180(&self) -> Self {
        let mut out = Self::blank(self.width, self.height);
        for y in 0..self.height {
            for x in 0..self.width {
                if self.is_black(x, y) {
                    out.set_black(self.width - 1 - x, self.height - 1 - y);
                }
            }
        }
        out
    }

    /// The dots inside `rect` (which lies within the bitmap).
    pub fn crop(&self, rect: DotRect) -> Self {
        let mut out = Self::blank(rect.x1 - rect.x0, rect.y1 - rect.y0);
        for y in rect.y0..rect.y1 {
            for x in rect.x0..rect.x1 {
                if self.is_black(x, y) {
                    out.set_black(x - rect.x0, y - rect.y0);
                }
            }
        }
        out
    }

    /// Bounding box of the black dots; `None` for a blank label. Padding
    /// dots beyond `width` never count.
    pub fn ink_bbox(&self) -> Option<DotRect> {
        let mut bbox: Option<DotRect> = None;
        for y in 0..self.height {
            for x in 0..self.width {
                if self.is_black(x, y) {
                    bbox = Some(match bbox {
                        None => DotRect {
                            x0: x,
                            y0: y,
                            x1: x + 1,
                            y1: y + 1,
                        },
                        Some(b) => DotRect {
                            x0: b.x0.min(x),
                            y0: b.y0.min(y),
                            x1: b.x1.max(x + 1),
                            y1: b.y1.max(y + 1),
                        },
                    });
                }
            }
        }
        bbox
    }
}

/// Where one label lands and what prints — logged for every label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelLayout {
    /// Size of Odoo's PNG in dots.
    pub png: (u32, u32),
    /// The `SIZE` canvas in dots.
    pub canvas: (u32, u32),
    pub rotate_180: bool,
    /// `BITMAP x,y` of what prints (printer coordinates).
    pub bitmap_at: (u32, u32),
    /// The printed ink on the canvas as the label is read; `None` when
    /// nothing black prints.
    pub ink: Option<DotRect>,
    /// Some ink fell off the canvas because of the offsets.
    pub ink_cropped: bool,
}

impl LabelLayout {
    /// How far the printed ink is from each edge of the label as read.
    pub fn ink_margins(&self) -> Option<Margins> {
        let (width, height) = self.canvas;
        self.ink.map(|ink| Margins {
            left: ink.x0,
            top: ink.y0,
            right: width - ink.x1,
            bottom: height - ink.y1,
        })
    }

    /// What deserves a WARN — never a reject: ink cut off by the offsets, and
    /// ink closer to an edge than BarTender ever prints on this roll.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.ink_cropped {
            out.push("ink cut off at the label edge by x_offset_dots / y_offset_dots".to_string());
        }
        if let Some(m) = self.ink_margins() {
            let safe = BARTENDER_SAFE_MARGINS;
            for (side, got, min) in [
                ("left", m.left, safe.left),
                ("top", m.top, safe.top),
                ("right", m.right, safe.right),
                ("bottom", m.bottom, safe.bottom),
            ] {
                if got < min {
                    out.push(format!(
                        "ink {got} dots from the {side} edge, BarTender keeps >= {min}"
                    ));
                }
            }
        }
        out
    }
}

impl fmt::Display for LabelLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (png_w, png_h) = self.png;
        let (x, y) = self.bitmap_at;
        let turn = if self.rotate_180 { " rotated 180" } else { "" };
        write!(f, "png {png_w}x{png_h} -> BITMAP {x},{y}{turn}")?;
        match (self.ink, self.ink_margins()) {
            (Some(ink), Some(m)) => write!(
                f,
                ", ink {ink} as read, margins L{} T{} R{} B{}",
                m.left, m.top, m.right, m.bottom
            ),
            _ => write!(f, ", no ink"),
        }
    }
}

/// A label ready for TSPL: the bitmap in printer orientation plus its layout
/// (`layout.bitmap_at` is where it goes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedLabel {
    pub bitmap: MonoBitmap,
    pub layout: LabelLayout,
}

/// `v` limited to `0..=max`.
fn clamp_dots(v: i64, max: u32) -> u32 {
    v.clamp(0, i64::from(max)) as u32
}

/// Rotate, centre and offset one label (a PNG-orientation bitmap that fits
/// the roll — `decode_png` guarantees that) on the `SIZE` canvas.
pub fn place_label(bitmap: &MonoBitmap, geometry: &LabelGeometry) -> PlacedLabel {
    let (canvas_w, canvas_h) = geometry.max_dots();
    let (cw, ch) = (i64::from(canvas_w), i64::from(canvas_h));
    let (w, h) = (i64::from(bitmap.width), i64::from(bitmap.height));
    // Centred in printer coordinates: 576 x 879 on 581 x 880 -> BITMAP 2,0.
    let (centre_x, centre_y) = ((cw - w).max(0) / 2, (ch - h).max(0) / 2);
    // The PNG's top-left corner on the canvas as the label is READ (rotated
    // 180° the centring gap swaps sides), then the reading-orientation offsets.
    let (rx, ry) = if geometry.rotate_180 {
        (cw - w - centre_x, ch - h - centre_y)
    } else {
        (centre_x, centre_y)
    };
    let rx = rx + i64::from(geometry.x_offset_dots);
    let ry = ry + i64::from(geometry.y_offset_dots);

    // The PNG dots that land on the canvas (PNG coordinates).
    let visible = DotRect {
        x0: clamp_dots(-rx, bitmap.width),
        y0: clamp_dots(-ry, bitmap.height),
        x1: clamp_dots(cw - rx, bitmap.width),
        y1: clamp_dots(ch - ry, bitmap.height),
    };
    let whole = DotRect {
        x0: 0,
        y0: 0,
        x1: bitmap.width,
        y1: bitmap.height,
    };
    let shown: Cow<'_, MonoBitmap> = if visible == whole {
        Cow::Borrowed(bitmap)
    } else {
        Cow::Owned(bitmap.crop(visible))
    };
    let ink_cropped =
        visible != whole && bitmap.ink_bbox().is_some_and(|ink| !visible.contains(&ink));

    // Top-left of the shown part on the canvas as read (the canvas edge when
    // the PNG starts before it), and its printed ink.
    let sx = clamp_dots(rx, canvas_w);
    let sy = clamp_dots(ry, canvas_h);
    let ink = shown.ink_bbox().map(|i| DotRect {
        x0: i.x0 + sx,
        y0: i.y0 + sy,
        x1: i.x1 + sx,
        y1: i.y1 + sy,
    });
    // As read, the shown part spans sx..sx+width; rotated 180° its printer
    // left edge is the reading right edge.
    let bitmap_at = if geometry.rotate_180 {
        (canvas_w - sx - shown.width, canvas_h - sy - shown.height)
    } else {
        (sx, sy)
    };
    let printed = if geometry.rotate_180 {
        shown.rotated_180()
    } else {
        shown.into_owned()
    };
    PlacedLabel {
        layout: LabelLayout {
            png: (bitmap.width, bitmap.height),
            canvas: (canvas_w, canvas_h),
            rotate_180: geometry.rotate_180,
            bitmap_at,
            ink,
            ink_cropped,
        },
        bitmap: printed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(rotate_180: bool, x_offset_dots: i32, y_offset_dots: i32) -> LabelGeometry {
        LabelGeometry {
            width_mm: 72.7,
            height_mm: 110.1,
            dpi: 203,
            rotate_180,
            x_offset_dots,
            y_offset_dots,
        }
    }

    /// `w × h` white bitmap with the given black dots.
    fn bitmap(w: u32, h: u32, black: &[(u32, u32)]) -> MonoBitmap {
        let mut b = MonoBitmap::blank(w, h);
        for &(x, y) in black {
            b.set_black(x, y);
        }
        b
    }

    fn black_dots(b: &MonoBitmap) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for y in 0..b.height {
            for x in 0..b.width {
                if b.is_black(x, y) {
                    out.push((x, y));
                }
            }
        }
        out
    }

    fn rect(x0: u32, y0: u32, x1: u32, y1: u32) -> DotRect {
        DotRect { x0, y0, x1, y1 }
    }

    #[test]
    fn test_blank_is_all_white_with_whole_bytes() {
        let b = MonoBitmap::blank(10, 3);
        assert_eq!((b.width, b.height, b.width_bytes), (10, 3, 2));
        assert_eq!(b.data, vec![0xFF; 6]);
        assert_eq!(b.ink_bbox(), None);
    }

    #[test]
    fn test_rotated_180_reverses_rows_and_dots_and_keeps_padding_right_and_white() {
        // 10 dots wide = 2 bytes per row, 6 padding dots at the right.
        let b = bitmap(10, 2, &[(0, 0), (1, 0), (9, 1)]);
        assert_eq!(b.data, vec![0x3F, 0xFF, 0xFF, 0xBF]);
        let r = b.rotated_180();
        assert_eq!((r.width, r.height, r.width_bytes), (10, 2, 2));
        assert_eq!(black_dots(&r), vec![(0, 0), (8, 1), (9, 1)]);
        // Padding bits (the low 6 bits of each 2nd byte) are still 1 = white.
        assert_eq!(r.data, vec![0x7F, 0xFF, 0xFF, 0x3F]);
        assert_eq!(r.rotated_180(), b);
    }

    #[test]
    fn test_ink_bbox_is_tight_and_ignores_padding() {
        let b = bitmap(10, 3, &[(2, 1), (7, 2)]);
        assert_eq!(b.ink_bbox(), Some(rect(2, 1, 8, 3)));
        assert_eq!(bitmap(10, 3, &[(9, 0)]).ink_bbox(), Some(rect(9, 0, 10, 1)));
        // Real dots 8 and 9 black, the padding bits black too: not ink.
        let padded = MonoBitmap {
            width: 10,
            height: 1,
            width_bytes: 2,
            data: vec![0xFF, 0x00],
        };
        assert_eq!(padded.ink_bbox(), Some(rect(8, 0, 10, 1)));
    }

    #[test]
    fn test_crop_keeps_the_dots_inside_the_rect() {
        let b = bitmap(10, 4, &[(3, 1), (9, 3)]);
        let c = b.crop(rect(3, 1, 9, 4));
        assert_eq!((c.width, c.height, c.width_bytes), (6, 3, 1));
        assert_eq!(black_dots(&c), vec![(0, 0)]);
        assert_eq!(black_dots(&b.crop(rect(3, 1, 10, 4))), vec![(0, 0), (6, 2)]);
        assert_eq!(b.crop(rect(0, 0, 10, 4)), b);
    }

    #[test]
    fn test_dot_rect_contains_and_display() {
        let r = rect(2, 3, 8, 9);
        assert!(r.contains(&r));
        assert!(r.contains(&rect(3, 4, 7, 8)));
        for outside in [
            rect(1, 3, 8, 9),
            rect(2, 2, 8, 9),
            rect(2, 3, 9, 9),
            rect(2, 3, 8, 10),
        ] {
            assert!(!r.contains(&outside), "{outside:?}");
        }
        assert_eq!(r.to_string(), "(2,3)-(8,9)");
    }

    #[test]
    fn test_odoo_png_is_centred_at_bitmap_2_0_and_read_at_3_1() {
        let b = bitmap(576, 879, &[(0, 0)]);
        let p = place_label(&b, &geometry(true, 0, 0));
        assert_eq!(p.bitmap, b.rotated_180());
        assert_eq!(
            p.layout,
            LabelLayout {
                png: (576, 879),
                canvas: (581, 880),
                rotate_180: true,
                bitmap_at: (2, 0),
                ink: Some(rect(3, 1, 4, 2)),
                ink_cropped: false,
            }
        );
        // rotate_180 = false: the PNG as it is (the 0.8.42 orientation),
        // centred the same way.
        let p = place_label(&b, &geometry(false, 0, 0));
        assert_eq!(p.bitmap, b);
        assert_eq!(p.layout.bitmap_at, (2, 0));
        assert_eq!(p.layout.ink, Some(rect(2, 0, 3, 1)));
        assert!(!p.layout.rotate_180);
    }

    #[test]
    fn test_centring_of_other_sizes() {
        // 11 / 10 dots of slack: printer gap 5 / 5, so read at 6 / 5.
        let p = place_label(&bitmap(570, 870, &[]), &geometry(true, 0, 0));
        assert_eq!((p.layout.bitmap_at, p.layout.ink), ((5, 5), None));
        assert_eq!(
            place_label(&bitmap(570, 870, &[]), &geometry(false, 0, 0))
                .layout
                .bitmap_at,
            (5, 5)
        );
        let full = bitmap(581, 880, &[(0, 0)]);
        let p = place_label(&full, &geometry(true, 0, 0));
        assert_eq!(p.layout.bitmap_at, (0, 0));
        assert_eq!(p.layout.ink, Some(rect(0, 0, 1, 1)));
        assert_eq!(p.bitmap, full.rotated_180());
    }

    #[test]
    fn test_offsets_move_the_label_as_read_in_both_modes() {
        let b = bitmap(576, 879, &[(100, 200)]);
        // Rotated: read at (3,1) + (4,-1) = (7,0). Columns 574/575 (paper)
        // fall off the right edge; the printer origin moves the other way.
        let p = place_label(&b, &geometry(true, 4, -1));
        assert_eq!(p.layout.ink, Some(rect(107, 200, 108, 201)));
        assert_eq!(p.layout.bitmap_at, (0, 1));
        assert_eq!(p.bitmap, b.crop(rect(0, 0, 574, 879)).rotated_180());
        assert!(!p.layout.ink_cropped, "only paper was cut");
        // Not rotated: printer = reading, (2,0) + (3,1) = (5,1).
        let p = place_label(&b, &geometry(false, 3, 1));
        assert_eq!(p.layout.bitmap_at, (5, 1));
        assert_eq!(p.layout.ink, Some(rect(105, 201, 106, 202)));
        assert_eq!(p.bitmap, b);
        // Not rotated, pushed 3 left: column 0 falls off the left edge.
        let p = place_label(&b, &geometry(false, -3, 0));
        assert_eq!(p.layout.bitmap_at, (0, 0));
        assert_eq!(p.layout.ink, Some(rect(99, 200, 100, 201)));
        assert_eq!(p.bitmap, b.crop(rect(1, 0, 576, 879)));
    }

    #[test]
    fn test_offset_past_an_edge_flags_lost_ink_on_every_side() {
        // Full-canvas PNG, rotated: read position = the offsets themselves.
        for (dx, dy, lost, kept) in [
            (-20, 0, (10, 100), (30, 100)),
            (5, 0, (578, 100), (570, 100)),
            (0, -5, (300, 2), (300, 8)),
            (0, 5, (300, 877), (300, 870)),
        ] {
            let g = geometry(true, dx, dy);
            let p = place_label(&bitmap(581, 880, &[lost, kept]), &g);
            assert!(p.layout.ink_cropped, "{dx},{dy}: {lost:?} must be lost");
            let (kx, ky) = (
                (i64::from(kept.0) + i64::from(dx)) as u32,
                (i64::from(kept.1) + i64::from(dy)) as u32,
            );
            assert_eq!(
                p.layout.ink,
                Some(rect(kx, ky, kx + 1, ky + 1)),
                "{dx},{dy}"
            );
            assert!(
                p.layout.warnings()[0].starts_with("ink cut off"),
                "{:?}",
                p.layout.warnings()
            );
            let p = place_label(&bitmap(581, 880, &[kept]), &g);
            assert!(!p.layout.ink_cropped, "{dx},{dy}: only paper cut");
        }
    }

    #[test]
    fn test_offset_beyond_the_canvas_prints_nothing_and_says_so() {
        let b = bitmap(576, 879, &[(100, 200)]);
        for (dx, dy) in [(1000, 0), (-1000, 0), (0, 1000), (0, -1000)] {
            let p = place_label(&b, &geometry(true, dx, dy));
            assert!(
                p.bitmap.width == 0 || p.bitmap.height == 0,
                "{dx},{dy}: {:?}",
                (p.bitmap.width, p.bitmap.height)
            );
            assert_eq!(p.layout.ink, None);
            assert!(p.layout.ink_cropped);
            assert!(p.layout.to_string().ends_with(", no ink"));
        }
    }

    #[test]
    fn test_warnings_per_side_and_none_on_the_bartender_margins() {
        let g = geometry(true, 0, 0);
        let warn = |dot: (u32, u32)| place_label(&bitmap(581, 880, &[dot]), &g).layout.warnings();
        assert_eq!(
            warn((12, 100)),
            vec!["ink 12 dots from the left edge, BarTender keeps >= 13"]
        );
        assert_eq!(
            warn((300, 46)),
            vec!["ink 46 dots from the top edge, BarTender keeps >= 47"]
        );
        assert_eq!(
            warn((568, 100)),
            vec!["ink 12 dots from the right edge, BarTender keeps >= 13"]
        );
        assert_eq!(
            warn((300, 863)),
            vec!["ink 16 dots from the bottom edge, BarTender keeps >= 17"]
        );
        for inside in [(13, 100), (300, 47), (567, 100), (300, 862)] {
            assert!(warn(inside).is_empty(), "{inside:?}");
        }
        let edges = place_label(&bitmap(581, 880, &[(13, 47), (567, 862)]), &g).layout;
        assert_eq!(
            edges.ink_margins(),
            Some(Margins {
                left: 13,
                top: 47,
                right: 13,
                bottom: 17
            })
        );
        assert!(edges.warnings().is_empty());
        assert!(
            place_label(&bitmap(581, 880, &[]), &g)
                .layout
                .warnings()
                .is_empty()
        );
    }

    #[test]
    fn test_layout_display_is_one_log_line() {
        let b = bitmap(576, 879, &[(0, 0)]);
        assert_eq!(
            place_label(&b, &geometry(true, 0, 0)).layout.to_string(),
            "png 576x879 -> BITMAP 2,0 rotated 180, ink (3,1)-(4,2) as read, margins L3 T1 R577 B878"
        );
        assert_eq!(
            place_label(&bitmap(576, 879, &[]), &geometry(false, 0, 0))
                .layout
                .to_string(),
            "png 576x879 -> BITMAP 2,0, no ink"
        );
    }
}
