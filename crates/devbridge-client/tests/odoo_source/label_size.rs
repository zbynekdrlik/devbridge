//! The batch's own label roll (#108): the 50 × 30 mm crate labels print on
//! the same TSC as the 72.7 × 110.1 mm bakery labels. A batch that names its
//! roll prints on it, a batch without one on the `[client.odoo]` roll
//! (byte-identical to before), an invalid size sends nothing, and the
//! heartbeat's `paper_size` follows the last document sent.

use serde_json::{Value, json};

use devbridge_client::odoo_source::PollOutcome;
use devbridge_client::odoo_source::tspl::LabelGeometry;

use super::fake::*;

const CRATES: LabelGeometry = LabelGeometry {
    width_mm: 50.0,
    height_mm: 30.0,
    ..SPISSKA
};

fn crate_size() -> Value {
    json!({"label_width_mm": 50, "label_height_mm": 30})
}

fn batch(batch_id: i64, printed: usize, rejected: usize, reacked: usize) -> PollOutcome {
    PollOutcome::Batch {
        batch_id,
        printed,
        rejected,
        reacked,
    }
}

fn starts_with(doc: &[u8], head: &str) -> bool {
    doc.starts_with(head.as_bytes())
}

fn contains(doc: &[u8], needle: &str) -> bool {
    doc.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// The job-history paper size of the document of `batch_id`.
fn job_paper_size(rig: &Rig, batch_id: i64) -> String {
    let prefix = format!("odoo-{batch_id}-");
    let jobs = rig.queue.get_all_jobs().unwrap();
    let job = jobs
        .iter()
        .find(|j| j.job_id.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no job for batch {batch_id}"));
    job.paper_size.clone()
}

#[tokio::test]
async fn test_crate_batch_then_bakery_batch_each_print_on_their_own_roll() {
    let rig = Rig::new("roll-switch").await;
    let source = rig.source();
    assert_eq!(
        source.heartbeat_payload().await.paper_size,
        "72.7x110.1 mm",
        "before any batch: the config roll"
    );

    // A crate batch on the 50 x 30 roll: Odoo's 399 x 240 PNG.
    let crate_png = png_b64(399, 240, 12);
    rig.set_batch(
        21,
        crate_size(),
        vec![line(2101, 10, 3, &crate_png, "code_label")],
    );
    assert_eq!(source.poll_once().await.unwrap(), batch(21, 1, 0, 0));
    let docs = rig.spooler.docs();
    assert_eq!(docs.len(), 1);
    assert_eq!(
        docs[0],
        expected_doc_on(&CRATES, &[(crate_png.as_str(), 3)])
    );
    assert!(starts_with(
        &docs[0],
        "SIZE 50 mm, 30 mm\r\nDIRECTION 0,0\r\nREFERENCE 0,0\r\nOFFSET 0 mm\r\nSET TEAR ON\r\nCLS\r\nBITMAP 0,0,50,240,0,"
    ));
    assert!(contains(&docs[0], "\r\nPRINT 1,3\r\n"));
    assert_eq!(
        rig.acks()[0],
        json!({"line_id": 2101, "printed_qty": 3, "error": null})
    );
    assert_eq!(job_paper_size(&rig, 21), "50x30 mm");
    assert_eq!(source.heartbeat_payload().await.paper_size, "50x30 mm");

    // The next bakery batch names no size: the config roll, byte for byte
    // the document a batch got before #108.
    let bakery_png = png_b64(576, 880, 7);
    rig.set_batch(
        22,
        json!({}),
        vec![line(2201, 10, 2, &bakery_png, "product")],
    );
    assert_eq!(source.poll_once().await.unwrap(), batch(22, 1, 0, 0));
    let docs = rig.spooler.docs();
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[1], expected_doc(&[(bakery_png.as_str(), 2)]));
    assert!(starts_with(
        &docs[1],
        "SIZE 72.7 mm, 110.1 mm\r\nDIRECTION 0,0\r\nREFERENCE 0,0\r\nOFFSET 0 mm\r\nSET TEAR ON\r\nCLS\r\nBITMAP 2,0,72,880,0,"
    ));
    assert_eq!(job_paper_size(&rig, 22), "72.7x110.1 mm");
    assert_eq!(source.heartbeat_payload().await.paper_size, "72.7x110.1 mm");

    // Odoo naming the bakery roll itself (false for an absent dpi) is the
    // same document.
    rig.set_batch(
        23,
        json!({"label_width_mm": 72.7, "label_height_mm": 110.1, "dpi": false}),
        vec![line(2301, 10, 2, &bakery_png, "product")],
    );
    assert_eq!(source.poll_once().await.unwrap(), batch(23, 1, 0, 0));
    assert_eq!(rig.spooler.docs()[2], docs[1]);

    // A format whose size is unset in Odoo sends its fields.Float as 0.0:
    // no size, the config roll — never a refused bakery batch.
    rig.set_batch(
        24,
        json!({"label_width_mm": 0.0, "label_height_mm": 0.0, "dpi": 0}),
        vec![line(2401, 10, 2, &bakery_png, "product")],
    );
    assert_eq!(source.poll_once().await.unwrap(), batch(24, 1, 0, 0));
    assert_eq!(rig.spooler.docs()[3], docs[1]);
}

#[tokio::test]
async fn test_invalid_batch_size_rejects_every_line_and_a_fixed_size_prints_them() {
    let rig = Rig::new("bad-size").await;
    let source = rig.source();
    let crate_png = png_b64(399, 240, 12);
    let lines = vec![
        line(3101, 10, 2, &crate_png, "code_label"),
        line(3102, 20, 1, &crate_png, "code_label"),
    ];
    rig.set_batch(
        31,
        json!({"label_width_mm": 120, "label_height_mm": 30}),
        lines.clone(),
    );
    assert_eq!(source.poll_once().await.unwrap(), batch(31, 0, 2, 0));
    assert!(rig.spooler.docs().is_empty(), "nothing is sent");
    let refused = "size: label_width_mm 120 mm outside 10-108 mm";
    assert_eq!(
        rig.acks(),
        vec![
            json!({"line_id": 3101, "printed_qty": 0, "error": refused}),
            json!({"line_id": 3102, "printed_qty": 0, "error": refused}),
        ]
    );
    assert_eq!(
        source.status().last_result,
        "nothing printable (2 lines rejected)"
    );
    assert_eq!(
        source.heartbeat_payload().await.paper_size,
        "72.7x110.1 mm",
        "a refused batch does not change the roll"
    );

    // Odoo fixes the size and offers the same lines again: never sent, so
    // processed anew (the ledger's "rejected before send" rule).
    rig.set_batch(31, crate_size(), lines);
    assert_eq!(source.poll_once().await.unwrap(), batch(31, 2, 0, 0));
    let docs = rig.spooler.docs();
    assert_eq!(docs.len(), 1);
    assert_eq!(
        docs[0],
        expected_doc_on(&CRATES, &[(crate_png.as_str(), 2), (crate_png.as_str(), 1)])
    );
    let acks = rig.acks();
    assert_eq!(
        acks[2..],
        [
            json!({"line_id": 3101, "printed_qty": 2, "error": null}),
            json!({"line_id": 3102, "printed_qty": 1, "error": null}),
        ]
    );
}

#[tokio::test]
async fn test_half_given_or_non_number_size_is_refused_per_batch() {
    let rig = Rig::new("half-size").await;
    let source = rig.source();
    let crate_png = png_b64(399, 240, 12);
    for (batch_id, fields, refused) in [
        (
            41,
            json!({"label_width_mm": 50}),
            "size: label_width_mm without label_height_mm",
        ),
        (
            42,
            json!({"label_height_mm": 30, "label_width_mm": null}),
            "size: label_height_mm without label_width_mm",
        ),
        (
            46,
            json!({"label_width_mm": 50, "label_height_mm": 0.0}),
            "size: label_width_mm without label_height_mm",
        ),
        (
            43,
            json!({"dpi": 203}),
            "size: dpi without label_width_mm / label_height_mm",
        ),
        (
            44,
            json!({"label_width_mm": "50", "label_height_mm": 30}),
            "size: label_width_mm \"50\" is not a number",
        ),
        (
            45,
            json!({"label_width_mm": 50, "label_height_mm": 30, "dpi": 600}),
            "size: dpi 600 is not 203 or 300",
        ),
    ] {
        let line_id = batch_id * 100 + 1;
        rig.set_batch(
            batch_id,
            fields,
            vec![line(line_id, 10, 1, &crate_png, "code_label")],
        );
        assert_eq!(
            source.poll_once().await.unwrap(),
            batch(batch_id, 0, 1, 0),
            "batch {batch_id}"
        );
        assert_eq!(
            rig.acks().last().cloned().unwrap(),
            json!({"line_id": line_id, "printed_qty": 0, "error": refused})
        );
    }
    assert!(rig.spooler.docs().is_empty());
}

#[tokio::test]
async fn test_png_larger_than_the_batch_roll_is_a_size_reject() {
    let rig = Rig::new("png-vs-roll").await;
    let source = rig.source();
    // A bakery-sized PNG in a crate batch, next to a crate label that fits.
    let crate_png = png_b64(399, 240, 12);
    rig.set_batch(
        51,
        crate_size(),
        vec![
            line(5101, 10, 1, &png_b64(576, 880, 7), "product"),
            line(5102, 20, 1, &png_b64(401, 240, 1), "code_label"),
            line(5103, 30, 4, &crate_png, "code_label"),
        ],
    );
    assert_eq!(source.poll_once().await.unwrap(), batch(51, 1, 2, 0));
    let docs = rig.spooler.docs();
    assert_eq!(docs.len(), 1);
    assert_eq!(
        docs[0],
        expected_doc_on(&CRATES, &[(crate_png.as_str(), 4)])
    );
    let acks = rig.acks();
    let ack = |id: i64| acks.iter().find(|a| a["line_id"] == id).cloned().unwrap();
    assert_eq!(
        ack(5101),
        json!({"line_id": 5101, "printed_qty": 0, "error": "size"})
    );
    assert_eq!(
        ack(5102),
        json!({"line_id": 5102, "printed_qty": 0, "error": "size"})
    );
    assert_eq!(
        ack(5103),
        json!({"line_id": 5103, "printed_qty": 4, "error": null})
    );
}

#[tokio::test]
async fn test_batch_dpi_sets_the_canvas() {
    let rig = Rig::new("dpi-300").await;
    let source = rig.source();
    let crate_png = png_b64(399, 240, 12);
    rig.set_batch(
        61,
        json!({"label_width_mm": 50, "label_height_mm": 30, "dpi": 300}),
        vec![line(6101, 10, 1, &crate_png, "code_label")],
    );
    assert_eq!(source.poll_once().await.unwrap(), batch(61, 1, 0, 0));
    let docs = rig.spooler.docs();
    // 50 x 30 mm at 300 dpi = 591 x 354 dots: the PNG centred at 96,57.
    let at_300 = LabelGeometry { dpi: 300, ..CRATES };
    assert_eq!(at_300.max_dots(), (591, 354));
    assert_eq!(
        docs[0],
        expected_doc_on(&at_300, &[(crate_png.as_str(), 1)])
    );
    assert!(starts_with(&docs[0], "SIZE 50 mm, 30 mm\r\n"));
    assert!(contains(&docs[0], "CLS\r\nBITMAP 96,57,50,240,0,"));
}
