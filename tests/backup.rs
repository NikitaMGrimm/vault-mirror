mod common;

use std::{collections::HashMap, path::Path, process::Command};

use anyhow::Result;
use common::{provision::TestAccount, *};
use time::OffsetDateTime;
use vault_mirror::{
    app::{self, RunFlags},
    backup::{BackupOptions, backup},
    config::Config,
    mirror::Endpoint,
    node::Node,
    plan::{Mode, mark_copy, marker_of},
    restore::{RestoreOptions, restore},
    snapshot,
    store::{Keys, Protection, Retention, Store},
    vault::{Account, Draft, Scope, Vault},
};

const PERSONAL: Scope = Scope::Personal;

fn keys(dir: &Path) -> Keys {
    Keys::open(
        dir,
        &Protection::Passphrase("test backup passphrase".into()),
    )
    .unwrap()
}

fn options(attachments: bool) -> BackupOptions<'static> {
    BackupOptions {
        attachments,
        retention: None,
        now: OffsetDateTime::now_utc(),
    }
}

/// Comparable content, folder name, archive state and attachment names.
type Summary = (Option<serde_json::Value>, Option<String>, bool, Vec<String>);

/// Content of every live item by name, in the form that is equal across servers.
fn contents(vault: &Vault) -> HashMap<String, Summary> {
    vault
        .items
        .iter()
        .filter(|item| !item.in_trash)
        .map(|item| {
            let mut files: Vec<String> = item
                .attachments
                .iter()
                .map(|a| a.file_name.clone())
                .collect();
            files.sort();
            (
                item.name().to_string(),
                (
                    Node::Map(item.content.clone()).normalized(),
                    folder_of(vault, item),
                    item.archived,
                    files,
                ),
            )
        })
        .collect()
}

async fn restored_copy(sandbox: &Sandbox, store: &Store<'_>) -> Result<Account> {
    let target = sandbox
        .open(&sandbox.register(&sandbox.dest_url).await?)
        .await?;
    let latest = store.list()?.pop().expect("a snapshot");
    let restorable = snapshot::parse(&store.read_snapshot(&latest.path)?)?;
    let endpoint = Endpoint {
        account: &target,
        scope: PERSONAL,
    };
    let options = RestoreOptions {
        allow_non_empty: false,
        concurrency: 4,
    };
    restore(
        &endpoint,
        &restorable,
        Some(store),
        &options,
        &Capture::default(),
    )
    .await?;
    Ok(target)
}

#[tokio::test]
async fn a_backup_restores_into_an_identical_vault() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source = sandbox
        .open(&sandbox.register(&sandbox.source_url).await?)
        .await?;
    fixtures::seed(&source).await?;
    // Enough items that the restore imports them in bulk.
    seed_extra(&source, 12).await?;
    let dir = tempfile::tempdir()?;
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys)?;

    let report = backup(
        &source,
        &PERSONAL,
        &store,
        &options(true),
        &Capture::default(),
    )
    .await?;
    assert_eq!(report.items, fixtures::samples().len() + 12);
    assert_eq!(report.attachments_downloaded, 4);
    assert!(report.snapshot.is_some());

    let target = restored_copy(&sandbox, &store).await?;
    let original = source.load(&PERSONAL).await?;
    let copy = target.load(&PERSONAL).await?;
    assert_eq!(contents(&copy), contents(&original));
    assert_eq!(copy.folders.len(), original.folders.len());
    for sample in fixtures::samples() {
        let item = by_name(&copy, sample.content["name"].as_str().unwrap());
        for (file, data) in &sample.attachments {
            let attachment = item
                .attachments
                .iter()
                .find(|a| &a.file_name == file)
                .unwrap();
            assert_eq!(
                &target
                    .download_attachment(&PERSONAL, item, attachment)
                    .await?,
                data
            );
        }
    }

    // Restoring again would duplicate everything, so it has to be asked for.
    let latest = store.list()?.pop().unwrap();
    let restorable = snapshot::parse(&store.read_snapshot(&latest.path)?)?;
    let endpoint = Endpoint {
        account: &target,
        scope: PERSONAL,
    };
    let refused = RestoreOptions {
        allow_non_empty: false,
        concurrency: 4,
    };
    let error = restore(
        &endpoint,
        &restorable,
        Some(&store),
        &refused,
        &Capture::default(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("--allow-non-empty"), "{error}");
    Ok(())
}

#[tokio::test]
async fn later_backups_fetch_only_new_attachments_and_skip_unchanged_vaults() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source = sandbox
        .open(&sandbox.register(&sandbox.source_url).await?)
        .await?;
    fixtures::seed(&source).await?;
    let dir = tempfile::tempdir()?;
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys)?;
    let quiet = Capture::default();

    backup(&source, &PERSONAL, &store, &options(true), &quiet).await?;
    let unchanged = backup(&source, &PERSONAL, &store, &options(true), &quiet).await?;
    assert_eq!(
        (unchanged.snapshot, unchanged.attachments_downloaded),
        (None, 0)
    );

    let vault = source.load(&PERSONAL).await?;
    source
        .upload_attachment(
            &PERSONAL,
            by_name(&vault, "Visa"),
            "card.txt",
            b"front and back",
        )
        .await?;
    let third = backup(&source, &PERSONAL, &store, &options(true), &quiet).await?;
    assert!(third.snapshot.is_some());
    assert_eq!(third.attachments_downloaded, 1);
    assert_eq!(store.list()?.len(), 2);

    // Deleting the item makes its attachment unreferenced once the older
    // snapshots are gone.
    source
        .delete_items(&[by_name(&vault, "Visa").id.clone()])
        .await?;
    let later = BackupOptions {
        attachments: true,
        retention: Some(&Retention {
            days: 30,
            keep_at_least: 1,
        }),
        now: OffsetDateTime::now_utc() + time::Duration::days(60),
    };
    let fourth = backup(&source, &PERSONAL, &store, &later, &quiet).await?;
    assert_eq!(fourth.pruned.snapshots_removed, 2);
    assert_eq!(fourth.pruned.attachments_removed, 1);
    assert_eq!(store.list()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_backup_without_attachment_content_restores_the_items_and_says_what_is_missing()
-> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source = sandbox
        .open(&sandbox.register(&sandbox.source_url).await?)
        .await?;
    fixtures::seed(&source).await?;
    let dir = tempfile::tempdir()?;
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys)?;
    let report = backup(
        &source,
        &PERSONAL,
        &store,
        &options(false),
        &Capture::default(),
    )
    .await?;
    assert_eq!(report.attachments_downloaded, 0);

    let target = sandbox
        .open(&sandbox.register(&sandbox.dest_url).await?)
        .await?;
    let latest = store.list()?.pop().unwrap();
    let restorable = snapshot::parse(&store.read_snapshot(&latest.path)?)?;
    let endpoint = Endpoint {
        account: &target,
        scope: PERSONAL,
    };
    let capture = Capture::default();
    let options = RestoreOptions {
        allow_non_empty: false,
        concurrency: 4,
    };
    let report = restore(&endpoint, &restorable, Some(&store), &options, &capture).await?;
    assert_eq!(report.items, fixtures::samples().len());
    assert_eq!(
        (report.attachments_restored, report.attachments_missing),
        (0, 4)
    );
    assert!(
        capture
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("warn:") && line.contains("4 attachment"))
    );
    Ok(())
}

fn config_for(
    source: &TestAccount,
    dest: Option<&TestAccount>,
    backups: &Path,
    extra: &[(&str, &str)],
) -> Config {
    let mut env: HashMap<String, String> = HashMap::new();
    let mut vault = |prefix: &str, account: &TestAccount| {
        env.insert(format!("{prefix}_SERVER"), account.server.clone());
        env.insert(
            format!("{prefix}_CLIENT_ID"),
            account.credentials.client_id.clone(),
        );
        env.insert(
            format!("{prefix}_CLIENT_SECRET"),
            account.credentials.client_secret.clone(),
        );
        env.insert(format!("{prefix}_PASSWORD"), account.password.clone());
    };
    vault("SOURCE", source);
    if let Some(dest) = dest {
        vault("DEST", dest);
    }
    env.insert("BACKUPS_DIR".into(), backups.display().to_string());
    env.insert("BACKUP_PASSPHRASE".into(), "test backup passphrase".into());
    for (name, value) in extra {
        env.insert(name.to_string(), value.to_string());
    }
    Config::from_env(&env).unwrap()
}

#[tokio::test]
async fn a_full_run_backs_up_both_sides_mirrors_and_records_the_result() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source_account = sandbox.register(&sandbox.source_url).await?;
    let dest_account = sandbox.register(&sandbox.dest_url).await?;
    let source = sandbox.open(&source_account).await?;
    fixtures::seed(&source).await?;
    let dir = tempfile::tempdir()?;
    let config = config_for(&source_account, Some(&dest_account), dir.path(), &[]);
    let count = fixtures::samples().len();

    let first = app::run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert!(first.succeeded(), "{first:?}");
    assert_eq!(app::read_status(dir.path())?, first);
    let mirror = first.mirror.as_ref().unwrap();
    assert!(mirror.changed);
    assert_eq!(
        (
            mirror.summary.items_created,
            mirror.summary.attachments_copied
        ),
        (count, 4)
    );
    assert_eq!(
        first.source_backup.as_ref().unwrap().attachments_downloaded,
        4
    );
    assert_eq!(first.dest_backup.as_ref().unwrap().items, 0);

    let second = app::run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert!(second.succeeded(), "{second:?}");
    assert!(!second.mirror.as_ref().unwrap().changed);
    assert_eq!(second.source_backup.as_ref().unwrap().snapshot, None);
    // The destination changed in the first run, so it gets a new snapshot.
    assert_eq!(second.dest_backup.as_ref().unwrap().items, count);

    // The same device id is presented on every run.
    let state = std::fs::read_to_string(dir.path().join("state.json"))?;
    app::run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("state.json"))?,
        state
    );

    // No secret is stored in the clear anywhere in the backup directory.
    for file in walk(dir.path()) {
        let raw = std::fs::read(&file)?;
        for secret in [
            "correct horse battery staple",
            "4111111111111111",
            "hunter2",
            &source_account.password,
        ] {
            assert!(
                !raw.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "{} leaks a secret",
                file.display()
            );
        }
    }

    // The mirror's own snapshot restores without mirror markers.
    let keys = keys(dir.path());
    let dest_store = Store::open(dir.path(), "dest", &keys)?;
    let latest = dest_store.list()?.pop().unwrap();
    let restorable = snapshot::parse(&dest_store.read_snapshot(&latest.path)?)?;
    let target = sandbox
        .open(&sandbox.register(&sandbox.dest_url).await?)
        .await?;
    let endpoint = Endpoint {
        account: &target,
        scope: PERSONAL,
    };
    let options = RestoreOptions {
        allow_non_empty: false,
        concurrency: 4,
    };
    restore(
        &endpoint,
        &restorable,
        Some(&dest_store),
        &options,
        &Capture::default(),
    )
    .await?;
    let restored = target.load(&PERSONAL).await?;
    assert_eq!(restored.items.len(), count);
    assert!(restored.items.iter().all(|item| marker_of(item).is_none()));
    Ok(())
}

/// While the source was down, the destination was used instead. A pull
/// brings its changes back into the source and leaves no marker there.
#[tokio::test]
async fn a_pull_brings_back_what_changed_in_the_destination() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source_account = sandbox.register(&sandbox.source_url).await?;
    let dest_account = sandbox.register(&sandbox.dest_url).await?;
    let source = sandbox.open(&source_account).await?;
    let dest = sandbox.open(&dest_account).await?;
    fixtures::seed(&source).await?;
    let dir = tempfile::tempdir()?;
    let config = config_for(&source_account, Some(&dest_account), dir.path(), &[]);
    let capture = Capture::default();
    let run = |flags| app::run_once(&config, flags, &capture);
    assert!(run(RunFlags::default()).await.succeeded());

    let vault = dest.load(&PERSONAL).await?;
    let visa = by_name(&vault, "Visa");
    let mut edited = Draft {
        content: visa.content.clone(),
        folder_id: visa.folder_id.clone(),
        collection_ids: vec![],
    };
    let Some(Node::Map(card)) = edited.content.get_mut("card") else {
        panic!()
    };
    card.insert("number".into(), Node::secret("5555555555554444"));
    dest.update_item(&PERSONAL, visa, &edited).await?;
    dest.delete_items(&[by_name(&vault, "Identity").id.clone()])
        .await?;
    // Changed only in the source: the pull leaves it as it is.
    let originals = source.load(&PERSONAL).await?;
    let passkey = by_name(&originals, "Passkey login");
    let mut noted = Draft {
        content: passkey.content.clone(),
        folder_id: passkey.folder_id.clone(),
        collection_ids: vec![],
    };
    noted
        .content
        .insert("notes".into(), Node::secret("edited in the source"));
    source.update_item(&PERSONAL, passkey, &noted).await?;
    let mut added = fixtures::named("Bare login without folder").content;
    added.insert("name".into(), Node::secret("Added during the outage"));
    dest.create_item(
        &PERSONAL,
        &Draft {
            content: added,
            folder_id: None,
            collection_ids: vec![],
        },
    )
    .await?;

    let dry = Capture::default();
    let previewed = app::run_once(
        &config,
        RunFlags {
            pull: true,
            dry_run: true,
            ..RunFlags::default()
        },
        &dry,
    )
    .await;
    assert!(previewed.succeeded(), "{previewed:?}");
    assert!(
        dry.0.lock().unwrap().contains(
            &"info: Would pull 1 new item and 1 changed item into the source".to_string()
        )
    );
    assert!(
        !dry.0
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("Pulling"))
    );

    let pulled = run(RunFlags {
        pull: true,
        ..RunFlags::default()
    })
    .await;
    assert!(pulled.succeeded(), "{pulled:?}");
    let summary = &pulled.pulled.as_ref().unwrap().summary;
    assert_eq!((summary.items_created, summary.items_updated), (1, 1));

    let back = source.load(&PERSONAL).await?;
    let card = by_name(&back, "Visa").content["card"].as_map().unwrap();
    assert_eq!(card["number"].as_str(), Some("5555555555554444"));
    by_name(&back, "Added during the outage");
    assert_eq!(
        by_name(&back, "Passkey login").content["notes"].as_str(),
        Some("edited in the source")
    );
    // Deleted in the destination, but the pull deletes nothing.
    by_name(&back, "Identity");
    assert!(back.items.iter().all(|item| marker_of(item).is_none()));
    let mirrored = dest.load(&PERSONAL).await?;
    assert_eq!(mirrored.items.len(), fixtures::samples().len() + 1);
    assert_eq!(
        by_name(&mirrored, "Passkey login").content["notes"].as_str(),
        Some("edited in the source")
    );
    assert!(mirrored.items.iter().all(|item| marker_of(item).is_some()));

    let again = run(RunFlags::default()).await;
    assert!(again.succeeded(), "{again:?}");
    assert!(!again.mirror.as_ref().unwrap().changed);
    Ok(())
}

/// An item the source server would accept but never show is left out of the
/// pull, instead of being added again on every pull.
#[tokio::test]
async fn a_pull_leaves_out_items_the_source_would_not_show() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source_account = sandbox.register(&sandbox.source_url).await?;
    let dest_account = sandbox.register(&sandbox.dest_url).await?;
    let source = sandbox.open(&source_account).await?;
    let dest = sandbox.open(&dest_account).await?;
    if !dest.server_traits().await?.hidden_item_types.is_empty() {
        eprintln!("skipping: the destination server does not show passports");
        return Ok(());
    }
    let passport: std::collections::BTreeMap<String, Node> = [
        ("surname", "Mustermann"),
        ("givenName", "Erika"),
        ("passportNumber", "C01X00T47"),
        ("dateOfBirth", "1964-08-12"),
        ("expirationDate", "2031-10-31"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), Node::secret(value)))
    .collect();
    let draft = Draft {
        content: [
            ("type".to_string(), Node::Number(8.into())),
            ("name".to_string(), Node::secret("Passport")),
            ("passport".to_string(), Node::Map(passport)),
        ]
        .into(),
        folder_id: None,
        collection_ids: vec![],
    };
    if let Err(error) = dest.create_item(&PERSONAL, &draft).await {
        eprintln!("skipping: the destination server rejects passports: {error:#}");
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let config = config_for(&source_account, Some(&dest_account), dir.path(), &[]);
    let hidden = !source.server_traits().await?.hidden_item_types.is_empty();
    let flags = RunFlags {
        pull: true,
        ..RunFlags::default()
    };
    let first = app::run_once(&config, flags, &Capture::default()).await;
    assert!(first.succeeded(), "{first:?}");
    let created = first.pulled.as_ref().unwrap().summary.items_created;
    assert_eq!(created, usize::from(!hidden));
    assert_eq!(
        first.warnings.iter().any(|w| w.contains("out of the pull")),
        hidden,
        "{:?}",
        first.warnings
    );
    let second = app::run_once(&config, flags, &Capture::default()).await;
    assert!(second.succeeded(), "{second:?}");
    assert_eq!(second.pulled.as_ref().unwrap().summary.items_created, 0);
    Ok(())
}

/// A pull that stopped after adding an item to the source, but before copying
/// its attachments, finishes it when run again. The destination keeps its file,
/// and the source keeps an edit made in between.
#[tokio::test]
async fn a_pull_run_again_finishes_an_item_it_added_halfway() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source_account = sandbox.register(&sandbox.source_url).await?;
    let dest_account = sandbox.register(&sandbox.dest_url).await?;
    let source = sandbox.open(&source_account).await?;
    let dest = sandbox.open(&dest_account).await?;
    fixtures::seed(&source).await?;
    let dir = tempfile::tempdir()?;
    let config = config_for(&source_account, Some(&dest_account), dir.path(), &[]);
    let capture = Capture::default();
    let run = |flags| app::run_once(&config, flags, &capture);
    assert!(run(RunFlags::default()).await.succeeded());

    let mut content = fixtures::named("Bare login without folder").content;
    content.insert("name".into(), Node::secret("Added during the outage"));
    let added = dest
        .create_item(
            &PERSONAL,
            &Draft {
                content,
                folder_id: None,
                collection_ids: vec![],
            },
        )
        .await?;
    dest.upload_attachment(&PERSONAL, &added, "outage.txt", b"made during the outage")
        .await?;
    // What the stopped pull left: the item, marked as still waiting for its
    // files, and edited in the source since.
    let mut edited = added.clone();
    edited
        .content
        .insert("notes".into(), Node::secret("edited in the source"));
    source
        .create_item(
            &PERSONAL,
            &Draft {
                content: mark_copy(&edited, &added, None, &[], Mode::Personal, None),
                folder_id: None,
                collection_ids: vec![],
            },
        )
        .await?;

    let pulled = run(RunFlags {
        pull: true,
        ..RunFlags::default()
    })
    .await;
    assert!(pulled.succeeded(), "{pulled:?}");
    assert_eq!(pulled.pulled.as_ref().unwrap().summary.items_created, 1);
    for (account, vault) in [
        (&source, source.load(&PERSONAL).await?),
        (&dest, dest.load(&PERSONAL).await?),
    ] {
        let named: Vec<_> = (vault.items.iter())
            .filter(|item| item.name() == "Added during the outage")
            .collect();
        assert_eq!(named.len(), 1);
        assert_eq!(
            named[0].content["notes"].as_str(),
            Some("edited in the source")
        );
        let [attachment] = named[0].attachments.as_slice() else {
            panic!("expected one attachment");
        };
        let data = account
            .download_attachment(&PERSONAL, named[0], attachment)
            .await?;
        assert_eq!(data, b"made during the outage");
    }
    let back = source.load(&PERSONAL).await?;
    assert!(back.items.iter().all(|item| marker_of(item).is_none()));
    Ok(())
}

#[tokio::test]
async fn a_failed_run_is_recorded_with_its_stage_and_changes_nothing() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source_account = sandbox.register(&sandbox.source_url).await?;
    let dest_account = sandbox.register(&sandbox.dest_url).await?;
    let source = sandbox.open(&source_account).await?;
    fixtures::seed(&source).await?;
    let dir = tempfile::tempdir()?;
    let config = config_for(&source_account, Some(&dest_account), dir.path(), &[]);
    assert!(
        app::run_once(&config, RunFlags::default(), &Capture::default())
            .await
            .succeeded()
    );

    // Swapped: the mirror is now the source, and the original the destination.
    let swapped = config_for(&dest_account, Some(&source_account), dir.path(), &[]);
    let refused = app::run_once(&swapped, RunFlags::default(), &Capture::default()).await;
    assert!(!refused.succeeded());
    assert_eq!(refused.failure_stage.as_deref(), Some("planning"));
    assert!(refused.error.as_deref().unwrap().contains("swapped"));
    let original = source.load(&PERSONAL).await?;
    assert_eq!(original.items.len(), fixtures::samples().len());
    assert!(original.items.iter().all(|item| marker_of(item).is_none()));
    // The destination was still backed up before the refusal.
    assert_eq!(
        refused.dest_backup.as_ref().unwrap().items,
        fixtures::samples().len()
    );

    let mut wrong = dest_account.clone();
    wrong.credentials.client_secret = "not-the-secret".into();
    let config = config_for(&source_account, Some(&wrong), dir.path(), &[]);
    let failed = app::run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert_eq!(failed.failure_stage.as_deref(), Some("destination login"));
    assert!(!failed.error.as_deref().unwrap().contains("not-the-secret"));

    Ok(())
}

#[tokio::test]
async fn destination_attachments_are_backed_up_before_the_mirror_deletes_them() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source_account = sandbox.register(&sandbox.source_url).await?;
    let dest_account = sandbox.register(&sandbox.dest_url).await?;
    let source = sandbox.open(&source_account).await?;
    let dest = sandbox.open(&dest_account).await?;
    source
        .create_item(
            &PERSONAL,
            &Draft {
                content: fixtures::named("Visa").content,
                folder_id: None,
                collection_ids: vec![],
            },
        )
        .await?;
    let dir = tempfile::tempdir()?;
    let config = config_for(&source_account, Some(&dest_account), dir.path(), &[]);
    assert!(
        app::run_once(&config, RunFlags::default(), &Capture::default())
            .await
            .succeeded()
    );
    // A file only the destination's copy has, so no source backup holds it.
    let copy = dest.load(&PERSONAL).await?.items.remove(0);
    dest.upload_attachment(&PERSONAL, &copy, "only-copy.txt", b"the only copy")
        .await?;

    let status = app::run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert!(status.succeeded(), "{status:?}");
    assert!(dest.load(&PERSONAL).await?.items[0].attachments.is_empty());
    assert_eq!(
        status.dest_backup.as_ref().unwrap().attachments_downloaded,
        1
    );

    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "dest", &keys)?;
    let target = restored_copy(&sandbox, &store).await?;
    let restored = target.load(&PERSONAL).await?;
    let item = &restored.items[0];
    let attachment = &item.attachments[0];
    assert_eq!(attachment.file_name, "only-copy.txt");
    assert_eq!(
        target
            .download_attachment(&PERSONAL, item, attachment)
            .await?,
        b"the only copy"
    );
    Ok(())
}

#[tokio::test]
async fn a_setup_without_a_destination_only_backs_up() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source_account = sandbox.register(&sandbox.source_url).await?;
    fixtures::seed(&sandbox.open(&source_account).await?).await?;
    let dir = tempfile::tempdir()?;
    let config = config_for(&source_account, None, dir.path(), &[]);
    let status = app::run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert!(status.succeeded(), "{status:?}");
    assert!(status.mirror.is_none() && status.dest_backup.is_none());
    assert_eq!(
        status.source_backup.unwrap().items,
        fixtures::samples().len()
    );
    Ok(())
}

/// The export inside a snapshot has to be importable by Bitwarden's own
/// client. Runs when `VAULT_MIRROR_BW` points at the official CLI.
#[tokio::test]
async fn the_official_cli_imports_a_snapshot() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let Some(bw) = std::env::var_os("VAULT_MIRROR_BW") else {
        eprintln!("skipping: VAULT_MIRROR_BW is not set");
        return Ok(());
    };
    let source = sandbox
        .open(&sandbox.register(&sandbox.source_url).await?)
        .await?;
    fixtures::seed(&source).await?;
    let dir = tempfile::tempdir()?;
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys)?;
    backup(
        &source,
        &PERSONAL,
        &store,
        &options(true),
        &Capture::default(),
    )
    .await?;
    let document = store.read_snapshot(&store.list()?.pop().unwrap().path)?;
    let export = dir.path().join("export.json");
    std::fs::write(&export, serde_json::to_vec(&document)?)?;

    let target_account = sandbox.register(&sandbox.dest_url).await?;
    let data = dir.path().join("bw-data");
    let run = |args: &[&str], extra: &[(&str, &str)]| {
        let output = Command::new(&bw)
            .args(args)
            .env("BITWARDENCLI_APPDATA_DIR", &data)
            .env("BW_CLIENTID", &target_account.credentials.client_id)
            .env("BW_CLIENTSECRET", &target_account.credentials.client_secret)
            .env("BW_PASSWORD", &target_account.password)
            .env("NODE_EXTRA_CA_CERTS", &sandbox.ca_file)
            .envs(extra.iter().copied())
            .output()
            .expect("running the Bitwarden CLI");
        assert!(
            output.status.success(),
            "bw {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    // The CLI refuses servers that are not served over HTTPS.
    run(&["config", "server", &sandbox.dest_tls_url], &[]);
    run(&["login", "--apikey"], &[]);
    let session = run(&["unlock", "--passwordenv", "BW_PASSWORD", "--raw"], &[]);
    run(
        &["import", "bitwardenjson", export.to_str().unwrap()],
        &[("BW_SESSION", &session)],
    );

    let imported = sandbox.open(&target_account).await?.load(&PERSONAL).await?;
    let original = source.load(&PERSONAL).await?;
    // The import format carries no attachments and no archive state.
    let strip = |vault: &Vault| -> HashMap<_, _> {
        contents(vault)
            .into_iter()
            .map(|(name, (content, folder, _, _))| (name, (content, folder)))
            .collect()
    };
    let (imported, original) = (strip(&imported), strip(&original));
    for (name, expected) in &original {
        let found = imported
            .get(name)
            .unwrap_or_else(|| panic!("{name} was not imported"));
        assert_eq!(found.1, expected.1, "{name}: folder");
        let paths = vault_mirror::plan::differing_paths(
            expected.0.as_ref().unwrap_or(&serde_json::Value::Null),
            found.0.as_ref().unwrap_or(&serde_json::Value::Null),
        );
        // The import format has no field for when a password last changed.
        let paths: Vec<_> = paths
            .into_iter()
            .filter(|path| {
                path != "content.login.passwordRevisionDate" && path != "login.passwordRevisionDate"
            })
            .collect();
        assert!(paths.is_empty(), "{name} differs after import at {paths:?}");
    }
    assert_eq!(imported.len(), original.len());
    Ok(())
}

fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}
