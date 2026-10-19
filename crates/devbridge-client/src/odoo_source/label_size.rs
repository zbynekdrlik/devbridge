//! The label roll of each Odoo batch (#108).
//!
//! The crate QR labels are 50 × 30 mm on the SAME TSC ML241P as the
//! 72.7 × 110.1 mm bakery labels — the store swaps the roll, and Odoo's roll
//! interlock lets only one roll print at a time. So `/food/print/next` may
//! carry the roll of the batch's label format (`label_width_mm` /
//! `label_height_mm`, optional `dpi`). A batch with it is printed on that
//! roll (`SIZE`, the PNG size check, the centring); a batch without it on
//! the `[client.odoo]` roll, byte-identical to the output before #108.
//! Rotation and offsets always come from `[client.odoo]`.
//!
//! The size is the PAIR width + height. One of them alone, a `dpi` without
//! the pair, a value that is not a number, or one outside what the printer
//! takes ([`WIDTH_MM`], [`HEIGHT_MM`], [`DPIS`]) rejects every printable line
//! of the batch with a `size: …` ack — nothing is sent, and the ledger
//! processes such a line anew once Odoo offers it again (rejected before
//! send). Never guessed, never clamped.

use std::fmt;
use std::ops::RangeInclusive;

use super::rpc::{BatchLabelSize, BatchNumber};
use super::tspl::LabelGeometry;

/// Label width a batch may name: the TSC ML241P prints 108 mm (4.25") wide;
/// under 10 mm is no label.
pub const WIDTH_MM: RangeInclusive<f64> = 10.0..=108.0;
/// Label length (feed direction) a batch may name.
pub const HEIGHT_MM: RangeInclusive<f64> = 10.0..=300.0;
/// Resolutions a batch may name (203 = the ML241P head, 300 = its 300-dpi
/// sibling).
pub const DPIS: [u32; 2] = [203, 300];

/// Where a batch's label size came from — logged with every batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeSource {
    /// The batch named its roll.
    Batch,
    /// No size in the batch: the `[client.odoo]` roll.
    Config,
}

impl fmt::Display for SizeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Batch => "batch",
            Self::Config => "config",
        })
    }
}

/// The geometry one batch prints with, and where its size came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatchGeometry {
    pub geometry: LabelGeometry,
    pub source: SizeSource,
}

/// A label roll in mm: what `SIZE` names, the heartbeat `paper_size` and the
/// job history's paper size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Roll {
    pub width_mm: f64,
    pub height_mm: f64,
}

impl Roll {
    pub fn of(geometry: &LabelGeometry) -> Self {
        Self {
            width_mm: geometry.width_mm,
            height_mm: geometry.height_mm,
        }
    }
}

impl fmt::Display for Roll {
    /// `72.7x110.1 mm`, `50x30 mm` — the shortest exact decimals, like `SIZE`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{} mm", self.width_mm, self.height_mm)
    }
}

/// Why a batch's size is refused. Every printable line of the batch is acked
/// with [`SizeError::ack_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeError(String);

impl SizeError {
    /// `size: <reason>` — the `size` ack of #90 with the reason after it.
    pub fn ack_text(&self) -> String {
        format!("size: {}", self.0)
    }
}

impl fmt::Display for SizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.ack_text())
    }
}

/// The geometry of a batch: its own size when it names one (validated), the
/// `[client.odoo]` geometry when it names none. `dpi` defaults to the
/// config's; rotation and offsets are always the config's.
pub fn geometry_for(
    config: &LabelGeometry,
    size: &BatchLabelSize,
) -> Result<BatchGeometry, SizeError> {
    let width = given("label_width_mm", &size.width_mm)?;
    let height = given("label_height_mm", &size.height_mm)?;
    let dpi = given("dpi", &size.dpi)?;
    let (width_mm, height_mm) = match (width, height, dpi) {
        (None, None, None) => {
            return Ok(BatchGeometry {
                geometry: *config,
                source: SizeSource::Config,
            });
        }
        (Some(w), Some(h), _) => (w, h),
        (Some(_), None, _) => return Err(refuse("label_width_mm without label_height_mm")),
        (None, Some(_), _) => return Err(refuse("label_height_mm without label_width_mm")),
        (None, None, Some(_)) => {
            return Err(refuse("dpi without label_width_mm / label_height_mm"));
        }
    };
    within("label_width_mm", width_mm, &WIDTH_MM)?;
    within("label_height_mm", height_mm, &HEIGHT_MM)?;
    let dpi = match dpi {
        Some(d) => supported_dpi(d)?,
        None => config.dpi,
    };
    Ok(BatchGeometry {
        geometry: LabelGeometry {
            width_mm,
            height_mm,
            dpi,
            ..*config
        },
        source: SizeSource::Batch,
    })
}

fn refuse(reason: &str) -> SizeError {
    SizeError(reason.to_string())
}

/// `Absent` → `None`, a number → `Some`; anything else refuses the batch.
fn given(name: &str, value: &BatchNumber) -> Result<Option<f64>, SizeError> {
    match value {
        BatchNumber::Absent => Ok(None),
        BatchNumber::Number(n) => Ok(Some(*n)),
        BatchNumber::NotANumber(raw) => Err(SizeError(format!("{name} {raw} is not a number"))),
    }
}

fn within(name: &str, mm: f64, range: &RangeInclusive<f64>) -> Result<(), SizeError> {
    if range.contains(&mm) {
        return Ok(());
    }
    Err(SizeError(format!(
        "{name} {mm} mm outside {}-{} mm",
        range.start(),
        range.end()
    )))
}

fn supported_dpi(dpi: f64) -> Result<u32, SizeError> {
    DPIS.into_iter()
        .find(|d| f64::from(*d) == dpi)
        .ok_or_else(|| SizeError(format!("dpi {dpi} is not 203 or 300")))
}

/// The WARN text when a document goes out on another roll than the one
/// before it: the store must have swapped the roll, and the TSC needs a gap
/// calibration after a swap. `None` = the same roll.
pub fn roll_change(previous: Roll, next: Roll) -> Option<String> {
    (previous != next).then(|| {
        format!(
            "roll changed {previous} -> {next}: the printer must hold the {next} roll (calibrate the TSC label gap after a roll swap)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPISSKA: LabelGeometry = LabelGeometry {
        width_mm: 72.7,
        height_mm: 110.1,
        dpi: 203,
        rotate_180: true,
        x_offset_dots: 4,
        y_offset_dots: -2,
    };

    fn size(width: BatchNumber, height: BatchNumber, dpi: BatchNumber) -> BatchLabelSize {
        BatchLabelSize {
            width_mm: width,
            height_mm: height,
            dpi,
        }
    }

    fn n(v: f64) -> BatchNumber {
        BatchNumber::Number(v)
    }

    const NO: BatchNumber = BatchNumber::Absent;

    fn refused(s: &BatchLabelSize) -> String {
        geometry_for(&SPISSKA, s).unwrap_err().ack_text()
    }

    #[test]
    fn test_no_size_is_the_config_geometry() {
        let got = geometry_for(&SPISSKA, &BatchLabelSize::default()).unwrap();
        assert_eq!(
            got,
            BatchGeometry {
                geometry: SPISSKA,
                source: SizeSource::Config
            }
        );
    }

    #[test]
    fn test_batch_size_wins_and_keeps_rotation_offsets_and_config_dpi() {
        let got = geometry_for(&SPISSKA, &size(n(50.0), n(30.0), NO)).unwrap();
        assert_eq!(got.source, SizeSource::Batch);
        assert_eq!(
            got.geometry,
            LabelGeometry {
                width_mm: 50.0,
                height_mm: 30.0,
                dpi: 203,
                rotate_180: true,
                x_offset_dots: 4,
                y_offset_dots: -2,
            }
        );
        assert_eq!(got.geometry.max_dots(), (400, 240));
        let at_300 = geometry_for(&SPISSKA, &size(n(50.0), n(30.0), n(300.0))).unwrap();
        assert_eq!(at_300.geometry.dpi, 300);
        assert_eq!(at_300.geometry.max_dots(), (591, 354));
        let at_203 = geometry_for(&SPISSKA, &size(n(72.7), n(110.1), n(203.0))).unwrap();
        assert_eq!(
            at_203,
            BatchGeometry {
                geometry: SPISSKA,
                source: SizeSource::Batch
            }
        );
    }

    #[test]
    fn test_bounds_are_inclusive_on_both_edges() {
        for (w, h) in [(10.0, 10.0), (108.0, 300.0), (10.0, 300.0), (108.0, 10.0)] {
            let got = geometry_for(&SPISSKA, &size(n(w), n(h), NO)).unwrap();
            assert_eq!(
                (got.geometry.width_mm, got.geometry.height_mm),
                (w, h),
                "{w}x{h}"
            );
        }
        assert_eq!(
            refused(&size(n(9.99), n(30.0), NO)),
            "size: label_width_mm 9.99 mm outside 10-108 mm"
        );
        assert_eq!(
            refused(&size(n(108.01), n(30.0), NO)),
            "size: label_width_mm 108.01 mm outside 10-108 mm"
        );
        assert_eq!(
            refused(&size(n(50.0), n(9.5), NO)),
            "size: label_height_mm 9.5 mm outside 10-300 mm"
        );
        assert_eq!(
            refused(&size(n(50.0), n(300.5), NO)),
            "size: label_height_mm 300.5 mm outside 10-300 mm"
        );
        assert_eq!(
            refused(&size(n(-50.0), n(30.0), NO)),
            "size: label_width_mm -50 mm outside 10-108 mm"
        );
        // The width is checked before the height.
        assert_eq!(
            refused(&size(n(0.0), n(0.0), NO)),
            "size: label_width_mm 0 mm outside 10-108 mm"
        );
    }

    #[test]
    fn test_dpi_must_be_203_or_300() {
        for bad in [0.0, 202.0, 204.0, 299.0, 301.0, 600.0, 203.5] {
            assert_eq!(
                refused(&size(n(50.0), n(30.0), n(bad))),
                format!("size: dpi {bad} is not 203 or 300")
            );
        }
        // The size is checked before the dpi.
        assert_eq!(
            refused(&size(n(5.0), n(30.0), n(600.0))),
            "size: label_width_mm 5 mm outside 10-108 mm"
        );
    }

    #[test]
    fn test_half_given_size_is_refused() {
        assert_eq!(
            refused(&size(n(50.0), NO, NO)),
            "size: label_width_mm without label_height_mm"
        );
        assert_eq!(
            refused(&size(NO, n(30.0), NO)),
            "size: label_height_mm without label_width_mm"
        );
        assert_eq!(
            refused(&size(n(50.0), NO, n(203.0))),
            "size: label_width_mm without label_height_mm"
        );
        assert_eq!(
            refused(&size(NO, NO, n(203.0))),
            "size: dpi without label_width_mm / label_height_mm"
        );
    }

    #[test]
    fn test_a_value_that_is_not_a_number_is_refused() {
        let text = |raw: &str| BatchNumber::NotANumber(raw.to_string());
        assert_eq!(
            refused(&size(text("\"50\""), n(30.0), NO)),
            "size: label_width_mm \"50\" is not a number"
        );
        assert_eq!(
            refused(&size(n(50.0), text("true"), NO)),
            "size: label_height_mm true is not a number"
        );
        assert_eq!(
            refused(&size(n(50.0), n(30.0), text("\"203\""))),
            "size: dpi \"203\" is not a number"
        );
        assert_eq!(
            refused(&size(NO, NO, text("[]"))),
            "size: dpi [] is not a number"
        );
    }

    #[test]
    fn test_size_error_display_is_the_ack_text() {
        let e = geometry_for(&SPISSKA, &size(n(50.0), NO, NO)).unwrap_err();
        assert_eq!(e.to_string(), e.ack_text());
        assert!(e.ack_text().starts_with("size: "));
    }

    #[test]
    fn test_source_and_roll_display() {
        assert_eq!(SizeSource::Batch.to_string(), "batch");
        assert_eq!(SizeSource::Config.to_string(), "config");
        assert_eq!(Roll::of(&SPISSKA).to_string(), "72.7x110.1 mm");
        let crate_roll = Roll {
            width_mm: 50.0,
            height_mm: 30.0,
        };
        assert_eq!(crate_roll.to_string(), "50x30 mm");
        assert_eq!(
            Roll::of(&LabelGeometry {
                width_mm: 50.0,
                height_mm: 30.0,
                ..SPISSKA
            }),
            crate_roll
        );
    }

    #[test]
    fn test_roll_change_only_when_the_roll_differs() {
        let bakery = Roll::of(&SPISSKA);
        let crates = Roll {
            width_mm: 50.0,
            height_mm: 30.0,
        };
        assert_eq!(roll_change(bakery, bakery), None);
        assert_eq!(
            roll_change(bakery, crates).as_deref(),
            Some(
                "roll changed 72.7x110.1 mm -> 50x30 mm: the printer must hold the 50x30 mm roll (calibrate the TSC label gap after a roll swap)"
            )
        );
        assert!(
            roll_change(crates, bakery)
                .unwrap()
                .starts_with("roll changed 50x30 mm -> 72.7x110.1 mm")
        );
        let longer = Roll {
            height_mm: 40.0,
            ..crates
        };
        assert!(roll_change(crates, longer).is_some(), "height alone");
        let wider = Roll {
            width_mm: 60.0,
            ..crates
        };
        assert!(roll_change(crates, wider).is_some(), "width alone");
    }
}
