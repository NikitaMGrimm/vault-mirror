mod common;

use anyhow::Result;
use common::*;
use vault_mirror::{
    mirror::{Filter, MirrorOptions},
    node::Node,
    plan::{MARKER_FIELD, Safety},
    vault::{Account, Draft, Item, Scope},
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

/// `item` with a marker as a run leaves it that stopped before its
/// attachments were copied and checked.
fn pending(item: &Item) -> Draft {
    let mut content = item.content.clone();
    let Some(Node::List(fields)) = content.get_mut("fields") else {
        panic!("no fields")
    };
    let marker = fields.last_mut().unwrap();
    let Node::Map(marker) = marker else { panic!() };
    let value = marker["value"].as_str().unwrap();
    let parts: Vec<&str> = value.split_whitespace().collect();
    let value = format!("{} ? ?", parts[..4].join(" "));
    marker.insert("value".into(), Node::secret(value));
    Draft {
        content,
        folder_id: item.folder_id.clone(),
        collection_ids: vec![],
    }
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
    fixtures::seed(&source).await?;
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
        assert_eq!(copied_from(mirrored).unwrap(), original.id, "{name}");
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
    fixtures::seed(&source).await?;
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
    let mut added = fixtures::named("Bare login without folder").content;
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
    // A deleted copy goes to the trash, which the server empties on its own.
    let visa: Vec<_> = after.items.iter().filter(|i| i.name() == "Visa").collect();
    assert!(matches!(visa.as_slice(), [copy] if copy.in_trash));
    assert_eq!(
        folder_of(&after, by_name(&after, "Identity")).as_deref(),
        Some("Brand new")
    );
    by_name(&after, "Added later");

    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

#[tokio::test]
async fn changes_to_copies_in_the_destination_are_replaced_only_when_allowed() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
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
    let stray = fixtures::named("Bare login without folder").content;
    dest.create_item(
        &PERSONAL,
        &Draft {
            content: stray,
            folder_id: None,
            collection_ids: vec![],
        },
    )
    .await?;
    let own_folder = dest.create_folder("Not in the source").await?;
    let note = by_name(&vault, "Archived note");
    dest.set_archived(std::slice::from_ref(&note.id), false)
        .await?;
    let changed = dest.load(&PERSONAL).await?;

    let mut ids = [visa.id.clone(), github.id.clone(), note.id.clone()];
    ids.sort();
    for error in [
        check(&source, &dest, &options()).await.unwrap_err(),
        run(&source, &dest, &options()).await.unwrap_err(),
    ] {
        let message = format!("{error:#}");
        assert!(
            message.contains(&format!(
                "3 copies changed in the destination since vault-mirror wrote them, and the \
                 run would replace or delete them ({})",
                ids.join(", ")
            )),
            "{message}"
        );
        assert!(message.contains("--allow-overwrite"), "{message}");
    }
    assert_eq!(dest.load(&PERSONAL).await?, changed);

    let preview = check(&source, &dest, &overwrite()).await?;
    assert!(!preview.changed);
    assert_eq!(dest.load(&PERSONAL).await?, changed);
    let outcome = run(&source, &dest, &overwrite()).await?;
    assert!(outcome.changed);
    assert_eq!(preview.summary, outcome.summary);
    // Identity and the trashed key are recreated, and the item added in the
    // destination stays. The trashed copy stays in the trash.
    assert_eq!(outcome.summary.items_created, 2);
    assert_eq!(outcome.summary.items_deleted, 0);
    assert_eq!(outcome.summary.items_updated, 3);
    assert!(
        (outcome.warnings.iter()).any(|w| w.starts_with("Replacing 3 copies changed")),
        "{:?}",
        outcome.warnings
    );

    let after = dest.load(&PERSONAL).await?;
    let live = after.items.iter().filter(|item| !item.in_trash).count();
    assert_eq!(live, fixtures::samples().len() + 1);
    assert_eq!(
        by_name(&after, "Visa").content["card"].as_map().unwrap()["number"].as_str(),
        Some("4111111111111111")
    );
    assert_eq!(by_name(&after, "GitHub").attachments.len(), 2);
    assert!(by_name(&after, "Archived note").archived);
    assert_eq!(after.items.iter().filter(|item| item.in_trash).count(), 1);
    assert!(after.folders.contains(&own_folder));
    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

#[tokio::test]
async fn a_check_reports_the_plan_and_changes_nothing() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;

    let outcome = check(&source, &dest, &options()).await?;
    assert!(!outcome.changed);
    assert_eq!(outcome.summary.items_created, fixtures::samples().len());
    let vault = dest.load(&PERSONAL).await?;
    assert!(vault.items.is_empty() && vault.folders.is_empty());
    Ok(())
}

#[tokio::test]
async fn items_vault_mirror_did_not_make_are_left_alone() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
    dest.create_item(
        &PERSONAL,
        &Draft {
            content: fixtures::named("Visa").content,
            folder_id: None,
            collection_ids: vec![],
        },
    )
    .await?;
    let own = dest.load(&PERSONAL).await?.items.remove(0);

    let first = run(&source, &dest, &options()).await?;
    assert_eq!(first.summary.items_created, fixtures::samples().len());
    assert_eq!(first.summary.items_deleted, 0);
    assert!(
        first
            .warnings
            .iter()
            .any(|w| w.contains("didn't copy from this source")),
        "{:?}",
        first.warnings
    );
    let after = dest.load(&PERSONAL).await?;
    assert_eq!(after.items.len(), fixtures::samples().len() + 1);
    let kept = after.items.iter().find(|item| item.id == own.id).unwrap();
    assert_eq!(kept.content, own.content);
    assert_eq!(kept.revision_date, own.revision_date);

    let second = run(&source, &dest, &options()).await?;
    assert!(!second.changed);
    assert!(second.warnings.is_empty(), "{:?}", second.warnings);
    Ok(())
}

#[tokio::test]
async fn swapped_source_and_destination_are_refused() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
    run(&source, &dest, &options()).await?;

    let everything = MirrorOptions {
        safety: Safety {
            allow_mass_delete: true,
            ..Safety::default()
        },
        ..options()
    };
    let error = run(&dest, &source, &everything).await.unwrap_err();
    assert!(format!("{error:#}").contains("swapped"), "{error:#}");
    let vault = source.load(&PERSONAL).await?;
    assert_eq!(vault.items.len(), fixtures::samples().len());
    assert!(vault.items.iter().all(|item| copied_from(item).is_none()));

    let error = run(&source, &source, &options()).await.unwrap_err();
    assert!(format!("{error:#}").contains("same vault"), "{error:#}");
    Ok(())
}

#[tokio::test]
async fn one_vault_reached_through_two_addresses_is_refused() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let account = sandbox.register(&sandbox.source_url).await?;
    let mut elsewhere = account.clone();
    elsewhere.server = account.server.replace("127.0.0.1", "localhost");
    assert_ne!(elsewhere.server, account.server);
    let (source, dest) = (
        sandbox.open(&account).await?,
        sandbox.open(&elsewhere).await?,
    );
    fixtures::seed(&source).await?;

    let everything = MirrorOptions {
        safety: Safety {
            allow_mass_delete: true,
            ..Safety::default()
        },
        ..options()
    };
    let error = run(&source, &dest, &everything).await.unwrap_err();
    assert!(format!("{error:#}").contains("same items"), "{error:#}");
    let vault = source.load(&PERSONAL).await?;
    assert_eq!(vault.items.len(), fixtures::samples().len());
    assert!(vault.items.iter().all(|item| copied_from(item).is_none()));
    Ok(())
}

/// After the source and destination switch roles, the old source takes the
/// new source's content into the items it already has, once confirmed, and
/// either direction then runs without new copies or extra markers.
#[tokio::test]
async fn switching_source_and_destination_takes_over_the_copies() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (first, second) = sandbox.pair().await?;
    fixtures::seed(&first).await?;
    run(&first, &second, &options()).await?;
    let before = first.load(&PERSONAL).await?;

    let error = run(&second, &first, &options()).await.unwrap_err();
    assert!(format!("{error:#}").contains("--reverse"), "{error:#}");
    let reverse = MirrorOptions {
        safety: Safety {
            allow_reverse: true,
            ..Safety::default()
        },
        ..options()
    };
    run(&second, &first, &reverse).await?;
    assert!(!run(&second, &first, &options()).await?.changed);
    assert!(!run(&first, &second, &options()).await?.changed);

    let after = first.load(&PERSONAL).await?;
    let ids = |vault: &vault_mirror::vault::Vault| {
        let mut ids: Vec<String> = vault.items.iter().map(|item| item.id.clone()).collect();
        ids.sort();
        ids
    };
    assert_eq!(
        ids(&after),
        ids(&before),
        "the same items, updated in place"
    );
    for item in after
        .items
        .iter()
        .chain(&second.load(&PERSONAL).await?.items)
    {
        let markers = item
            .fields()
            .iter()
            .filter(|field| {
                field
                    .as_map()
                    .and_then(|m| m.get("name"))
                    .and_then(Node::as_str)
                    == Some(MARKER_FIELD)
            })
            .count();
        assert!(markers <= 1, "{} has {markers} markers", item.id);
    }
    Ok(())
}

#[tokio::test]
async fn an_emptied_source_does_not_empty_the_mirror() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    for index in 0..12 {
        let mut content = fixtures::named("Bare login without folder").content;
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
    let after = dest.load(&PERSONAL).await?;
    assert!(after.items.iter().all(|item| item.in_trash));
    Ok(())
}

#[tokio::test]
async fn editing_an_archived_item_keeps_it_archived() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
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

    if dest.server_traits().await?.hidden_item_types.is_empty() {
        let first = run(&source, &dest, &options()).await?;
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
        assert!(!run(&source, &dest, &options()).await?.changed);
    } else {
        // A destination that would accept the items but never show them
        // gets none, instead of a new invisible copy on every run, and the
        // run fails rather than look complete.
        let error = run(&source, &dest, &options()).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot hold 3 items"),
            "{error:#}"
        );
        assert!(dest.load(&PERSONAL).await?.items.is_empty());
    }
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
    fixtures::seed(&source).await?;
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

    // A misspelt name stops the run, which would otherwise delete the
    // copies it no longer selects. Folder names are vault content, so the
    // error names the setting.
    let before = dest.load(&PERSONAL).await?;
    for typo in [filtered(&["Wrok"], &[]), filtered(&[], &["Wrok"])] {
        let error = run(&source, &dest, &typo).await.unwrap_err();
        let setting = match typo.filter.include.is_empty() {
            true => "EXCLUDE_FOLDERS",
            false => "INCLUDE_FOLDERS",
        };
        assert_eq!(
            format!("{error:#}"),
            format!(
                "planning failed: 1 name in {setting} matches no folder in the source. Check the \
                 spelling"
            )
        );
    }
    assert_eq!(dest.load(&PERSONAL).await?, before);
    Ok(())
}

/// Bitwarden accepts attachments only from premium accounts. A run into a
/// destination without premium fails before it changes anything, rather
/// than leave the attachments out and look complete.
#[tokio::test]
async fn a_destination_without_premium_gets_no_partial_copy() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source = sandbox
        .open(&sandbox.register(&sandbox.source_url).await?)
        .await?;
    let mut dest = sandbox
        .open(&sandbox.register_free(&sandbox.dest_url).await?)
        .await?;
    fixtures::seed(&source).await?;
    // Vaultwarden makes every account premium.
    dest.premium = false;

    let error = run(&source, &dest, &options()).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("no premium subscription"),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("4 attachments"), "{error:#}");
    assert!(dest.load(&PERSONAL).await?.items.is_empty());
    Ok(())
}

/// A destination that loses premium keeps the attachments it already has.
/// A run that would copy new ones fails before it changes anything.
#[tokio::test]
async fn attachments_stay_when_the_destination_loses_premium() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, mut dest) = sandbox.pair().await?;
    seed_extra(&source, 1).await?;
    let vault = source.load(&PERSONAL).await?;
    let item = by_name(&vault, "Extra note 0");
    source
        .upload_attachment(&PERSONAL, item, "kept.txt", b"kept")
        .await?;
    run(&source, &dest, &options()).await?;

    dest.premium = false;
    assert!(!run(&source, &dest, &options()).await?.changed);
    source
        .upload_attachment(&PERSONAL, item, "new.txt", b"new")
        .await?;
    let error = run(&source, &dest, &options()).await.unwrap_err();
    assert!(format!("{error:#}").contains("no premium"), "{error:#}");
    let mirrored = dest.load(&PERSONAL).await?;
    assert_eq!(
        attachment_bytes(&dest, &mirrored, "Extra note 0", "kept.txt").await?,
        b"kept"
    );
    assert_eq!(by_name(&mirrored, "Extra note 0").attachments.len(), 1);

    dest.premium = true;
    assert!(run(&source, &dest, &options()).await?.changed);
    let mirrored = dest.load(&PERSONAL).await?;
    assert_eq!(
        attachment_bytes(&dest, &mirrored, "Extra note 0", "new.txt").await?,
        b"new"
    );
    Ok(())
}

/// A run that stops while copying attachments leaves a marker that vouches
/// for none. Without files, the next run copies them all. With files,
/// nothing tells those the stopped run uploaded from ones someone added
/// since, so the next run refuses until it is allowed to replace them.
#[tokio::test]
async fn an_interrupted_attachment_copy_is_repaired() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    seed_extra(&source, 2).await?;
    let vault = source.load(&PERSONAL).await?;
    for name in ["Extra note 0", "Extra note 1"] {
        source
            .upload_attachment(&PERSONAL, by_name(&vault, name), "file.txt", b"old")
            .await?;
    }
    run(&source, &dest, &options()).await?;

    let vault = source.load(&PERSONAL).await?;
    for name in ["Extra note 0", "Extra note 1"] {
        let item = by_name(&vault, name);
        source
            .delete_attachment(&item.id, &item.attachments[0].id)
            .await?;
        source
            .upload_attachment(&PERSONAL, item, "file.txt", b"new")
            .await?;
    }
    let dest_vault = dest.load(&PERSONAL).await?;
    let emptied = by_name(&dest_vault, "Extra note 0");
    dest.delete_attachment(&emptied.id, &emptied.attachments[0].id)
        .await?;
    let emptied = dest.get_item(&PERSONAL, &emptied.id).await?;
    dest.update_item(&PERSONAL, &emptied, &pending(&emptied))
        .await?;
    let kept = by_name(&dest_vault, "Extra note 1");
    dest.update_item(&PERSONAL, kept, &pending(kept)).await?;

    let error = run(&source, &dest, &options()).await.unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains(&format!("({})", kept.id)), "{message}");
    assert!(message.contains("--allow-overwrite"), "{message}");
    run(&source, &dest, &overwrite()).await?;
    let mirrored = dest.load(&PERSONAL).await?;
    for name in ["Extra note 0", "Extra note 1"] {
        assert_eq!(
            attachment_bytes(&dest, &mirrored, name, "file.txt").await?,
            b"new"
        );
    }
    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

/// A run copies attachment files out of the source backup it just made, so
/// it copies what the backup holds even if the source changes meanwhile.
#[tokio::test]
async fn attachments_are_copied_from_the_source_backup() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    seed_extra(&source, 1).await?;
    let vault = source.load(&PERSONAL).await?;
    let item = by_name(&vault, "Extra note 0");
    source
        .upload_attachment(&PERSONAL, item, "file.txt", b"backed up")
        .await?;
    let source_read = protect(&source, &PERSONAL).await?;
    let dest_read = protect(&dest, &PERSONAL).await?;
    let item = by_name(&source_read.vault, "Extra note 0");
    source
        .delete_attachment(&item.id, &item.attachments[0].id)
        .await?;
    // Added after the backup, so the run leaves it for the next one.
    seed_extra(&source, 2).await?;

    let outcome = run_protected(
        &source,
        PERSONAL,
        &dest,
        PERSONAL,
        (&source_read, &dest_read),
        &options(),
    )
    .await?;
    assert_eq!(outcome.summary.items_created, 1);
    let mirrored = dest.load(&PERSONAL).await?;
    assert_eq!(mirrored.items.len(), 1);
    assert_eq!(
        attachment_bytes(&dest, &mirrored, "Extra note 0", "file.txt").await?,
        b"backed up"
    );
    Ok(())
}

/// A copy the plan deletes, but that was edited after the backup was read,
/// is not deleted: no backup holds the edit.
#[tokio::test]
async fn a_copy_edited_after_the_backup_is_not_deleted() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
    run(&source, &dest, &options()).await?;
    let originals = source.load(&PERSONAL).await?;
    source
        .delete_items(&[by_name(&originals, "Identity").id.clone()])
        .await?;

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
    .expect_err("the edited copy must stop the deletion");
    let error = error.downcast::<vault_mirror::mirror::MirrorError>()?;
    assert_eq!(error.stage, "deleting obsolete items");
    let after = dest.load(&PERSONAL).await?;
    assert_eq!(
        by_name(&after, "Identity").content["notes"].as_str(),
        Some("edited meanwhile")
    );

    // The next run's backup holds the edit, and it deletes the copy only
    // when allowed to.
    let error = run(&source, &dest, &options()).await.unwrap_err();
    assert!(format!("{error:#}").contains("--allow-overwrite"));
    run(&source, &dest, &overwrite()).await?;
    let after = dest.load(&PERSONAL).await?;
    assert!((after.items.iter()).all(|item| item.in_trash || item.name() != "Identity"));
    Ok(())
}

/// Copies go into the folder the plan picked, even when the destination has
/// two of that name that both have to stay.
#[tokio::test]
async fn copies_converge_with_two_folders_of_one_name() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
    run(&source, &dest, &options()).await?;
    let first = dest.load(&PERSONAL).await?.folders;
    let first = first.iter().find(|f| f.name == "Work").unwrap();
    let second = dest.create_folder("Work").await?;
    // An item of your own in each keeps both folders, whichever the server
    // lists first.
    for folder in [&first.id, &second.id] {
        let mut content = fixtures::named("Bare login without folder").content;
        content.insert("name".into(), Node::secret("Own item"));
        dest.create_item(
            &PERSONAL,
            &Draft {
                content,
                folder_id: Some(folder.clone()),
                collection_ids: vec![],
            },
        )
        .await?;
    }
    let source_vault = source.load(&PERSONAL).await?;
    let work = source_vault
        .folders
        .iter()
        .find(|f| f.name == "Work")
        .unwrap();
    let mut content = fixtures::named("Bare login without folder").content;
    content.insert("name".into(), Node::secret("New in Work"));
    source
        .create_item(
            &PERSONAL,
            &Draft {
                content,
                folder_id: Some(work.id.clone()),
                collection_ids: vec![],
            },
        )
        .await?;

    assert!(run(&source, &dest, &options()).await?.changed);
    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

/// Items in the destination's trash are the server's to purge.
#[tokio::test]
async fn the_destination_trash_is_left_alone() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
    run(&source, &dest, &options()).await?;
    let mirrored = dest.load(&PERSONAL).await?;
    let trashed = mirrored.items[0].id.clone();
    dest.trash_items(std::slice::from_ref(&trashed)).await?;

    let again = run(&source, &dest, &options()).await?;
    assert_eq!(again.summary.items_created, 1);
    assert_eq!(again.summary.items_deleted, 0);
    let after = dest.load(&PERSONAL).await?;
    assert!(
        after
            .items
            .iter()
            .any(|item| item.id == trashed && item.in_trash)
    );
    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

/// An archived item whose attachments are copied ends up archived, also on
/// servers where an upload takes an item out of the archive.
#[tokio::test]
async fn archived_items_with_attachments_stay_archived() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    // Enough items that they are imported in bulk.
    seed_extra(&source, 12).await?;
    let vault = source.load(&PERSONAL).await?;
    let item = by_name(&vault, "Extra note 0");
    source
        .upload_attachment(&PERSONAL, item, "file.txt", b"archived")
        .await?;
    source
        .set_archived(std::slice::from_ref(&item.id), true)
        .await?;

    run(&source, &dest, &options()).await?;
    let mirrored = dest.load(&PERSONAL).await?;
    let copy = by_name(&mirrored, "Extra note 0");
    assert!(copy.archived);
    assert_eq!(
        attachment_bytes(&dest, &mirrored, "Extra note 0", "file.txt").await?,
        b"archived"
    );

    // Replaced attachments on an item that is already archived.
    let vault = source.load(&PERSONAL).await?;
    let item = by_name(&vault, "Extra note 0");
    source
        .upload_attachment(&PERSONAL, item, "second.txt", b"second")
        .await?;
    // Bitwarden's server takes the source item out of the archive too.
    let vault = source.load(&PERSONAL).await?;
    let item = by_name(&vault, "Extra note 0");
    if !item.archived {
        source
            .set_archived(std::slice::from_ref(&item.id), true)
            .await?;
    }
    run(&source, &dest, &options()).await?;
    let mirrored = dest.load(&PERSONAL).await?;
    assert!(by_name(&mirrored, "Extra note 0").archived);
    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

/// A first run that creates more items than are created one at a time
/// mirrors them all.
#[tokio::test]
async fn a_first_run_with_many_new_items_mirrors_them_all() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    fixtures::seed(&source).await?;
    seed_extra(&source, 12).await?;
    let total = fixtures::samples().len() + 12;

    let first = run(&source, &dest, &options()).await?;
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    assert_eq!(first.summary.items_created, total);
    assert_eq!(first.summary.attachments_copied, 4);
    let mirrored = dest.load(&PERSONAL).await?;
    assert_eq!(mirrored.items.len(), total);
    let extra = by_name(&mirrored, "Extra note 0");
    assert_eq!(folder_of(&mirrored, extra).as_deref(), Some("Work"));
    assert!(mirrored.items.iter().any(|item| item.archived));
    assert!(!run(&source, &dest, &options()).await?.changed);
    Ok(())
}

/// Ciphertext this tool cannot read is not plain text: copying it verbatim
/// would put a value into the destination that no client can open, and the
/// next run would report the copy as verified.
#[tokio::test]
async fn ciphertext_in_an_unsupported_format_stops_the_run() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, dest) = sandbox.pair().await?;
    let enc = vault_mirror::crypto::SymKey::generate().encrypt_str("unreadable")?;
    // Type 1 is a legacy format that bitwarden-crypto does not read.
    let unsupported = format!("1{}", &enc[1..]);
    let draft = Draft {
        content: [
            ("type".into(), Node::Number(2.into())),
            ("name".into(), Node::secret("Unreadable note")),
            (
                "secureNote".into(),
                Node::Map([("type".into(), Node::Number(0.into()))].into()),
            ),
            ("notes".into(), Node::Text(unsupported.clone())),
        ]
        .into(),
        folder_id: None,
        collection_ids: vec![],
    };
    let error = source.create_item(&PERSONAL, &draft).await.unwrap_err();
    // Bitwarden's server validates encrypted strings and refuses the item.
    // Vaultwarden stores it, and reading it back is what fails.
    let refused = error
        .downcast_ref::<vault_mirror::api::ApiError>()
        .and_then(|error| error.status())
        .is_some_and(|status| status.as_u16() == 400);
    if refused {
        return Ok(());
    }
    assert!(
        format!("{error:#}").contains("unsupported format"),
        "{error:#}"
    );

    let error = run(&source, &dest, &options()).await.unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("decrypting item"), "{message}");
    assert!(message.contains("field notes"), "{message}");
    assert!(message.contains("unsupported format"), "{message}");
    assert!(!message.contains(&unsupported[2..12]), "{message}");
    assert!(dest.load(&PERSONAL).await?.items.is_empty());
    Ok(())
}
