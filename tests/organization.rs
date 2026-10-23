mod common;

use anyhow::Result;
use common::*;
use vault_mirror::{
    crypto::KdfParams,
    mirror::{self, Endpoint, MirrorOptions},
    node::Node,
    vault::{Account, Draft, Scope, Vault},
};

async fn organization(sandbox: &Sandbox, server: &str) -> Result<(Account, Scope, String)> {
    let owner = sandbox.register(server).await?;
    let org = create_organization(sandbox, &owner, "Mirror Test Org").await?;
    // Opened after the organization exists, so its key is loaded.
    Ok((
        sandbox.open(&owner).await?,
        Scope::Organization(org.clone()),
        org,
    ))
}

fn collections_of(vault: &Vault, name: &str) -> Vec<String> {
    let item = by_name(vault, name);
    let mut names: Vec<String> = vault
        .collections
        .iter()
        .filter(|c| item.collection_ids.contains(&c.id))
        .map(|c| c.name.clone())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn an_organization_is_mirrored_by_collection_name() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, source_scope, source_org) = organization(&sandbox, &sandbox.source_url).await?;
    let (dest, dest_scope, _) = organization(&sandbox, &sandbox.dest_url).await?;

    let team_a = source
        .create_collection(&source_org, "Team A", Some("ext-a"))
        .await?;
    let team_b = source
        .create_collection(&source_org, "Team B", None)
        .await?;
    source
        .create_collection(&source_org, "Empty team", None)
        .await?;
    let mut samples = fixtures::samples();
    let targets = [
        vec![team_a.id.clone()],
        vec![team_a.id.clone(), team_b.id.clone()],
        vec![team_b.id.clone()],
    ];
    for (sample, collection_ids) in samples.drain(..3).zip(targets) {
        let draft = Draft {
            content: sample.content,
            folder_id: None,
            collection_ids,
        };
        let item = source.create_item(&source_scope, &draft).await?;
        for (name, data) in &sample.attachments {
            source
                .upload_attachment(&source_scope, &item, name, data)
                .await?;
        }
    }
    // The owner's individual vault is not part of the organization mirror.
    let private = samples.remove(0).content;
    source
        .create_item(
            &Scope::Personal,
            &Draft {
                content: private,
                folder_id: None,
                collection_ids: vec![],
            },
        )
        .await?;

    let mut options = MirrorOptions::default();
    let first = run_scoped(
        &source,
        source_scope.clone(),
        &dest,
        dest_scope.clone(),
        &options,
    )
    .await?;
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    assert_eq!(first.summary.items_created, 3);
    assert_eq!(first.summary.collections_created, 3);
    assert_eq!(first.summary.attachments_copied, 2);

    let vault = dest.load(&dest_scope).await?;
    assert_eq!(vault.items.len(), 3);
    assert_eq!(collections_of(&vault, "GitHub"), ["Team A"]);
    assert_eq!(
        collections_of(&vault, "Passkey login"),
        ["Team A", "Team B"]
    );
    assert_eq!(
        collections_of(&vault, "Bare login without folder"),
        ["Team B"]
    );
    let team_a_copy = vault
        .collections
        .iter()
        .find(|c| c.name == "Team A")
        .unwrap();
    assert_eq!(team_a_copy.external_id.as_deref(), Some("ext-a"));
    let github = by_name(&vault, "GitHub");
    let file = github
        .attachments
        .iter()
        .find(|a| a.file_name == "recovery codes.txt")
        .unwrap();
    assert_eq!(
        dest.download_attachment(&dest_scope, github, file).await?,
        b"1234-5678\nabcd-efgh\n"
    );
    assert!(dest.load(&Scope::Personal).await?.items.is_empty());

    assert!(
        !run_scoped(
            &source,
            source_scope.clone(),
            &dest,
            dest_scope.clone(),
            &options
        )
        .await?
        .changed
    );

    let source_vault = source.load(&source_scope).await?;
    let github = by_name(&source_vault, "GitHub");
    let moved = Draft {
        content: github.content.clone(),
        folder_id: None,
        collection_ids: vec![team_b.id.clone()],
    };
    source.update_item(&source_scope, github, &moved).await?;
    let passkey = by_name(&source_vault, "Passkey login");
    let mut edited = Draft {
        content: passkey.content.clone(),
        folder_id: None,
        collection_ids: passkey.collection_ids.clone(),
    };
    edited
        .content
        .insert("notes".into(), Node::secret("edited in the organization"));
    source.update_item(&source_scope, passkey, &edited).await?;
    let empty = source_vault
        .collections
        .iter()
        .find(|c| c.name == "Empty team")
        .unwrap();
    source.delete_collection(&source_org, &empty.id).await?;

    let needs_permission = !dest.server_traits().await?.organization_updates_safe;
    if needs_permission {
        let before = dest.load(&dest_scope).await?;
        let refused = run_scoped(
            &source,
            source_scope.clone(),
            &dest,
            dest_scope.clone(),
            &options,
        )
        .await
        .unwrap_err();
        assert!(
            dest.load(&dest_scope).await? == before,
            "a refused run changed the destination"
        );
        assert!(format!("{refused:#}").contains("cannot safely update organization copies"));
        assert!(format!("{refused:#}").contains("--allow-organization-metadata-loss"));
    }

    let source_read = source.load_with_traits(&source_scope).await?;
    let dest_read = dest.load_with_traits(&dest_scope).await?;
    let warning = "Organization updates can erase members' private folder, favorite and archive \
                   data. Backups cannot restore it.";
    let expected_warnings: Vec<String> = if needs_permission {
        vec![warning.into()]
    } else {
        vec![]
    };
    for allowed in [false, true] {
        options.allow_organization_metadata_loss = allowed;
        let preview = mirror::check(
            &Endpoint {
                account: &source,
                scope: source_scope.clone(),
            },
            &Endpoint {
                account: &dest,
                scope: dest_scope.clone(),
            },
            &source_read,
            &dest_read,
            &options,
            &folders(),
            &Capture::default(),
        )
        .await;
        if needs_permission && !allowed {
            let error = preview.unwrap_err();
            assert!(format!("{:#}", error.source).contains("--allow-organization-metadata-loss"));
        } else {
            let preview = preview?;
            assert_eq!(preview.summary.items_updated, 2);
            assert_eq!(preview.warnings, expected_warnings);
        }
        assert!(
            dest.load(&dest_scope).await? == *dest_read.vault(),
            "a preview changed the destination"
        );
    }

    let second = run_scoped(
        &source,
        source_scope.clone(),
        &dest,
        dest_scope.clone(),
        &options,
    )
    .await?;
    assert_eq!(second.summary.items_updated, 2);
    assert_eq!(second.warnings, expected_warnings);
    let vault = dest.load(&dest_scope).await?;
    assert_eq!(collections_of(&vault, "GitHub"), ["Team B"]);
    assert_eq!(
        by_name(&vault, "GitHub").attachments.len(),
        2,
        "moving an item keeps its attachments"
    );
    assert_eq!(
        by_name(&vault, "Passkey login").content["notes"].as_str(),
        Some("edited in the organization")
    );
    // Collections are left for you to delete.
    assert!(vault.collections.iter().any(|c| c.name == "Empty team"));
    let unchanged = run_scoped(&source, source_scope, &dest, dest_scope, &options).await?;
    assert!(!unchanged.changed);
    assert!(unchanged.warnings.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_copy_keeps_the_folder_and_favorite_its_member_chose() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, source_scope, source_org) = organization(&sandbox, &sandbox.source_url).await?;
    let (dest, dest_scope, _) = organization(&sandbox, &sandbox.dest_url).await?;
    let team = source.create_collection(&source_org, "Team", None).await?;
    for name in ["GitHub", "Visa"] {
        let draft = Draft {
            content: fixtures::named(name).content,
            folder_id: None,
            collection_ids: vec![team.id.clone()],
        };
        source.create_item(&source_scope, &draft).await?;
    }
    let options = MirrorOptions::default();
    let run = || {
        run_scoped(
            &source,
            source_scope.clone(),
            &dest,
            dest_scope.clone(),
            &options,
        )
    };
    run().await?;
    let favorite = |vault: &Vault, name: &str| by_name(vault, name).content["favorite"].clone();
    let copies = dest.load(&dest_scope).await?;
    // The source GitHub item is a favorite of its member, not of the organization.
    assert_eq!(favorite(&copies, "GitHub"), Node::Bool(false));

    // Folders and favorites are the member's own, not part of the organization.
    let mine = dest.create_folder("Mine").await?;
    for copy in copies.items {
        let mut content = copy.content.clone();
        content.insert("favorite".into(), Node::Bool(copy.name() == "Visa"));
        let filed = Draft {
            content,
            folder_id: Some(mine.id.clone()),
            collection_ids: copy.collection_ids.clone(),
        };
        dest.update_item(&dest_scope, &copy, &filed).await?;
    }
    assert!(!run().await?.changed);

    for copy in dest.load(&dest_scope).await?.items {
        assert_eq!(copy.folder_id.as_ref(), Some(&mine.id));
    }

    let vault = source.load(&source_scope).await?;
    let github = by_name(&vault, "GitHub");
    let mut edited = Draft {
        content: github.content.clone(),
        folder_id: None,
        collection_ids: github.collection_ids.clone(),
    };
    edited
        .content
        .insert("notes".into(), Node::secret("edited"));
    let github = source.update_item(&source_scope, github, &edited).await?;
    source
        .upload_attachment(&source_scope, &github, "codes.txt", b"1234")
        .await?;
    let visa = by_name(&vault, "Visa");
    source
        .set_archived(std::slice::from_ref(&visa.id), true)
        .await?;

    if !dest.server_traits().await?.organization_updates_safe {
        source
            .create_item(
                &source_scope,
                &Draft {
                    content: fixtures::named("Visa").content,
                    folder_id: None,
                    collection_ids: vec![team.id.clone()],
                },
            )
            .await?;
        let before = (
            source.load(&source_scope).await?,
            dest.load(&dest_scope).await?,
        );
        let mut permitted = MirrorOptions::default();
        permitted.safety.allow_overwrite = true;
        permitted.safety.allow_mass_delete = true;
        permitted.safety.allow_reverse = true;
        for options in [&options, &permitted] {
            let refused = run_scoped(
                &source,
                source_scope.clone(),
                &dest,
                dest_scope.clone(),
                options,
            )
            .await
            .unwrap_err();
            assert!(
                (
                    source.load(&source_scope).await?,
                    dest.load(&dest_scope).await?
                ) == before,
                "a refused run changed a vault"
            );
            assert!(format!("{refused:#}").contains("cannot safely update organization copies"));
        }
        return Ok(());
    }

    assert_eq!(run().await?.summary.items_updated, 2);
    let copies = dest.load(&dest_scope).await?;
    for copy in &copies.items {
        assert_eq!(copy.folder_id.as_ref(), Some(&mine.id), "{}", copy.name());
    }
    assert_eq!(favorite(&copies, "GitHub"), Node::Bool(false));
    assert_eq!(favorite(&copies, "Visa"), Node::Bool(true));
    assert_eq!(by_name(&copies, "GitHub").attachments.len(), 1);
    assert!(by_name(&copies, "Visa").archived);
    assert!(!run().await?.changed);
    Ok(())
}

#[tokio::test]
async fn an_individual_vault_cannot_be_mirrored_into_an_organization() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let source = sandbox
        .open(&sandbox.register(&sandbox.source_url).await?)
        .await?;
    let (dest, dest_scope, _) = organization(&sandbox, &sandbox.dest_url).await?;
    let error = run_scoped(
        &source,
        Scope::Personal,
        &dest,
        dest_scope,
        &MirrorOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("both be"), "{error:#}");
    Ok(())
}

#[tokio::test]
async fn accounts_using_argon2_work() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let kdf = KdfParams::Argon2id {
        iterations: 3,
        memory_mib: 64,
        parallelism: 4,
    };
    let source = sandbox
        .open(&sandbox.register_with(&sandbox.source_url, &kdf).await?)
        .await?;
    let dest = sandbox
        .open(&sandbox.register(&sandbox.dest_url).await?)
        .await?;
    fixtures::seed(&source).await?;
    let outcome = run(&source, &dest, &MirrorOptions::default()).await?;
    assert_eq!(outcome.summary.items_created, fixtures::samples().len());
    Ok(())
}

#[tokio::test]
async fn a_wrong_master_password_is_reported_as_such() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let mut account = sandbox.register(&sandbox.source_url).await?;
    account.password = "not the password".into();
    let error = sandbox.open(&account).await.err().expect("must fail");
    assert!(
        format!("{error:#}").contains("master password is wrong"),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test]
async fn a_server_behind_a_private_certificate_authority_needs_that_authority() -> Result<()> {
    use vault_mirror::api::{ClientOptions, ServerUrls, build_http_client};

    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let account = sandbox.register(&sandbox.dest_url).await?;
    let urls = ServerUrls::from_server(&sandbox.dest_tls_url)?;

    let untrusting = build_http_client(&ClientOptions::default())?;
    let error = Account::open(
        untrusting,
        urls.clone(),
        account.credentials.clone(),
        &account.password,
    )
    .await
    .err()
    .expect("an unknown authority must be rejected");
    let message = format!("{error:#}");
    assert!(message.contains("certificate"), "{message}");
    assert!(
        !message.contains("127.0.0.1"),
        "the URL is not repeated: {message}"
    );

    let options = ClientOptions {
        extra_root_certs: vec![std::fs::read(&sandbox.ca_file)?],
        ..ClientOptions::default()
    };
    let trusting = build_http_client(&options)?;
    let opened = Account::open(
        trusting,
        urls,
        account.credentials.clone(),
        &account.password,
    )
    .await?;
    assert_eq!(opened.user_id, account.user_id);
    Ok(())
}

/// Bitwarden's server leaves collections an owner is not assigned to out of
/// the owner's syncs, so the run stops instead of treating their items as
/// deleted.
#[tokio::test]
async fn collections_the_account_is_not_assigned_to_stop_the_run() -> Result<()> {
    use vault_mirror::api::ServerUrls;

    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let server = &sandbox.dest_url;
    let api = ServerUrls::from_server(server)?.api;
    let config: serde_json::Value = sandbox
        .http
        .get(format!("{api}/config"))
        .send()
        .await?
        .json()
        .await?;
    if config
        .pointer("/server/name")
        .and_then(|name| name.as_str())
        == Some("Vaultwarden")
    {
        eprintln!("skipping: Vaultwarden shows owners every collection");
        return Ok(());
    }
    let owner = sandbox.register(server).await?;
    let org = create_organization(&sandbox, &owner, "Mirror Test Org").await?;
    // A collection nobody is assigned to, as an administrator might add.
    official_sql(&format!(
        r#"INSERT INTO "Collection" ("Id", "OrganizationId", "Name", "CreationDate", "RevisionDate")
           VALUES ('{}', '{org}', 'unassigned', now(), now());"#,
        uuid::Uuid::new_v4()
    ))?;

    let account = sandbox.open(&owner).await?;
    let error = account
        .load(&Scope::Organization(org))
        .await
        .expect_err("an unassigned collection must stop the run");
    assert!(
        format!("{error:#}").contains("1 collection and 0 items"),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test]
async fn hidden_member_items_stop_backups_and_runs() -> Result<()> {
    use vault_mirror::{
        backup::{BackupOptions, backup},
        snapshot,
        store::{Keys, Store},
    };

    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, source_scope, source_org) = organization(&sandbox, &sandbox.source_url).await?;
    if !source.server_traits().await?.items_need_collections {
        eprintln!("skipping: Vaultwarden has no private member collections");
        return Ok(());
    }
    let (dest, dest_scope, _) = organization(&sandbox, &sandbox.dest_url).await?;
    let shared = source.load(&source_scope).await?.collections[0].id.clone();
    let member = source
        .create_collection(&source_org, "Member collection", None)
        .await?;
    let mut member_item = String::new();
    for (index, sample) in fixtures::samples().into_iter().take(3).enumerate() {
        let item = source
            .create_item(
                &source_scope,
                &Draft {
                    content: sample.content,
                    folder_id: None,
                    collection_ids: vec![if index == 0 { &member.id } else { &shared }.clone()],
                },
            )
            .await?;
        if index == 0 {
            member_item = item.id;
        }
    }
    let options = MirrorOptions::default();
    let first = run_scoped(
        &source,
        source_scope.clone(),
        &dest,
        dest_scope.clone(),
        &options,
    )
    .await?;
    assert_eq!(first.summary.items_created, 3);
    let before = dest.load(&dest_scope).await?;

    let dir = tempfile::tempdir()?;
    let keys = Keys::open(dir.path(), "synthetic passphrase")?;
    let store = Store::new(dir.path(), "source", &keys);
    let now = time::OffsetDateTime::now_utc();
    let (complete, _) = backup(
        &source,
        &source_scope,
        &store,
        &BackupOptions {
            retention_days: 1,
            now: now - time::Duration::days(60),
        },
        &Capture::default(),
    )
    .await?;
    let complete = complete
        .snapshot
        .expect("the first backup makes a snapshot");

    // Bitwarden excludes default member collections from its shared inventory.
    official_sql(&format!(
        r#"UPDATE "Collection" SET "Type" = 1 WHERE "Id" = '{id}';
           DELETE FROM "CollectionUsers" WHERE "CollectionId" = '{id}';"#,
        id = member.id,
    ))?;
    let error = source
        .load_with_traits(&source_scope)
        .await
        .err()
        .expect("an inaccessible member item must stop the read");
    assert!(format!("{error:#}").contains("0 collections and 1 item"));
    let partial = backup(
        &source,
        &source_scope,
        &store,
        &BackupOptions {
            retention_days: 1,
            now,
        },
        &Capture::default(),
    )
    .await;
    assert!(partial.is_err(), "a partial backup was accepted");
    assert_eq!(store.list()?.as_slice(), std::slice::from_ref(&complete));
    assert_eq!(
        snapshot::parse(&store.snapshot(&complete).document()?)?
            .items()
            .len(),
        3
    );
    assert!(
        run_scoped(
            &source,
            source_scope.clone(),
            &dest,
            dest_scope.clone(),
            &options,
        )
        .await
        .is_err()
    );
    assert!(dest.load(&dest_scope).await? == before);

    official_sql(&format!(
        r#"UPDATE "Collection" SET "Type" = 0 WHERE "Id" = '{id}';
           INSERT INTO "CollectionUsers" ("CollectionId", "OrganizationUserId", "ReadOnly", "HidePasswords", "Manage")
           SELECT '{id}', "Id", false, false, true FROM "OrganizationUser"
           WHERE "OrganizationId" = '{source_org}' AND "UserId" = '{user}';"#,
        id = member.id,
        user = source.user_id,
    ))?;
    source.trash_items(&[member_item]).await?;
    let deleted = run_scoped(&source, source_scope, &dest, dest_scope, &options).await?;
    assert_eq!(deleted.summary.items_deleted, 1);
    Ok(())
}

/// An organization item can be in no collection. Bitwarden's server hides
/// such items from their owner and refuses to create them, so neither side
/// may lose one silently.
#[tokio::test]
async fn items_in_no_collection_are_never_lost_silently() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (source, source_scope, _) = organization(&sandbox, &sandbox.source_url).await?;
    let (dest, dest_scope, _) = organization(&sandbox, &sandbox.dest_url).await?;
    let note = |collection_ids: Vec<String>| Draft {
        content: [
            ("type".to_string(), Node::Number(2.into())),
            ("name".to_string(), Node::secret("Loose note")),
            (
                "secureNote".to_string(),
                Node::Map([("type".to_string(), Node::Number(0.into()))].into()),
            ),
        ]
        .into(),
        folder_id: None,
        collection_ids,
    };

    if source.server_traits().await?.items_need_collections {
        // Created in a collection, then taken out of it, as the web vault
        // allows an administrator to do.
        let collection = source.load(&source_scope).await?.collections[0].id.clone();
        let item = source
            .create_item(&source_scope, &note(vec![collection]))
            .await?;
        official_sql(&format!(
            r#"DELETE FROM "CollectionCipher" WHERE "CipherId" = '{}';"#,
            item.id
        ))?;
        let error = source
            .load(&source_scope)
            .await
            .expect_err("an item hidden from the owner must stop the run");
        assert!(
            format!("{error:#}").contains("0 collections and 1 item"),
            "{error:#}"
        );
        return Ok(());
    }

    source.create_item(&source_scope, &note(vec![])).await?;
    let options = MirrorOptions::default();
    let run = || {
        run_scoped(
            &source,
            source_scope.clone(),
            &dest,
            dest_scope.clone(),
            &options,
        )
    };
    if dest.server_traits().await?.items_need_collections {
        let error = run().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("1 item of the source is in no collection"),
            "{error:#}"
        );
        assert!(dest.load(&dest_scope).await?.items.is_empty());
    } else {
        let first = run().await?;
        assert_eq!(first.summary.items_created, 1);
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);
        assert!(!run().await?.changed);
    }
    Ok(())
}
