//! `vault-mirror init`: asks for what a first setup needs, checks the
//! logins, and writes `vault-mirror.env` and `compose.yaml`.

use std::{
    collections::HashMap,
    fs,
    io::{IsTerminal, Write},
    path::Path,
    process::ExitCode,
};

use anyhow::{Result, anyhow, bail};
use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};
use vault_mirror::{app, config::Config};

use crate::{Console, check_config, describe_vault};

pub const ENV_FILE: &str = "vault-mirror.env";
const COMPOSE_FILE: &str = "compose.yaml";

/// Bitwarden's cloud regions, then any other server.
const SERVERS: &[(&str, Option<&str>)] = &[
    (
        "Bitwarden cloud, US (vault.bitwarden.com)",
        Some("https://vault.bitwarden.com"),
    ),
    (
        "Bitwarden cloud, EU (vault.bitwarden.eu)",
        Some("https://vault.bitwarden.eu"),
    ),
    ("Self-hosted Bitwarden or Vaultwarden", None),
];

/// The schedule choices and the cron expression each one writes.
const SCHEDULES: &[(&str, &str)] = &[
    ("Every day at 03:00", "0 3 * * *"),
    ("Every 6 hours", "0 */6 * * *"),
    ("Every hour", "0 * * * *"),
];

pub async fn init(dir: &Path) -> Result<ExitCode> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "init asks questions, so it needs a terminal. To write the settings by hand, see \
             https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/configuration.md"
        );
    }
    let dir = &dir
        .canonicalize()
        .map_err(|error| anyhow!("{}: {error}", dir.display()))?;
    let env_path = dir.join(ENV_FILE);
    let compose_path = dir.join(COMPOSE_FILE);
    for path in [&env_path, &compose_path] {
        if path.exists() {
            bail!("{} already exists; move it away first", path.display());
        }
    }
    println!(
        "This writes {ENV_FILE} and {COMPOSE_FILE} to {}.\n\
         Each account needs a personal API key: in the web vault, open\n\
         Settings → Security → Keys → View API key.\n",
        dir.display()
    );

    let theme = ColorfulTheme::default();
    // The logins below record their device ids here, where the scheduled
    // runs will find them, so the server sees one device throughout.
    let backups = dir.join("backups");
    // The passphrase is asked for last; until then a placeholder makes
    // the settings complete enough to log in with. Nothing is encrypted.
    let mut env: HashMap<String, String> = HashMap::from([
        ("BACKUPS_DIR".into(), backups.display().to_string()),
        ("BACKUP_PASSPHRASE".into(), "placeholder".into()),
    ]);
    let mut written: Vec<(&str, String)> = Vec::new();
    // A server with a private CA needs it for the logins below too.
    if let Ok(path) = std::env::var("CA_CERT_FILE") {
        env.insert("CA_CERT_FILE".into(), path.clone());
        written.push(("CA_CERT_FILE", path));
    }

    let source = ask_vault(&theme, &mut env, "SOURCE", "the source").await?;
    written.extend(source);
    let mirror = Confirm::with_theme(&theme)
        .with_prompt("Mirror the source into a second account?")
        .default(true)
        .interact()?;
    if mirror {
        println!(
            "vault-mirror keeps a copy of every source item in this account. It leaves \
             everything else in the account as it is."
        );
        let dest = ask_vault(&theme, &mut env, "DEST", "the destination").await?;
        written.extend(dest);
    }

    let choice = Select::with_theme(&theme)
        .with_prompt("How often should it run?")
        .items(SCHEDULES.iter().map(|(label, ..)| *label))
        .default(0)
        .interact()?;
    written.push(("SCHEDULE", SCHEDULES[choice].1.to_string()));
    let zone: String = Input::with_theme(&theme)
        .with_prompt("Time zone")
        .default(local_timezone())
        .validate_with(|zone: &String| {
            zone.parse::<chrono_tz::Tz>()
                .map(|_| ())
                .map_err(|_| "not a time zone name, such as Europe/Berlin")
        })
        .interact_text()?;
    written.push(("TZ", zone));

    let generate = Select::with_theme(&theme)
        .with_prompt("Backups are encrypted with a passphrase")
        .items(["Generate one", "Enter my own"])
        .default(0)
        .interact()?
        == 0;
    let passphrase = if generate {
        generate_passphrase()?
    } else {
        Password::with_theme(&theme)
            .with_prompt("Backup passphrase")
            .with_confirmation("Repeat it", "The two entries differ")
            .interact()?
    };
    written.push(("BACKUP_PASSPHRASE", passphrase));

    write_new(&env_path, &env_file(&written), true)?;
    if readable_only_by_owner(&env_path) {
        Console::status("Created", &format!("{ENV_FILE}, readable only by you"));
    } else {
        Console::status("Created", ENV_FILE);
        Console::warning(&format!(
            "{ENV_FILE} holds your passwords and API keys, and other accounts on this \
             computer may be able to read it; make it readable only by you"
        ));
    }
    write_new(&compose_path, &compose_file(dir)?, false)?;
    Console::status("Created", COMPOSE_FILE);
    Console::note(
        "keep BACKUP_PASSPHRASE outside the backups and vault. After the first run, \
         copy backups/key.age there too. Recovery needs both.",
    );

    if mirror {
        let mut settings = env.clone();
        settings.extend(written.iter().map(|(k, v)| (k.to_string(), v.clone())));
        let config = Config::from_env(&settings).map_err(|error| anyhow!("{error}"))?;
        // The files are written either way; a safety stop here is about the
        // vaults' contents, not the settings.
        if let Err(error) = check_config(&config, false).await {
            Console::error(&format!("{error:#}"));
            println!(
                "\nOnce that is resolved, check again with `docker compose run --rm vault-mirror check`."
            );
            return Ok(ExitCode::FAILURE);
        }
    }
    println!(
        "\nStart it with `docker compose up -d`, or without Docker with\n\
         `vault-mirror --env-file {ENV_FILE} daemon`."
    );
    Ok(ExitCode::SUCCESS)
}

/// Asks for one account until its login works, and returns its settings.
async fn ask_vault(
    theme: &ColorfulTheme,
    env: &mut HashMap<String, String>,
    prefix: &str,
    role: &str,
) -> Result<Vec<(&'static str, String)>> {
    let names = match prefix {
        "SOURCE" => [
            "SOURCE_SERVER",
            "SOURCE_CLIENT_ID",
            "SOURCE_CLIENT_SECRET",
            "SOURCE_PASSWORD",
        ],
        _ => [
            "DEST_SERVER",
            "DEST_CLIENT_ID",
            "DEST_CLIENT_SECRET",
            "DEST_PASSWORD",
        ],
    };
    println!();
    loop {
        let choice = Select::with_theme(theme)
            .with_prompt(format!("Server of {role}"))
            .items(SERVERS.iter().map(|(label, _)| *label))
            .default(0)
            .interact()?;
        let server = match SERVERS[choice].1 {
            Some(url) => url.to_string(),
            None => Input::with_theme(theme)
                .with_prompt("Server URL")
                .with_initial_text(env.get(names[0]).cloned().unwrap_or_default())
                .validate_with(|url: &String| {
                    if url.starts_with("https://") || url.starts_with("http://") {
                        Ok(())
                    } else {
                        Err("start with https://")
                    }
                })
                .interact_text()?,
        };
        let client_id: String = Input::with_theme(theme)
            .with_prompt("client_id")
            .with_initial_text(env.get(names[1]).cloned().unwrap_or_default())
            .interact_text()?;
        let client_secret = Password::with_theme(theme)
            .with_prompt("client_secret")
            .interact()?;
        let password = Password::with_theme(theme)
            .with_prompt("Master password")
            .interact()?;
        let values = [
            server,
            client_id.trim().to_string(),
            client_secret.trim().to_string(),
            password,
        ];
        for (name, value) in names.iter().zip(&values) {
            env.insert(name.to_string(), value.clone());
        }
        match log_in(env, prefix, role).await {
            Ok(()) => return Ok(names.into_iter().zip(values).collect()),
            Err(error) => {
                Console::error(&format!("{error:#}"));
                let again = Confirm::with_theme(theme)
                    .with_prompt("Try again?")
                    .default(true)
                    .interact()?;
                if !again {
                    bail!("the login to {role} did not work");
                }
            }
        }
    }
}

async fn log_in(env: &HashMap<String, String>, prefix: &str, role: &str) -> Result<()> {
    let config = Config::from_env(env).map_err(|error| anyhow!("{error}"))?;
    let vault = match prefix {
        "SOURCE" => &config.source,
        _ => config
            .dest
            .as_ref()
            .ok_or_else(|| anyhow!("no destination settings"))?,
    };
    // A personal API key's client id names its account on any server address.
    if prefix == "DEST" && vault.client_id == config.source.client_id {
        bail!("this is the source account; the destination must be another account");
    }
    let http = app::http_client(&config)?;
    let account =
        app::open_account(&config.backups_dir, &http, vault, &prefix.to_lowercase()).await?;
    let loaded = account.load(&app::scope_of(vault)).await?;
    Console::status(
        "Logged in",
        &format!("to {role}: {}", describe_vault(&loaded)),
    );
    Ok(())
}

/// The settings as `NAME=value` lines, taken literally: no quotes, no
/// variables. Docker Compose reads them with `format: raw`, and
/// `--env-file` the same way.
fn env_file(settings: &[(&str, String)]) -> String {
    let mut text = String::from(
        "# Written by `vault-mirror init`. Values are taken literally, without quotes.\n\
         # Every setting: https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/configuration.md\n",
    );
    let mut last_group = "";
    for (name, value) in settings {
        let group = match name.split('_').next().unwrap_or_default() {
            "TZ" => "SCHEDULE",
            group => group,
        };
        if group != last_group {
            text.push('\n');
        }
        last_group = group;
        text.push_str(&format!("{name}={value}\n"));
    }
    text
}

fn compose_file(dir: &Path) -> Result<String> {
    let version = env!("CARGO_PKG_VERSION");
    let tag = version
        .rsplit_once('.')
        .map_or(version, |(major_minor, _)| major_minor);
    // A directory Docker creates for a bind mount belongs to root, and the
    // image runs unprivileged, so the container runs as the directory's owner.
    let user = owner(&dir.join("backups"))?
        .map(|(uid, gid)| format!("    user: \"{uid}:{gid}\"\n"))
        .unwrap_or_default();
    Ok(format!(
        "services:\n  \
           vault-mirror:\n    \
             image: ghcr.io/nikitamgrimm/vault-mirror:{tag}\n    \
             command: daemon\n\
         {user}    \
             env_file:\n      \
               - path: {ENV_FILE}\n        \
                 format: raw\n    \
             volumes:\n      \
               - ./backups:/data\n    \
             restart: unless-stopped\n    \
             # Gives a run in progress up to 5 minutes to finish when the container stops.\n    \
             stop_grace_period: 5m\n"
    ))
}

#[cfg(unix)]
fn owner(path: &Path) -> Result<Option<(u32, u32)>> {
    use std::os::unix::fs::MetadataExt;
    fs::create_dir_all(path)?;
    let metadata = fs::metadata(path)?;
    Ok(Some((metadata.uid(), metadata.gid())))
}

#[cfg(not(unix))]
fn owner(path: &Path) -> Result<Option<(u32, u32)>> {
    fs::create_dir_all(path)?;
    Ok(None)
}

#[cfg(unix)]
fn readable_only_by_owner(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // Some file systems, such as a Windows drive mounted in WSL, ignore the
    // mode a file is created with.
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o077 == 0)
}

/// Windows access lists are not checked, so the file is never called private.
#[cfg(not(unix))]
fn readable_only_by_owner(_: &Path) -> bool {
    false
}

/// Writes a file that must not exist yet, readable only by its owner if
/// `private`.
fn write_new(path: &Path, content: &str, private: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if private { 0o600 } else { 0o644 });
    }
    let mut file = options
        .open(path)
        .map_err(|error| anyhow!("creating {}: {error}", path.display()))?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

/// 30 characters from a 32-letter alphabet: 150 random bits, in groups of
/// five without look-alike characters.
fn generate_passphrase() -> Result<String> {
    const ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 30];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("no randomness: {error}"))?;
    let letters: Vec<char> = bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(byte % 32)]))
        .collect();
    Ok(letters
        .chunks(5)
        .map(|group| group.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-"))
}

/// The machine's time zone name, or UTC.
fn local_timezone() -> String {
    let from_link = || {
        let target = fs::read_link("/etc/localtime").ok()?;
        let target = target.to_string_lossy();
        let (_, name) = target.split_once("zoneinfo/")?;
        Some(name.to_string())
    };
    std::env::var("TZ")
        .ok()
        .or_else(|| {
            fs::read_to_string("/etc/timezone")
                .ok()
                .map(|zone| zone.trim().to_string())
        })
        .or_else(from_link)
        .filter(|zone| zone.parse::<chrono_tz::Tz>().is_ok())
        .unwrap_or_else(|| "UTC".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passphrases_are_long_and_differ() {
        let first = generate_passphrase().unwrap();
        assert_eq!(first.len(), 35);
        assert_eq!(first.split('-').count(), 6);
        assert_ne!(first, generate_passphrase().unwrap());
    }

    #[test]
    fn the_compose_file_lets_a_run_finish_on_stop() {
        let dir = tempfile::tempdir().unwrap();
        let compose = compose_file(dir.path()).unwrap();
        assert!(compose.contains("\n    command: daemon\n"), "{compose}");
        assert!(
            compose.contains("\n    stop_grace_period: 5m\n"),
            "{compose}"
        );
    }

    #[test]
    fn the_env_file_takes_values_literally() {
        let text = env_file(&[
            ("SOURCE_SERVER", "https://vault.bitwarden.com".into()),
            ("SOURCE_PASSWORD", "it's $HOME \"#x\" ".into()),
            ("SCHEDULE", "0 3 * * *".into()),
            ("TZ", "Europe/Berlin".into()),
        ]);
        assert!(text.contains("\nSOURCE_PASSWORD=it's $HOME \"#x\" \n"));
        let parsed = crate::parse_env_file(&text).unwrap();
        assert_eq!(parsed["SOURCE_PASSWORD"], "it's $HOME \"#x\" ");
        assert_eq!(parsed["SCHEDULE"], "0 3 * * *");
    }

    #[test]
    fn the_settings_file_is_called_private_only_when_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ENV_FILE);
        write_new(&path, "", true).unwrap();
        assert_eq!(readable_only_by_owner(&path), cfg!(unix));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(!readable_only_by_owner(&path));
        }
    }
}
