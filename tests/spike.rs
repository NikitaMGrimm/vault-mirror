use std::collections::BTreeMap;

use serde_json::json;
use vault_mirror::{
    api::{ClientOptions, ServerUrls, build_http_client},
    crypto::KdfParams,
    node::Node,
    provision,
    vault::{Account, Draft, Scope},
};

#[tokio::test]
async fn spike() -> anyhow::Result<()> {
    let http = build_http_client(&ClientOptions::default())?;
    let server = "http://127.0.0.1:18081";
    let email = format!("spike-{}@example.com", uuid::Uuid::new_v4().simple());
    let kdf = KdfParams::Pbkdf2 { iterations: 600_000 };
    let t = std::time::Instant::now();
    let acct = provision::register(&http, server, &email, "master-pass-1", &kdf).await?;
    println!("registered {} in {:?}", acct.user_id, t.elapsed());

    let t = std::time::Instant::now();
    let account = Account::open(http.clone(), ServerUrls::from_server(server)?, acct.credentials.clone(), "master-pass-1").await?;
    println!("opened in {:?}", t.elapsed());
    let scope = Scope::Personal;
    let folder = account.create_folder("Work/Infra").await?;

    let mut content = BTreeMap::new();
    content.insert("type".to_string(), Node::Number(1.into()));
    content.insert("name".to_string(), Node::secret("GitHub"));
    content.insert("notes".to_string(), Node::secret("some notes"));
    content.insert("favorite".to_string(), Node::Bool(true));
    content.insert("reprompt".to_string(), Node::Number(0.into()));
    let login: BTreeMap<String, Node> = [
        ("username".to_string(), Node::secret("octocat")),
        ("password".to_string(), Node::secret("hunter2")),
        ("totp".to_string(), Node::Null),
    ].into();
    content.insert("login".to_string(), Node::Map(login));
    let draft = Draft { content, folder_id: Some(folder.id.clone()), collection_ids: vec![] };
    let created = account.create_item(&scope, &draft).await?;
    println!("created: {created:#?}");

    account.upload_attachment(&scope, &created, "hello.txt", b"hello attachment").await?;
    let vault = account.load(&scope).await?;
    println!("vault: {} folders {} items", vault.folders.len(), vault.items.len());
    let item = &vault.items[0];
    println!("loaded: {item:#?}");
    let data = account.download_attachment(&scope, item, &item.attachments[0]).await?;
    assert_eq!(data, b"hello attachment");

    let mut draft2 = draft.clone();
    draft2.content.insert("name".into(), Node::secret("GitHub 2"));
    let updated = account.update_item(&scope, item, &draft2).await?;
    assert_eq!(updated.name(), "GitHub 2");
    account.set_archived(&[updated.id.clone()], true).await?;
    let vault = account.load(&scope).await?;
    assert!(vault.items[0].archived);
    println!("normalized: {}", json!(Node::Map(vault.items[0].content.clone()).normalized()));
    account.delete_attachment(&updated.id, &vault.items[0].attachments[0].id).await?;
    account.delete_items(&[updated.id.clone()]).await?;
    account.delete_folder(&folder.id).await?;
    let vault = account.load(&scope).await?;
    assert!(vault.items.is_empty() && vault.folders.is_empty());

    let org = provision::create_organization(&http, &acct, "Spike Org").await?;
    let account = Account::open(http.clone(), ServerUrls::from_server(server)?, acct.credentials.clone(), "master-pass-1").await?;
    let oscope = Scope::Organization(org.clone());
    let v = account.load(&oscope).await?;
    println!("org vault: {v:?}");
    let col = account.create_collection(&org, "Team A", Some("ext-1")).await?;
    let mut d = draft.clone();
    d.folder_id = None;
    d.collection_ids = vec![col.id.clone()];
    let oitem = account.create_item(&oscope, &d).await?;
    account.upload_attachment(&oscope, &oitem, "org.bin", &[1, 2, 3]).await?;
    let v = account.load(&oscope).await?;
    println!("org vault: {v:#?}");
    let data = account.download_attachment(&oscope, &v.items[0], &v.items[0].attachments[0]).await?;
    assert_eq!(data, [1, 2, 3]);
    Ok(())
}
