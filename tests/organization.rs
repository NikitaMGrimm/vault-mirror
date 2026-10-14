mod common;

use anyhow::Result;
use common::*;
use vault_mirror::{
    crypto::KdfParams,
    fixtures,
    mirror::MirrorOptions,
    node::Node,
    provision,
    vault::{Account, Draft, Scope, Vault},
};

async fn organization(sandbox: &Sandbox, server: &str) -> Result<(Account, Scope, String)> {
    let owner = sandbox.register(server).await?;
    let org = provision::create_organization(&sandbox.http, &owner, "Mirror Test Org").await?;
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
