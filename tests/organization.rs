mod common;

use anyhow::Result;
use common::*;
use vault_mirror::{
    crypto::KdfParams,
    fixtures,
    mirror::{Endpoint, MirrorOptions},
    node::Node,
    restore::{RestoreOptions, restore},
    snapshot::{Restorable, RestorableItem},
    vault::{Account, Collection, Draft, Scope, Vault},
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

    let options = MirrorOptions::default();
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
    // Both organizations start with the collection the server creates.
    assert_eq!(first.summary.collections_deleted, 0);
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

    // Move an item to another collection, edit one, and drop a collection.
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

    let second = run_scoped(
        &source,
        source_scope.clone(),
        &dest,
        dest_scope.clone(),
        &options,
    )
    .await?;
    assert_eq!(second.summary.items_updated, 2);
    assert_eq!(second.summary.collections_deleted, 1);
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
    assert!(vault.collections.iter().all(|c| c.name != "Empty team"));
    assert!(
        !run_scoped(&source, source_scope, &dest, dest_scope, &options)
            .await?
            .changed
    );
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
    seed(&source).await?;
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
        format!("{error:#}").contains("1 collection(s) and 0 item(s)"),
        "{error:#}"
    );
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
            format!("{error:#}").contains("0 collection(s) and 1 item(s)"),
            "{error:#}"
        );
        return Ok(());
    }

    source.create_item(&source_scope, &note(vec![])).await?;
    let options = MirrorOptions::default();
    let first = run_scoped(
        &source,
        source_scope.clone(),
        &dest,
        dest_scope.clone(),
        &options,
    )
    .await?;
    if dest.server_traits().await?.items_need_collections {
        assert_eq!(first.summary.items_created, 0);
        assert!(
            first
                .warnings
                .iter()
                .any(|w| w.contains("in no collection")),
            "{:?}",
            first.warnings
        );
    } else {
        assert_eq!(first.summary.items_created, 1);
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    }
    assert!(
        !run_scoped(&source, source_scope, &dest, dest_scope, &options)
            .await?
            .changed
    );
    Ok(())
}

/// A restore leaves out what the target server would accept but not show,
/// and says so, instead of reporting items that cannot be found.
#[tokio::test]
async fn a_restore_leaves_out_items_the_target_would_hide() -> Result<()> {
    let Some(sandbox) = sandbox().await else {
        return Ok(());
    };
    let (dest, dest_scope, _) = organization(&sandbox, &sandbox.dest_url).await?;
    let note = |id: &str, name: &str, collection_ids: Vec<String>| RestorableItem {
        id: id.to_string(),
        folder_id: None,
        collection_ids,
        archived: false,
        content: [
            ("type".to_string(), Node::Number(2.into())),
            ("name".to_string(), Node::secret(name)),
            (
                "secureNote".to_string(),
                Node::Map([("type".to_string(), Node::Number(0.into()))].into()),
            ),
        ]
        .into(),
        attachments: vec![],
    };
    let snapshot = Restorable {
        organization: true,
        collections: vec![Collection {
            id: "c1".to_string(),
            name: "Shared".to_string(),
            external_id: None,
        }],
        items: vec![
            note("i1", "Loose note", vec![]),
            note("i2", "Shared note", vec!["c1".to_string()]),
        ],
        ..Restorable::default()
    };
    let capture = Capture::default();
    let report = restore(
        &Endpoint {
            account: &dest,
            scope: dest_scope.clone(),
        },
        &snapshot,
        None,
        &RestoreOptions {
            allow_non_empty: false,
            concurrency: 1,
        },
        &capture,
    )
    .await?;

    let restored = dest.load(&dest_scope).await?;
    assert_eq!(restored.items.len(), report.items);
    if dest.server_traits().await?.items_need_collections {
        assert_eq!((report.items, report.items_left_out), (1, 1));
        assert!(
            capture
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.starts_with("warn:") && line.contains("1 item(s)")),
        );
    } else {
        assert_eq!((report.items, report.items_left_out), (2, 0));
    }
    Ok(())
}
