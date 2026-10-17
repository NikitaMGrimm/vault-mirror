//! Deleting the personal folders runs made, once nothing needs them.

mod common;

use anyhow::Result;
use common::*;
use reqwest::Method;
use serde_json::json;
use vault_mirror::{
    api::{ServerUrls, Session},
    mirror::{MirrorError, MirrorOptions},
    node::Node,
    plan::Safety,
    store::FolderReceipts,
    vault::{Account, Draft, Scope, Vault},
};

const PERSONAL: Scope = Scope::Personal;

fn options() -> MirrorOptions {
    MirrorOptions::default()
}

fn overwrite() -> MirrorOptions {
    MirrorOptions {
        safety: Safety {
            allow_overwrite: true,
            ..Safety::default()
        },
        ..options()
    }
}

fn names(vault: &Vault) -> Vec<&str> {
    let mut names: Vec<&str> = vault.folders.iter().map(|f| f.name.as_str()).collect();
    names.sort_unstable();
    names
}

fn folder_id(vault: &Vault, name: &str) -> String {
    let mut found = vault.folders.iter().filter(|f| f.name == name);
    let folder = found.next().unwrap_or_else(|| panic!("no folder {name:?}"));
    assert!(found.next().is_none(), "two folders {name:?}");
    folder.id.clone()
}

/// Moves every live item in the folder `from` to the folder `to`.
async fn move_items(account: &Account, from: &str, to: Option<&str>) -> Result<usize> {
    let vault = account.load(&PERSONAL).await?;
    let (from, to) = (folder_id(&vault, from), to.map(|to| folder_id(&vault, to)));
    let items = (vault.items.iter())
        .filter(|item| !item.in_trash && item.folder_id.as_deref() == Some(from.as_str()));
    let mut moved = 0;
    for item in items {
        let draft = Draft {
            content: item.content.clone(),
            folder_id: to.clone(),
            collection_ids: vec![],
        };
        account.update_item(&PERSONAL, item, &draft).await?;
        moved += 1;
    }
    Ok(moved)
}

/// An item of the destination's own, not a copy.
async fn own_item(account: &Account, folder: &str) -> Result<()> {
    let mut content = fixtures::named("Bare login without folder").content;
    content.insert("name".into(), Node::secret("Own item"));
    let draft = Draft {
        content,
        folder_id: Some(folder.to_string()),
        collection_ids: vec![],
    };
    account.create_item(&PERSONAL, &draft).await?;
    Ok(())
}

/// Renames a folder to the name it has, as a client does that stores the
/// name encrypted anew.
async fn rename_to_itself(
    sandbox: &Sandbox,
    account: &provision::TestAccount,
    opened: &Account,
    id: &str,
) -> Result<()> {
    let vault = opened.load(&PERSONAL).await?;
    let name = &vault.folders.iter().find(|f| f.id == id).unwrap().name;
    let encrypted_anew = opened.create_folder(name).await?;
    let session = Session::login(
        sandbox.http.clone(),
        ServerUrls::from_server(&account.server)?,
        account.credentials.clone(),
    )
    .await?;
    let sync = session.get("/sync?excludeDomains=true").await?;
    let ciphertext = (sync["folders"].as_array().unwrap().iter())
        .find(|f| f["id"] == encrypted_anew.id.as_str())
        .map(|f| f["name"].clone())
        .unwrap();
    (session.send_idempotent(
        Method::PUT,
        &format!("/folders/{id}"),
        Some(&json!({ "name": ciphertext })),
    ))
    .await?;
    opened.delete_folder(&encrypted_anew.id).await?;
    assert_eq!(names(&opened.load(&PERSONAL).await?), names(&vault));
    Ok(())
}

/// A folder the source no longer has is deleted by the run that empties it,
/// or by the next run once only that is left, and check shows both.
#[tokio::test]
async fn folders_a_run_made_go_once_nothing_needs_them() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
    run(&source, &dest, &options()).await?;
    let samples = fixtures::samples().len();

    let vault = source.load(&PERSONAL).await?;
    source.delete_folder(&folder_id(&vault, "Empty")).await?;
    let before = dest.load(&PERSONAL).await?;
    let preview = check(&source, &dest, &options()).await?;
    assert!(!preview.changed);
    assert_eq!(preview.summary.folders_deleted, 1);
    assert_eq!(dest.load(&PERSONAL).await?, before);
    let outcome = run(&source, &dest, &options()).await?;
    assert!(outcome.changed);
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    assert_eq!(outcome.summary.folders_deleted, 1);
    assert_eq!(outcome.summary.items_unchanged, samples);
    assert_eq!(
        names(&dest.load(&PERSONAL).await?),
        ["Persönlich ✈", "Work", "Work/Infra"]
    );
    let again = run(&source, &dest, &options()).await?;
    assert!(!again.changed);
    assert_eq!(again.summary.folders_deleted, 0);

    // Renamed in the source: its copies move, then the old folder goes.
    source.create_folder("Infra").await?;
    let moved = move_items(&source, "Work/Infra", Some("Infra")).await?;
    let vault = source.load(&PERSONAL).await?;
    source
        .delete_folder(&folder_id(&vault, "Work/Infra"))
        .await?;
    let preview = check(&source, &dest, &options()).await?;
    assert_eq!(
        (
            preview.summary.items_updated,
            preview.summary.folders_created,
            preview.summary.folders_deleted
        ),
        (moved, 1, 1)
    );
    let outcome = run(&source, &dest, &options()).await?;
    assert!(outcome.changed);
    assert_eq!(outcome.summary, preview.summary);
    let mirrored = dest.load(&PERSONAL).await?;
    assert_eq!(names(&mirrored), ["Infra", "Persönlich ✈", "Work"]);
    let infra = folder_id(&mirrored, "Infra");
    assert_eq!(
        (mirrored.items.iter())
            .filter(|item| item.folder_id.as_deref() == Some(infra.as_str()))
            .count(),
        moved
    );
    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

/// Folders that were there before, that hold something, even in the trash,
/// or that were renamed in the destination, stay.
#[tokio::test]
async fn folders_runs_did_not_make_or_that_are_in_use_stay() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source = sandbox
        .open(&sandbox.register(&sandbox.source_url).await?)
        .await?;
    let dest_account = sandbox.register(&sandbox.dest_url).await?;
    let dest = sandbox.open(&dest_account).await?;
    fixtures::seed(&source).await?;
    dest.create_folder("Work").await?;
    let first = run(&source, &dest, &options()).await?;
    assert_eq!(first.summary.folders_created, fixtures::FOLDERS.len() - 1);
    let mirrored = dest.load(&PERSONAL).await?;
    own_item(&dest, &folder_id(&mirrored, "Empty")).await?;
    rename_to_itself(
        &sandbox,
        &dest_account,
        &dest,
        &folder_id(&mirrored, "Work/Infra"),
    )
    .await?;

    let vault = source.load(&PERSONAL).await?;
    move_items(&source, "Work", None).await?;
    move_items(&source, "Work/Infra", None).await?;
    let private: Vec<String> = (vault.items.iter())
        .filter(|item| item.folder_id == Some(folder_id(&vault, "Persönlich ✈")))
        .map(|item| item.id.clone())
        .collect();
    source.delete_items(&private).await?;
    for name in fixtures::FOLDERS {
        source.delete_folder(&folder_id(&vault, name)).await?;
    }
    let outcome = run(&source, &dest, &options()).await?;
    assert_eq!(outcome.summary.items_deleted, private.len());
    assert_eq!(outcome.summary.folders_deleted, 0);
    let all = ["Empty", "Persönlich ✈", "Work", "Work/Infra"];
    assert_eq!(names(&dest.load(&PERSONAL).await?), all);

    // Once the trash is emptied, the folder of the deleted copies goes.
    let trashed: Vec<String> = (dest.load(&PERSONAL).await?.items.iter())
        .filter(|item| item.in_trash)
        .map(|item| item.id.clone())
        .collect();
    assert_eq!(trashed.len(), private.len());
    dest.delete_items(&trashed).await?;
    let outcome = run(&source, &dest, &options()).await?;
    assert!(outcome.changed);
    assert_eq!(outcome.summary.folders_deleted, 1);
    assert_eq!(
        names(&dest.load(&PERSONAL).await?),
        ["Empty", "Work", "Work/Infra"]
    );

    // Emptied by hand, the folder a run made goes too.
    let mirrored = dest.load(&PERSONAL).await?;
    let own: Vec<String> = (mirrored.items.iter())
        .filter(|item| item.name() == "Own item")
        .map(|item| item.id.clone())
        .collect();
    dest.delete_items(&own).await?;
    let outcome = run(&source, &dest, &options()).await?;
    assert_eq!(outcome.summary.folders_deleted, 1);
    assert_eq!(names(&dest.load(&PERSONAL).await?), ["Work", "Work/Infra"]);
    Ok(())
}

/// Without the record of the folders runs made, no folder is deleted.
#[tokio::test]
async fn without_its_record_a_run_deletes_no_folder() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
    let elsewhere = tempfile::tempdir()?;
    let reads = (
        protect(&source, &PERSONAL).await?,
        protect(&dest, &PERSONAL).await?,
    );
    run_recorded(
        ((&source, PERSONAL), (&dest, PERSONAL)),
        (&reads.0, &reads.1),
        &options(),
        &FolderReceipts::new(elsewhere.path()),
    )
    .await?;
    assert!(elsewhere.path().join("folders").is_dir());

    let vault = source.load(&PERSONAL).await?;
    source.delete_folder(&folder_id(&vault, "Empty")).await?;
    let outcome = run(&source, &dest, &options()).await?;
    assert!(!outcome.changed);
    assert_eq!(outcome.summary.folders_deleted, 0);
    assert!(names(&dest.load(&PERSONAL).await?).contains(&"Empty"));
    Ok(())
}

/// A folder planned for deletion that something is put in after the backup
/// stays.
#[tokio::test]
async fn a_folder_filled_after_the_backup_stays() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
    run(&source, &dest, &options()).await?;
    let vault = source.load(&PERSONAL).await?;
    source.delete_folder(&folder_id(&vault, "Empty")).await?;

    let source_read = protect(&source, &PERSONAL).await?;
    let dest_read = protect(&dest, &PERSONAL).await?;
    own_item(&dest, &folder_id(&dest_read.vault, "Empty")).await?;
    let outcome = run_protected(
        &source,
        PERSONAL,
        &dest,
        PERSONAL,
        (&source_read, &dest_read),
        &options(),
    )
    .await?;
    assert!(!outcome.changed);
    assert_eq!(outcome.summary.folders_deleted, 0);
    assert!(names(&dest.load(&PERSONAL).await?).contains(&"Empty"));
    Ok(())
}

/// A folder created by a run that then failed is still recognized as made by
/// a run, and a refused run deletes no folder.
#[tokio::test]
async fn failed_and_refused_runs_keep_the_record_and_the_folders() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
    run(&source, &dest, &options()).await?;
    let later = source.create_folder("Later").await?;
    let originals = source.load(&PERSONAL).await?;
    source
        .delete_items(&[by_name(&originals, "Identity").id.clone()])
        .await?;

    // The copy to delete is edited after the backup, so the run stops after
    // it created the folder.
    let source_read = protect(&source, &PERSONAL).await?;
    let dest_read = protect(&dest, &PERSONAL).await?;
    let copy = by_name(&dest_read.vault, "Identity");
    let mut edited = Draft {
        content: copy.content.clone(),
        folder_id: copy.folder_id.clone(),
        collection_ids: vec![],
    };
    edited
        .content
        .insert("notes".into(), Node::secret("edited meanwhile"));
    dest.update_item(&PERSONAL, copy, &edited).await?;
    let error = run_protected(
        &source,
        PERSONAL,
        &dest,
        PERSONAL,
        (&source_read, &dest_read),
        &options(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast::<MirrorError>()?.stage,
        "deleting obsolete items"
    );
    assert!(names(&dest.load(&PERSONAL).await?).contains(&"Later"));

    source.delete_folder(&later.id).await?;
    let error = run(&source, &dest, &options()).await.unwrap_err();
    let error = error.downcast::<MirrorError>()?;
    assert!(format!("{:#}", error.source).contains("--allow-overwrite"));
    assert_eq!(error.summary.unwrap().folders_deleted, 0);
    assert!(names(&dest.load(&PERSONAL).await?).contains(&"Later"));

    let outcome = run(&source, &dest, &overwrite()).await?;
    assert_eq!(outcome.summary.folders_deleted, 1);
    assert!(!names(&dest.load(&PERSONAL).await?).contains(&"Later"));
    Ok(())
}

/// A folder made for a first run that imports its items in bulk is known
/// as made by the run all the same.
#[tokio::test]
async fn a_folder_made_for_a_bulk_import_can_go() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    let bulk = source.create_folder("Bulk").await?;
    for index in 0..11 {
        let draft = Draft {
            content: [
                ("type".to_string(), Node::Number(2.into())),
                ("name".to_string(), Node::secret(format!("Note {index}"))),
                (
                    "secureNote".to_string(),
                    Node::Map([("type".to_string(), Node::Number(0.into()))].into()),
                ),
            ]
            .into(),
            folder_id: Some(bulk.id.clone()),
            collection_ids: vec![],
        };
        source.create_item(&PERSONAL, &draft).await?;
    }
    let first = run(&source, &dest, &options()).await?;
    assert_eq!(first.summary.items_created, 11);

    source.create_folder("Bulk 2").await?;
    assert_eq!(move_items(&source, "Bulk", Some("Bulk 2")).await?, 11);
    source.delete_folder(&bulk.id).await?;
    let outcome = run(&source, &dest, &options()).await?;
    assert_eq!(
        (
            outcome.summary.items_updated,
            outcome.summary.folders_created,
            outcome.summary.folders_deleted
        ),
        (11, 1, 1)
    );
    assert_eq!(names(&dest.load(&PERSONAL).await?), ["Bulk 2"]);
    Ok(())
}

/// Organization items can be in an account's folders, unseen in its
/// individual vault, so an account in an organization keeps its folders.
#[tokio::test]
async fn an_account_in_an_organization_keeps_its_folders() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source = sandbox
        .open(&sandbox.register(&sandbox.source_url).await?)
        .await?;
    let dest_account = sandbox.register(&sandbox.dest_url).await?;
    create_organization(&sandbox, &dest_account, "Folder Test Org").await?;
    let dest = sandbox.open(&dest_account).await?;
    fixtures::seed(&source).await?;
    run(&source, &dest, &options()).await?;
    let vault = source.load(&PERSONAL).await?;
    source.delete_folder(&folder_id(&vault, "Empty")).await?;

    for outcome in [
        check(&source, &dest, &options()).await?,
        run(&source, &dest, &options()).await?,
    ] {
        assert!(!outcome.changed);
        assert_eq!(outcome.summary.folders_deleted, 0);
        assert!(
            (outcome.warnings.iter())
                .any(|w| w.starts_with("Kept 1 folder") && w.contains("organization")),
            "{:?}",
            outcome.warnings
        );
    }
    assert!(names(&dest.load(&PERSONAL).await?).contains(&"Empty"));
    Ok(())
}
