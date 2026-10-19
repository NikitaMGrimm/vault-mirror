mod common;

use std::{collections::HashMap, path::Path, process::Command};

use anyhow::Result;
use common::{provision::TestAccount, *};
use time::OffsetDateTime;
use vault_mirror::{
    app::{self, RunFlags},
    backup::{BackupOptions, backup},
    config::Config,
    node::Node,
    plan,
    snapshot::{self, Restorable},
    store::{Keys, Snapshot, Store},
    vault::{Draft, Scope, Vault},
};

const PERSONAL: Scope = Scope::Personal;

fn keys(dir: &Path) -> Keys {
    Keys::open(dir, "test backup passphrase").unwrap()
}

fn options() -> BackupOptions {
    BackupOptions {
        retention_days: 30,
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

fn latest<'a>(store: &Store<'a>) -> Result<(Restorable, Snapshot<'a>)> {
    let snapshot = store.snapshot(&store.list()?.pop().expect("a snapshot"));
    Ok((snapshot::parse(&snapshot.document()?)?, snapshot))
}

#[tokio::test]
async fn a_backup_holds_the_whole_vault() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source = sandbox
        .open(&sandbox.register(&sandbox.source_url).await?)
        .await?;
    fixtures::seed(&source).await?;
    let dir = tempfile::tempdir()?;
    let keys = keys(dir.path());
    let store = Store::new(dir.path(), "source", &keys);

    let (report, _) = backup(&source, &PERSONAL, &store, &options(), &Capture::default()).await?;
    assert_eq!(report.items, fixtures::samples().len());
    assert_eq!(report.attachments_downloaded, 4);
    assert!(report.snapshot.is_some());

    let (backed_up, snapshot) = latest(&store)?;
    let original = source.load(&PERSONAL).await?;
    // A backup holds what a folder can be restored from; a read also
    // identifies the encrypted name, which a backup has no use for.
    let folders = |list: &[vault_mirror::vault::Folder]| {
        let mut pairs: Vec<(String, String)> = (list.iter())
            .map(|f| (f.id.clone(), f.name.clone()))
            .collect();
        pairs.sort();
        pairs
    };
    assert_eq!(folders(backed_up.folders()), folders(&original.folders));
    assert_eq!(backed_up.items().len(), original.items.len());
    for item in &original.items {
        let copy = (backed_up.items().iter())
            .find(|copy| copy.id == item.id)
            .unwrap();
        assert_eq!(
            Node::Map(copy.content.clone()).normalized(),
            Node::Map(item.content.clone()).normalized(),
            "{}",
            item.name()
        );
        assert_eq!(
            (&copy.folder_id, copy.archived),
            (&item.folder_id, item.archived)
        );
        assert_eq!(copy.attachments.len(), item.attachments.len());
        for attachment in &item.attachments {
            assert_eq!(
                snapshot.attachment(&attachment.id)?,
                source
                    .download_attachment(&PERSONAL, item, attachment)
                    .await?
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn changed_backups_own_their_attachments_and_unchanged_vaults_are_checked() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source = sandbox
        .open(&sandbox.register(&sandbox.source_url).await?)
        .await?;
    fixtures::seed(&source).await?;
    let dir = tempfile::tempdir()?;
    let keys = keys(dir.path());
    let store = Store::new(dir.path(), "source", &keys);
    let quiet = Capture::default();

    backup(&source, &PERSONAL, &store, &options(), &quiet).await?;
    let (unchanged, read) = backup(&source, &PERSONAL, &store, &options(), &quiet).await?;
    assert_eq!(
        (unchanged.snapshot, unchanged.attachments_downloaded),
        (None, 0)
    );
    // The run's read is in the newest snapshot all the same.
    let (newest, _) = latest(&store)?;
    assert_eq!(newest.items().len(), read.read.vault().items.len());

    let vault = source.load(&PERSONAL).await?;
    source
        .upload_attachment(
            &PERSONAL,
            by_name(&vault, "Visa"),
            "card.txt",
            b"front and back",
        )
        .await?;
    let (third, _) = backup(&source, &PERSONAL, &store, &options(), &quiet).await?;
    assert!(third.snapshot.is_some());
    assert_eq!(third.attachments_downloaded, 5);
    assert_eq!(store.list()?.len(), 2);

    // Pruning removes both older independent attachment sets.
    source
        .delete_items(&[by_name(&vault, "Visa").id.clone()])
        .await?;
    let later = BackupOptions {
        retention_days: 30,
        now: OffsetDateTime::now_utc() + time::Duration::days(60),
    };
    let (fourth, _) = backup(&source, &PERSONAL, &store, &later, &quiet).await?;
    assert_eq!(fourth.snapshots_pruned, 2);
    assert_eq!(store.list()?.len(), 1);
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

    // The mirror's own snapshot holds the copies without mirror markers.
    let keys = keys(dir.path());
    let (backed_up, _) = latest(&Store::new(dir.path(), "dest", &keys))?;
    assert_eq!(backed_up.items().len(), count);
    for item in backed_up.items() {
        let fields = item.content["fields"].as_list().unwrap_or_default();
        assert!(!fields.iter().any(plan::is_mirror_marker));
    }
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
    assert!(
        original
            .items
            .iter()
            .all(|item| copied_from(item).is_none())
    );
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

    let refused = app::run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert!(!refused.succeeded());
    assert!(refused.error.unwrap().contains("--allow-overwrite"));
    let overwrite = RunFlags {
        allow_overwrite: true,
        ..RunFlags::default()
    };
    let status = app::run_once(&config, overwrite, &Capture::default()).await;
    assert!(status.succeeded(), "{status:?}");
    assert!(dest.load(&PERSONAL).await?.items[0].attachments.is_empty());
    // The refused run backed it up already.
    assert_eq!(
        refused.dest_backup.as_ref().unwrap().attachments_downloaded,
        1
    );

    let keys = keys(dir.path());
    let (backed_up, snapshot) = latest(&Store::new(dir.path(), "dest", &keys))?;
    let attachment = &backed_up.items()[0].attachments[0];
    assert_eq!(attachment.file_name, "only-copy.txt");
    assert_eq!(
        snapshot.attachment(&attachment.attachment_id)?,
        b"the only copy"
    );
    Ok(())
}

/// Skipping attachments changes what a run copies, not what it backs up.
/// Each side's backup holds the files only that side has.
#[tokio::test]
async fn backups_keep_every_attachment_while_runs_skip_them() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source_account = sandbox.register(&sandbox.source_url).await?;
    let dest_account = sandbox.register(&sandbox.dest_url).await?;
    let source = sandbox.open(&source_account).await?;
    let dest = sandbox.open(&dest_account).await?;
    fixtures::seed(&source).await?;
    let dir = tempfile::tempdir()?;
    let config = config_for(
        &source_account,
        Some(&dest_account),
        dir.path(),
        &[("SKIP_ATTACHMENTS", "true")],
    );

    let first = app::run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert!(first.succeeded(), "{first:?}");
    assert_eq!(first.mirror.as_ref().unwrap().summary.attachments_copied, 0);
    assert!(
        (first.warnings.iter()).any(|warning| warning.contains("SKIP_ATTACHMENTS")),
        "{:?}",
        first.warnings
    );
    assert_eq!(
        first.source_backup.as_ref().unwrap().attachments_downloaded,
        4
    );
    // Bitwarden's server unarchives an item it uploads an attachment to.
    let copy = (dest.load(&PERSONAL).await?.items.into_iter())
        .find(|item| !item.archived)
        .unwrap();
    assert!(copy.attachments.is_empty());
    dest.upload_attachment(&PERSONAL, &copy, "only-copy.txt", b"the only copy")
        .await?;

    let second = app::run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert!(second.succeeded(), "{second:?}");
    assert!(!second.mirror.as_ref().unwrap().changed);
    assert_eq!(
        second.dest_backup.as_ref().unwrap().attachments_downloaded,
        1
    );
    let (_, source_files) = export(dir.path(), "source")?;
    for sample in fixtures::samples() {
        for (name, data) in &sample.attachments {
            assert_eq!(source_files.get(*name), Some(data), "{name}");
        }
    }
    let (_, dest_files) = export(dir.path(), "dest")?;
    assert_eq!(dest_files.len(), 1);
    assert_eq!(dest_files["only-copy.txt"], b"the only copy");
    Ok(())
}

/// Exports the newest snapshot of `role` with the command line. Returns the
/// export and the content of each attachment file by name.
fn export(backups: &Path, role: &str) -> Result<(serde_json::Value, HashMap<String, Vec<u8>>)> {
    let parent = tempfile::tempdir()?;
    let output = parent.path().join("export");
    let exported = Command::new(env!("CARGO_BIN_EXE_vault-mirror"))
        .env_clear()
        .env("BACKUPS_DIR", backups)
        .env("BACKUP_PASSPHRASE", "test backup passphrase")
        .args(["export", "--from", role, "--output"])
        .arg(&output)
        .output()?;
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let document = serde_json::from_slice(&std::fs::read(output.join("export.json"))?)?;
    let restorable = vault_mirror::snapshot::parse(&document)?;
    let files = restorable
        .attachments()
        .map(|attachment| {
            let path = output
                .join("attachments")
                .join(&attachment.attachment_id)
                .join("attachment");
            Ok((attachment.file_name.clone(), std::fs::read(path)?))
        })
        .collect::<Result<_>>()?;
    Ok((document, files))
}

/// An attachment that a backup of `role` cannot download stops every run
/// before it writes anything, and leaves the last complete snapshot of that
/// vault, even past retention, for export.
async fn an_unreadable_attachment_in(role: &str) -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source_account = sandbox.register(&sandbox.source_url).await?;
    let dest_account = sandbox.register(&sandbox.dest_url).await?;
    let source = sandbox.open(&source_account).await?;
    let dest = sandbox.open(&dest_account).await?;
    let item = source
        .create_item(
            &PERSONAL,
            &Draft {
                content: fixtures::named("Visa").content,
                folder_id: None,
                collection_ids: vec![],
            },
        )
        .await?;
    source
        .upload_attachment(&PERSONAL, &item, "card.txt", b"front and back")
        .await?;
    let dir = tempfile::tempdir()?;
    let config = config_for(&source_account, Some(&dest_account), dir.path(), &[]);
    // The second run backs up the destination with its copy.
    for _ in 0..2 {
        let status = app::run_once(&config, RunFlags::default(), &Capture::default()).await;
        assert!(status.succeeded(), "{status:?}");
    }
    let (account, broken_item, stage) = match role {
        "source" => (&source_account, item.id.clone(), "source backup"),
        _ => {
            let copy = dest.load(&PERSONAL).await?.items.remove(0);
            (&dest_account, copy.id, "destination backup")
        }
    };
    let keys = keys(dir.path());
    let store = Store::new(dir.path(), role, &keys);
    let complete = store.list()?;

    let broken =
        provision::register_attachment_without_file(&sandbox.http, account, &broken_item).await?;
    let before = (source.load(&PERSONAL).await?, dest.load(&PERSONAL).await?);
    for _ in 0..2 {
        let refused = app::run_once(&config, RunFlags::default(), &Capture::default()).await;
        assert_eq!(refused.failure_stage.as_deref(), Some(stage));
        let error = refused.error.unwrap();
        assert!(
            error.contains(&broken) && error.contains(&broken_item),
            "{error}"
        );
    }
    let after = (source.load(&PERSONAL).await?, dest.load(&PERSONAL).await?);
    assert_eq!(after, before, "a refused run changed a vault");

    // Long past retention, a refused backup still prunes nothing.
    let later = BackupOptions {
        retention_days: 1,
        now: OffsetDateTime::now_utc() + time::Duration::days(60),
    };
    let side = if role == "source" { &source } else { &dest };
    for _ in 0..2 {
        let refused = backup(side, &PERSONAL, &store, &later, &Capture::default()).await;
        assert!(refused.is_err());
    }
    assert_eq!(store.list()?, complete);
    let (document, files) = export(dir.path(), role)?;
    assert_eq!(document["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(document["items"][0]["card"]["number"], "4111111111111111");
    assert_eq!(
        files,
        HashMap::from([("card.txt".to_string(), b"front and back".to_vec())])
    );
    Ok(())
}

#[tokio::test]
async fn an_unreadable_source_attachment_stops_runs_and_the_last_snapshot_stays() -> Result<()> {
    an_unreadable_attachment_in("source").await
}

#[tokio::test]
async fn an_unreadable_destination_attachment_stops_runs_and_the_last_snapshot_stays() -> Result<()>
{
    an_unreadable_attachment_in("dest").await
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
    let store = Store::new(dir.path(), "source", &keys);
    backup(&source, &PERSONAL, &store, &options(), &Capture::default()).await?;
    let document = store.snapshot(&store.list()?.pop().unwrap()).document()?;
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
