//! Creates two throwaway accounts on the sandbox servers and prints a settings
//! file for them, for trying vault-mirror by hand:
//!
//! ```sh
//! sandbox/up.sh
//! cargo run --example sandbox -- --seed > sandbox.env
//! cargo run -- --env-file sandbox.env run
//! ```

#[path = "../tests/common/fixtures.rs"]
mod fixtures;
#[allow(dead_code)]
#[path = "../tests/common/provision.rs"]
mod provision;

use std::collections::HashMap;

use anyhow::Result;
use clap::Parser;
use vault_mirror::{
    api::{self, ServerUrls},
    crypto::KdfParams,
    vault::{Account, Draft, Scope},
};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:18081")]
    source: String,
    #[arg(long, default_value = "http://127.0.0.1:18082")]
    dest: String,
    /// Fill the source vault with sample items.
    #[arg(long)]
    seed: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let http = api::build_http_client(&api::ClientOptions::default())?;
    let kdf = KdfParams::Pbkdf2 {
        iterations: 600_000,
    };
    let password = "sandbox-master-password";
    let mut lines = Vec::new();
    for (prefix, server) in [("SOURCE", &args.source), ("DEST", &args.dest)] {
        let email = format!("sandbox-{}@example.com", uuid::Uuid::new_v4().simple());
        let account = provision::register(&http, server, &email, password, &kdf).await?;
        if args.seed && prefix == "SOURCE" {
            let urls = ServerUrls::from_server(server)?;
            let opened =
                Account::open(http.clone(), urls, account.credentials.clone(), password).await?;
            seed(&opened).await?;
        }
        lines.push(format!("{prefix}_SERVER={server}"));
        lines.push(format!("{prefix}_PASSWORD={password}"));
        lines.push(format!(
            "{prefix}_CLIENT_ID={}",
            account.credentials.client_id
        ));
        lines.push(format!(
            "{prefix}_CLIENT_SECRET={}",
            account.credentials.client_secret
        ));
    }
    lines.push("BACKUP_PASSPHRASE=sandbox-backup-passphrase".to_string());
    println!("{}", lines.join("\n"));
    Ok(())
}

async fn seed(account: &Account) -> Result<()> {
    let mut folders = HashMap::new();
    for name in fixtures::FOLDERS {
        folders.insert(*name, account.create_folder(name).await?.id);
    }
    for sample in fixtures::samples() {
        let draft = Draft {
            content: sample.content,
            folder_id: sample.folder.map(|name| folders[name].clone()),
            collection_ids: Vec::new(),
        };
        let item = account.create_item(&Scope::Personal, &draft).await?;
        for (name, data) in &sample.attachments {
            account
                .upload_attachment(&Scope::Personal, &item, name, data)
                .await?;
        }
        if sample.archived {
            account
                .set_archived(std::slice::from_ref(&item.id), true)
                .await?;
        }
    }
    Ok(())
}
