//! Card-only store (`[client.pokladna] fiscal = false`, #102 lane V) on BOTH
//! channels: the LAN API of the tills and the Odoo `next` loop, built from
//! the real `registry_from_config` — no Portos executor, every `fiscal.*`
//! refused `unsupported` before journaling, the till's Portos never asked.

use std::sync::Arc;

use devbridge_client::device_jobs::journal::JobState;

use super::fake::*;
use super::fake_portos::*;
use super::lan::*;

/// Card-only (`[client.pokladna] fiscal = false`, #102 lane V): no Portos
/// executor is built, so a receipt from EITHER channel is refused
/// `unsupported` "nothing ran" before journaling (Odoo: `insert_refused`,
/// one `failed` ack) and the till's Portos is never asked.
#[tokio::test]
async fn test_card_only_refuses_fiscal_jobs_on_both_channels_without_portos() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let journal = journal_in(dir.path());
    let mut cfg = lan_config(&url);
    cfg.fiscal = false;
    cfg.kasy[0].portos_url = Some(purl.clone());
    let registry =
        devbridge_client::device_jobs::registry_from_config(&cfg, dir.path(), &journal).unwrap();
    assert!(registry.kinds().is_empty(), "no Portos, no TIM block");
    fake.lock()
        .unwrap()
        .queue_on(901, "fiscal.cash_register", sale(), 3);
    let lan = start_lan(
        cfg,
        Arc::clone(&journal),
        registry,
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );

    let (status, body) = post(&lan, Some(KEY3), &receipt("card-only-1")).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "unsupported");
    assert!(
        body["error"]["text"]
            .as_str()
            .unwrap()
            .contains("nothing ran"),
        "{body}"
    );
    assert_eq!(lan_state(&journal, "card-only-1"), None, "never journaled");

    eventually("the Odoo receipt refused", || {
        final_state(&fake, 901) == St::Failed
    })
    .await;
    lan.run.stop().await;
    let s = fake.lock().unwrap();
    let acks = s.acks_for(901);
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(acks[0]["error_code"], "unsupported");
    assert!(
        acks[0]["error"].as_str().unwrap().contains("nothing ran"),
        "{acks:?}"
    );
    drop(s);
    assert_eq!(journal.get(901).unwrap().unwrap().state, JobState::Failed);
    assert_eq!(portos.lock().unwrap().post_count(), 0);
    assert!(
        portos.lock().unwrap().lookups.is_empty(),
        "the till's Portos was never asked"
    );
    assert_clean(&fake);
}
