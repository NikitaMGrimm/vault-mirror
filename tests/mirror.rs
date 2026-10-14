mod common;

use anyhow::Result;
use common::*;
use vault_mirror::{
    fixtures,
    mirror::{Filter, MirrorOptions},
    node::Node,
    plan::{MARKER_FIELD, Safety, marker_of},
    vault::{Account, Draft, Scope},
};

const PERSONAL: Scope = Scope::Personal;

fn options() -> MirrorOptions {
    MirrorOptions::default()
}

async fn attachment_bytes(
    account: &Account,
    vault: &vault_mirror::vault::Vault,
    item: &str,
    file: &str,
) -> Result<Vec<u8>> {
    let item = by_name(vault, item);
    let attachment = item
        .attachments
        .iter()
        .find(|a| a.file_name == file)
        .expect("attachment");
    account
        .download_attachment(&PERSONAL, item, attachment)
        .await
}

#[tokio::test]
async fn a_vault_is_mirrored_completely_and_then_left_alone() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    seed(&source).await?;
    let samples = fixtures::samples();

    let first = run(&source, &dest, &options()).await?;
    assert!(first.changed);
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    assert_eq!(first.summary.items_created, samples.len());
    assert_eq!(first.summary.folders_created, fixtures::FOLDERS.len());
    assert_eq!(first.summary.attachments_copied, 4);

    let source_vault = source.load(&PERSONAL).await?;
    let dest_vault = dest.load(&PERSONAL).await?;
    assert_eq!(dest_vault.items.len(), samples.len());
    let mut folders: Vec<&str> = dest_vault.folders.iter().map(|f| f.name.as_str()).collect();
    folders.sort_unstable();
    let mut expected_folders = fixtures::FOLDERS.to_vec();
    expected_folders.sort_unstable();
    assert_eq!(folders, expected_folders);

    for sample in &samples {
        let name = sample.content["name"].as_str().unwrap();
        let mirrored = by_name(&dest_vault, name);
        let original = by_name(&source_vault, name);
        assert_eq!(
            folder_of(&dest_vault, mirrored).as_deref(),
            sample.folder,
            "{name}"
        );
        assert_eq!(mirrored.archived, sample.archived, "{name}");
        assert_eq!(
            marker_of(mirrored).unwrap().source_id,
            original.id,
            "{name}"
        );
        // Everything but the marker is the content the source holds.
        let mut content = mirrored.content.clone();
        let Some(Node::List(fields)) = content.get_mut("fields") else {
            panic!()
        };
        let marker = fields.pop().unwrap();
        assert_eq!(
            marker.as_map().unwrap()["name"].as_str(),
            Some(MARKER_FIELD)
        );
        assert_eq!(
            Node::Map(content).normalized(),
            Node::Map(original.content.clone()).normalized(),
            "{name}"
        );
        for (file, data) in &sample.attachments {
            assert_eq!(
                &attachment_bytes(&dest, &dest_vault, name, file).await?,
                data,
                "{name}/{file}"
            );
        }
    }

    let second = run(&source, &dest, &options()).await?;
    assert!(!second.changed);
    assert_eq!(second.summary.items_unchanged, samples.len());
    assert_eq!(
        second.summary.items_created + second.summary.items_updated + second.summary.items_deleted,
        0
    );
    Ok(())
}

#[tokio::test]
async fn source_changes_are_carried_over_in_place() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    seed(&source).await?;
    run(&source, &dest, &options()).await?;
    let before = dest.load(&PERSONAL).await?;
    let vault = source.load(&PERSONAL).await?;

    // Edit one item, replace an attachment under the same name, trash one
    // item, add one, and move one to a new folder.
    let github = by_name(&vault, "GitHub");
    let mut draft = Draft {
        content: github.content.clone(),
        folder_id: github.folder_id.clone(),
        collection_ids: vec![],
    };
    let Some(Node::Map(login)) = draft.content.get_mut("login") else {
        panic!()
    };
    login.insert("password".into(), Node::secret("rotated password"));
    source.update_item(&PERSONAL, github, &draft).await?;
    let note = by_name(&vault, "Sichere Notiz — 日本語 🔐");
    let old = note
        .attachments
        .iter()
        .find(|a| a.file_name == "ünïcödé ✈.txt")
        .unwrap();
    source.delete_attachment(&note.id, &old.id).await?;
    source
        .upload_attachment(&PERSONAL, note, "ünïcödé ✈.txt", "andere größe".as_bytes())
        .await?;
    source
        .trash_items(&[by_name(&vault, "Visa").id.clone()])
        .await?;
    let new_folder = source.create_folder("Brand new").await?;
    let identity = by_name(&vault, "Identity");
    let moved = Draft {
        content: identity.content.clone(),
        folder_id: Some(new_folder.id.clone()),
        collection_ids: vec![],
    };
    source.update_item(&PERSONAL, identity, &moved).await?;
    let mut added = fixtures::samples().remove(2).content;
    added.insert("name".into(), Node::secret("Added later"));
    source
        .create_item(
            &PERSONAL,
            &Draft {
                content: added,
                folder_id: None,
                collection_ids: vec![],
            },
        )
        .await?;

    let outcome = run(&source, &dest, &options()).await?;
    assert_eq!(outcome.summary.items_created, 1);
    assert_eq!(outcome.summary.items_updated, 3);
    assert_eq!(outcome.summary.items_deleted, 1);
    assert_eq!(outcome.summary.folders_created, 1);
    assert_eq!(outcome.summary.attachments_copied, 2);

    let after = dest.load(&PERSONAL).await?;
    let github = by_name(&after, "GitHub");
    assert_eq!(
        github.id,
        by_name(&before, "GitHub").id,
        "an update keeps the destination item"
    );
    assert_eq!(
        github.content["login"].as_map().unwrap()["password"].as_str(),
        Some("rotated password")
    );
    // The untouched attachments of an edited item were not copied again.
    let old_ids: Vec<&str> = by_name(&before, "GitHub")
        .attachments
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    assert!(
        github
            .attachments
            .iter()
            .all(|a| old_ids.contains(&a.id.as_str()))
    );
    assert_eq!(
        attachment_bytes(&dest, &after, "Sichere Notiz — 日本語 🔐", "ünïcödé ✈.txt").await?,
        "andere größe".as_bytes()
    );
    assert_eq!(
        attachment_bytes(&dest, &after, "Sichere Notiz — 日本語 🔐", "empty.txt").await?,
        b""
    );
    assert!(after.items.iter().all(|item| item.name() != "Visa"));
    assert_eq!(
        folder_of(&after, by_name(&after, "Identity")).as_deref(),
        Some("Brand new")
    );
    by_name(&after, "Added later");

    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

#[tokio::test]
async fn changes_made_in_the_destination_are_undone() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    seed(&source).await?;
    run(&source, &dest, &options()).await?;
    let vault = dest.load(&PERSONAL).await?;

    let visa = by_name(&vault, "Visa");
    let mut tampered = Draft {
        content: visa.content.clone(),
        folder_id: None,
        collection_ids: vec![],
    };
    let Some(Node::Map(card)) = tampered.content.get_mut("card") else {
        panic!()
    };
    card.insert("number".into(), Node::secret("0000000000000000"));
    dest.update_item(&PERSONAL, visa, &tampered).await?;
    let github = by_name(&vault, "GitHub");
    dest.delete_attachment(&github.id, &github.attachments[0].id)
        .await?;
    dest.delete_items(&[by_name(&vault, "Identity").id.clone()])
        .await?;
    dest.trash_items(&[by_name(&vault, "Deploy key").id.clone()])
        .await?;
    let stray = fixtures::samples().remove(2).content;
    dest.create_item(
        &PERSONAL,
        &Draft {
            content: stray,
            folder_id: None,
            collection_ids: vec![],
        },
    )
    .await?;
    dest.create_folder("Not in the source").await?;
    dest.set_archived(&[by_name(&vault, "Archived note").id.clone()], false)
        .await?;

    let outcome = run(&source, &dest, &options()).await?;
    assert!(outcome.changed);
    // Identity and the trashed key are recreated; the stray item and the
    // trashed copy are removed.
    assert_eq!(outcome.summary.items_created, 2);
    assert_eq!(outcome.summary.items_deleted, 2);
    assert_eq!(outcome.summary.items_updated, 3);
    assert_eq!(outcome.summary.folders_deleted, 1);

    let after = dest.load(&PERSONAL).await?;
    assert_eq!(after.items.len(), fixtures::samples().len());
    assert_eq!(
        by_name(&after, "Visa").content["card"].as_map().unwrap()["number"].as_str(),
        Some("4111111111111111")
    );
    assert_eq!(by_name(&after, "GitHub").attachments.len(), 2);
    assert!(by_name(&after, "Archived note").archived);
    assert!(after.items.iter().all(|item| !item.in_trash));
    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

#[tokio::test]
async fn a_dry_run_reports_the_plan_and_changes_nothing() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    seed(&source).await?;

    let dry = MirrorOptions {
        dry_run: true,
        ..options()
    };
    let outcome = run(&source, &dest, &dry).await?;
    assert!(!outcome.changed);
    assert_eq!(outcome.summary.items_created, fixtures::samples().len());
    let vault = dest.load(&PERSONAL).await?;
    assert!(vault.items.is_empty() && vault.folders.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_destination_with_its_own_content_is_not_overwritten_unasked() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    seed(&source).await?;
    let own = fixtures::samples().remove(4).content;
    dest.create_item(
        &PERSONAL,
        &Draft {
            content: own,
            folder_id: None,
            collection_ids: vec![],
        },
    )
    .await?;

    let error = run(&source, &dest, &options()).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("--allow-initial-overwrite"),
        "{error:#}"
    );
    let untouched = dest.load(&PERSONAL).await?;
    assert_eq!(untouched.items.len(), 1);
    assert_eq!(untouched.items[0].name(), "Visa");

    let confirmed = MirrorOptions {
        safety: Safety {
            allow_initial_overwrite: true,
            ..Safety::default()
        },
        ..options()
    };
    let outcome = run(&source, &dest, &confirmed).await?;
    assert_eq!(outcome.summary.items_deleted, 1);
    assert_eq!(
        dest.load(&PERSONAL).await?.items.len(),
        fixtures::samples().len()
    );
    Ok(())
}

#[tokio::test]
async fn swapped_source_and_destination_are_refused() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    seed(&source).await?;
    run(&source, &dest, &options()).await?;

    let everything = MirrorOptions {
        safety: Safety {
            allow_initial_overwrite: true,
            allow_mass_delete: true,
            ..Safety::default()
        },
        ..options()
    };
    let error = run(&dest, &source, &everything).await.unwrap_err();
    assert!(format!("{error:#}").contains("swapped"), "{error:#}");
    let vault = source.load(&PERSONAL).await?;
    assert_eq!(vault.items.len(), fixtures::samples().len());
    assert!(vault.items.iter().all(|item| marker_of(item).is_none()));

    let error = run(&source, &source, &options()).await.unwrap_err();
    assert!(format!("{error:#}").contains("same vault"), "{error:#}");
    Ok(())
}

#[tokio::test]
async fn an_emptied_source_does_not_empty_the_mirror() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    for index in 0..12 {
        let mut content = fixtures::samples().remove(2).content;
        content.insert("name".into(), Node::secret(format!("Item {index}")));
        source
            .create_item(
                &PERSONAL,
                &Draft {
                    content,
                    folder_id: None,
                    collection_ids: vec![],
                },
            )
            .await?;
    }
    run(&source, &dest, &options()).await?;
    let ids: Vec<String> = source
        .load(&PERSONAL)
        .await?
        .items
        .into_iter()
        .map(|item| item.id)
        .collect();
    source.delete_items(&ids).await?;

    let error = run(&source, &dest, &options()).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("--allow-mass-delete"),
        "{error:#}"
    );
    assert_eq!(dest.load(&PERSONAL).await?.items.len(), 12);

    let confirmed = MirrorOptions {
        safety: Safety {
            allow_mass_delete: true,
            ..Safety::default()
        },
        ..options()
    };
    run(&source, &dest, &confirmed).await?;
    assert!(dest.load(&PERSONAL).await?.items.is_empty());
    Ok(())
}

#[tokio::test]
async fn editing_an_archived_item_keeps_it_archived() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    seed(&source).await?;
    run(&source, &dest, &options()).await?;

    let vault = source.load(&PERSONAL).await?;
    let archived = by_name(&vault, "Archived note");
    let mut draft = Draft {
        content: archived.content.clone(),
        folder_id: archived.folder_id.clone(),
        collection_ids: vec![],
    };
    draft
        .content
        .insert("notes".into(), Node::secret("edited while archived"));
    let edited = source.update_item(&PERSONAL, archived, &draft).await?;
    assert!(
        edited.archived,
        "an edit must not un-archive the source item either"
    );

    let outcome = run(&source, &dest, &options()).await?;
    assert_eq!(outcome.summary.items_updated, 1);
    let mirrored = dest.load(&PERSONAL).await?;
    let note = by_name(&mirrored, "Archived note");
    assert!(note.archived);
    assert_eq!(
        note.content["notes"].as_str(),
        Some("edited while archived")
    );

    // Taking an item out of the archive is carried over as well.
    source
        .set_archived(std::slice::from_ref(&edited.id), false)
        .await?;
    let outcome = run(&source, &dest, &options()).await?;
    assert_eq!(outcome.summary.items_updated, 1);
    assert!(!by_name(&dest.load(&PERSONAL).await?, "Archived note").archived);
    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

/// Bank accounts, driver's licences and passports. Servers that do not know
/// these types reject them; the test is skipped there.
#[tokio::test]
async fn newer_item_types_are_mirrored() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    if !source.server_traits().await?.hidden_item_types.is_empty() {
        eprintln!("skipping: the source server does not show these item types");
        return Ok(());
    }
    let block = |entries: &[(&str, &str)]| {
        Node::Map(
            entries
                .iter()
                .map(|(key, value)| (key.to_string(), Node::secret(*value)))
                .collect(),
        )
    };
    let items = [
        (
            6,
            "Bank account",
            "bankAccount",
            block(&[
                ("bankName", "Sparkasse"),
                ("nameOnAccount", "Erika Mustermann"),
                ("accountType", "checking"),
                ("iban", "DE02120300000000202051"),
                ("swiftCode", "BYLADEM1001"),
                ("pin", "1234"),
            ]),
        ),
        (
            7,
            "Driver's licence",
            "driversLicense",
            block(&[
                ("firstName", "Erika"),
                ("lastName", "Mustermann"),
                ("licenseNumber", "B072RRE2I55"),
                ("issueDate", "2015-03-01"),
                ("expirationDate", "2030-03-01"),
            ]),
        ),
        (
            8,
            "Passport",
            "passport",
            block(&[
                ("surname", "Mustermann"),
                ("givenName", "Erika"),
                ("passportNumber", "C01X00T47"),
                ("dateOfBirth", "1964-08-12"),
                ("expirationDate", "2031-10-31"),
            ]),
        ),
    ];
    let draft = |kind: u64, name: &str, key: &str, block: &Node| Draft {
        content: [
            ("type".to_string(), Node::Number(kind.into())),
            ("name".to_string(), Node::secret(name)),
            (key.to_string(), block.clone()),
        ]
        .into(),
        folder_id: None,
        collection_ids: vec![],
    };
    for (kind, name, key, block) in &items {
        if let Err(error) = source
            .create_item(&PERSONAL, &draft(*kind, name, key, block))
            .await
        {
            eprintln!("skipping: the source server rejects item type {kind}: {error:#}");
            return Ok(());
        }
        // The destination has to know the type as well.
        match dest
            .create_item(&PERSONAL, &draft(*kind, name, key, block))
            .await
        {
            Ok(probe) => dest.delete_items(&[probe.id]).await?,
            Err(error) => {
                eprintln!("skipping: the destination server rejects item type {kind}: {error:#}");
                return Ok(());
            }
        }
    }

    let first = run(&source, &dest, &options()).await?;
    if dest.server_traits().await?.hidden_item_types.is_empty() {
        assert_eq!(first.summary.items_created, items.len());
        let mirrored = dest.load(&PERSONAL).await?;
        for (_, name, key, block) in &items {
            // Servers fill in the fields that were left out as nulls.
            assert_eq!(
                by_name(&mirrored, name).content[*key].normalized(),
                block.normalized(),
                "{name}"
            );
        }
    } else {
        // A destination that would accept the items but never show them
        // gets none, instead of a new invisible copy on every run.
        assert_eq!(first.summary.items_created, 0);
        assert!(
            first.warnings.iter().any(|w| w.contains("left out")),
            "{:?}",
            first.warnings
        );
    }
    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

/// A folder filter mirrors a part of the vault, and changing it moves the
/// mirror to the new selection.
#[tokio::test]
async fn a_folder_filter_mirrors_part_of_the_vault() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    seed(&source).await?;
    let filtered = |include: &[&str], exclude: &[&str]| MirrorOptions {
        filter: Filter {
            include: include.iter().map(|n| n.to_string()).collect(),
            exclude: exclude.iter().map(|n| n.to_string()).collect(),
        },
        ..options()
    };
    let count = |selected: &dyn Fn(Option<&str>) -> bool| {
        fixtures::samples()
            .iter()
            .filter(|sample| selected(sample.folder))
            .count()
    };
    let folders = |vault: &vault_mirror::vault::Vault| {
        let mut names: Vec<String> = vault.folders.iter().map(|f| f.name.clone()).collect();
        names.sort();
        names
    };

    let work = filtered(&["Work"], &["Work/Infra"]);
    let first = run(&source, &dest, &work).await?;
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    let mirrored = dest.load(&PERSONAL).await?;
    assert_eq!(mirrored.items.len(), count(&|f| f == Some("Work")));
    assert_eq!(folders(&mirrored), ["Work"]);
    assert!(!run(&source, &dest, &work).await?.changed);

    let not_private = filtered(&[], &["Persönlich ✈"]);
    run(&source, &dest, &not_private).await?;
    let mirrored = dest.load(&PERSONAL).await?;
    assert_eq!(mirrored.items.len(), count(&|f| f != Some("Persönlich ✈")));
    assert_eq!(folders(&mirrored), ["Empty", "Work", "Work/Infra"]);
    assert!(!run(&source, &dest, &not_private).await?.changed);

    let typo = run(&source, &dest, &filtered(&[], &["Persönlich ✈", "Wrok"])).await?;
    assert!(
        typo.warnings.iter().any(|w| w.contains("\"Wrok\"")),
        "{:?}",
        typo.warnings
    );
    Ok(())
}
