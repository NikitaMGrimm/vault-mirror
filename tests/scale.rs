//! How a run scales with the size of a vault. Slow, so it only runs on request:
//!
//! ```sh
//! cargo test --release --test scale -- --ignored --nocapture
//! ```
//!
//! `VAULT_MIRROR_SCALE_ITEMS` sets the number of items (default 2000).
mod common;

use std::time::Instant;

use anyhow::Result;
use common::*;
use futures_util::{StreamExt, TryStreamExt, stream};
use vault_mirror::{
    mirror::MirrorOptions,
    node::Node,
    vault::{Draft, Scope},
};

const PERSONAL: Scope = Scope::Personal;

#[tokio::test]
#[ignore = "slow; run on request"]
async fn a_large_vault_is_mirrored_and_then_checked_quickly() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let count: usize = std::env::var("VAULT_MIRROR_SCALE_ITEMS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2000);
    let (source, dest) = sandbox.pair().await?;

    // The samples without their attachments, repeated under distinct names.
    let samples = fixtures::samples();
    let started = Instant::now();
    stream::iter(0..count)
        .map(|index| {
            let mut content = samples[index % samples.len()].content.clone();
            content.insert("name".into(), Node::secret(format!("Item {index:05}")));
            let source = &source;
            async move {
                let draft = Draft {
                    content,
                    folder_id: None,
                    collection_ids: vec![],
                };
                source.create_item(&PERSONAL, &draft).await
            }
        })
        .buffer_unordered(16)
        .try_collect::<Vec<_>>()
        .await?;
    eprintln!("seeded {count} items in {:.1?}", started.elapsed());

    let options = MirrorOptions::default();
    let started = Instant::now();
    let first = run(&source, &dest, &options).await?;
    eprintln!("first run: {:.1?}", started.elapsed());
    assert_eq!(first.summary.items_created, count);

    let started = Instant::now();
    let second = run(&source, &dest, &options).await?;
    let unchanged = started.elapsed();
    eprintln!("unchanged run: {unchanged:.1?}");
    assert!(!second.changed);

    // Edit one item in a hundred.
    let vault = source.load(&PERSONAL).await?;
    let edits = count.div_ceil(100);
    for item in vault.items.iter().take(edits) {
        let mut content = item.content.clone();
        content.insert("notes".into(), Node::secret("edited"));
        let draft = Draft {
            content,
            folder_id: item.folder_id.clone(),
            collection_ids: vec![],
        };
        source.update_item(&PERSONAL, item, &draft).await?;
    }
    let started = Instant::now();
    let third = run(&source, &dest, &options).await?;
    eprintln!("run after {edits} edits: {:.1?}", started.elapsed());
    assert_eq!(third.summary.items_updated, edits);
    assert_eq!(third.summary.items_unchanged, count - edits);
    Ok(())
}
