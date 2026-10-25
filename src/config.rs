//! Configuration, read from environment variables.

use std::{collections::HashMap, fs, path::PathBuf, str::FromStr};

use crate::mirror::Filter;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ConfigError(String);

type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Clone)]
pub struct VaultConfig {
    pub server: String,
    pub client_id: String,
    pub client_secret: String,
    pub password: String,
    pub organization_id: Option<String>,
}

#[derive(Clone)]
pub struct BackupConfig {
    pub passphrase: String,
    pub retention_days: u32,
}

#[derive(Clone)]
pub struct Config {
    pub source: VaultConfig,
    /// Absent for a backup-only setup.
    pub dest: Option<VaultConfig>,
    pub backups_dir: PathBuf,
    pub backup: BackupConfig,
    pub filter: Filter,
    pub skip_attachments: bool,
    pub allow_organization_metadata_loss: bool,
    pub ca_cert_file: Option<PathBuf>,
    /// When `daemon` runs: a five- or six-field cron expression, evaluated in
    /// `timezone`.
    pub schedule: Option<String>,
    pub timezone: Option<String>,
    pub notify_success_url: Option<String>,
    pub notify_failure_url: Option<String>,
}

struct Env<'a>(&'a HashMap<String, String>);

impl Env<'_> {
    fn get(&self, name: &str) -> Option<String> {
        self.0
            .get(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// Reads `NAME`, or the content of the file named by `NAME_FILE`, which
    /// is how container secrets are usually delivered.
    ///
    /// The value is taken verbatim: spaces at either end of a password are
    /// part of it.
    fn secret(&self, name: &str) -> Result<Option<String>> {
        let file_var = format!("{name}_FILE");
        let inline = self.0.get(name).filter(|value| !value.is_empty()).cloned();
        match (inline, self.get(&file_var)) {
            (Some(_), Some(_)) => Err(ConfigError(format!(
                "set either {name} or {file_var}, not both"
            ))),
            (Some(value), None) => Ok(Some(value)),
            (None, Some(path)) => {
                let content = fs::read_to_string(&path).map_err(|error| {
                    ConfigError(format!("{file_var}: cannot read {path}: {error}"))
                })?;
                // A trailing newline is an artefact of how the file was written.
                let value = content.trim_end_matches(['\r', '\n']).to_string();
                if value.is_empty() {
                    return Err(ConfigError(format!("{file_var}: {path} is empty")));
                }
                Ok(Some(value))
            }
            (None, None) => Ok(None),
        }
    }

    /// A secret that cannot contain spaces at its ends, such as an API key or
    /// a URL, unlike a password.
    fn token(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .secret(name)?
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()))
    }

    fn parse<T: FromStr>(&self, name: &str, default: T) -> Result<T> {
        match self.get(name) {
            None => Ok(default),
            Some(raw) => raw
                .parse()
                .map_err(|_| ConfigError(format!("{name} has an invalid value"))),
        }
    }

    /// A comma-separated list of folder or collection names.
    fn names(&self, name: &str) -> Result<Vec<String>> {
        let Some(list) = self.0.get(name) else {
            return Ok(Vec::new());
        };
        let names: Vec<String> = list
            .split(',')
            .map(|part| part.trim().trim_end_matches('/').to_string())
            .collect();
        if names.iter().any(String::is_empty) {
            return Err(ConfigError(format!(
                "{name} must contain nonempty names separated by commas; unset it to remove this filter"
            )));
        }
        Ok(names)
    }

    fn vault(&self, prefix: &str) -> Result<Option<VaultConfig>> {
        let name = |suffix: &str| format!("{prefix}_{suffix}");
        let server = self.get(&name("SERVER"));
        let client_id = self.token(&name("CLIENT_ID"))?;
        let client_secret = self.token(&name("CLIENT_SECRET"))?;
        let password = self.secret(&name("PASSWORD"))?;
        let organization_id = self.get(&name("ORGANIZATION_ID"));

        let present = [
            server.is_some(),
            client_id.is_some(),
            client_secret.is_some(),
            password.is_some(),
        ];
        if present.iter().all(|set| !set) && organization_id.is_none() {
            return Ok(None);
        }
        let require = |value: Option<String>, suffix: &str| {
            value.ok_or_else(|| ConfigError(format!("{} is not set", name(suffix))))
        };
        Ok(Some(VaultConfig {
            server: require(server, "SERVER")?,
            client_id: require(client_id, "CLIENT_ID")?,
            client_secret: require(client_secret, "CLIENT_SECRET")?,
            password: require(password, "PASSWORD")?,
            organization_id,
        }))
    }
}

impl Config {
    pub fn from_env(env: &HashMap<String, String>) -> Result<Self> {
        let env = Env(env);
        let source = env.vault("SOURCE")?.ok_or_else(|| {
            ConfigError("no source vault configured: set SOURCE_SERVER, SOURCE_CLIENT_ID, SOURCE_CLIENT_SECRET and SOURCE_PASSWORD".into())
        })?;
        let dest = env.vault("DEST")?;
        if let Some(dest) = &dest
            && source.organization_id.is_some() != dest.organization_id.is_some()
        {
            return Err(ConfigError(
                    "set both SOURCE_ORGANIZATION_ID and DEST_ORGANIZATION_ID to mirror an organization, or neither".into(),
                ));
        }

        let backup = BackupConfig {
            passphrase: env
                .secret("BACKUP_PASSPHRASE")?
                .ok_or_else(|| ConfigError("BACKUP_PASSPHRASE is not set".into()))?,
            retention_days: env.parse("RETENTION_DAYS", 30)?,
        };

        let folders = Filter {
            include: env.names("INCLUDE_FOLDERS")?,
            exclude: env.names("EXCLUDE_FOLDERS")?,
        };
        let collections = Filter {
            include: env.names("INCLUDE_COLLECTIONS")?,
            exclude: env.names("EXCLUDE_COLLECTIONS")?,
        };
        let filter = match (source.organization_id.is_some(), folders, collections) {
            (false, folders, collections) if collections.is_empty() => folders,
            (true, folders, collections) if folders.is_empty() => collections,
            (false, ..) => {
                return Err(ConfigError(
                    "INCLUDE_COLLECTIONS and EXCLUDE_COLLECTIONS apply to organizations; use INCLUDE_FOLDERS and EXCLUDE_FOLDERS for an individual vault".into(),
                ));
            }
            (true, ..) => {
                return Err(ConfigError(
                    "organizations are mirrored without folders; use INCLUDE_COLLECTIONS and EXCLUDE_COLLECTIONS".into(),
                ));
            }
        };

        Ok(Self {
            source,
            dest,
            backups_dir: backups_dir(env.0),
            backup,
            filter,
            skip_attachments: env.parse("SKIP_ATTACHMENTS", false)?,
            allow_organization_metadata_loss: env
                .parse("ALLOW_ORGANIZATION_METADATA_LOSS", false)?,
            ca_cert_file: env.get("CA_CERT_FILE").map(PathBuf::from),
            schedule: env.get("SCHEDULE"),
            timezone: env.get("TZ"),
            notify_success_url: env.token("NOTIFY_SUCCESS_URL")?,
            notify_failure_url: env.token("NOTIFY_FAILURE_URL")?,
        })
    }
}

/// `BACKUPS_DIR`, on its own: `healthcheck` needs it without the rest of the
/// configuration.
pub fn backups_dir(env: &HashMap<String, String>) -> PathBuf {
    PathBuf::from(
        Env(env)
            .get("BACKUPS_DIR")
            .or_else(|| Env(env).get("VAULT_MIRROR_DEFAULT_BACKUPS_DIR"))
            .unwrap_or_else(|| "backups".into()),
    )
}

pub fn backup_passphrase(env: &HashMap<String, String>) -> Result<Option<String>> {
    Env(env).secret("BACKUP_PASSPHRASE")
}

/// Whether a name is supported in an explicit settings file.
pub fn is_setting(name: &str) -> bool {
    let secret = name.strip_suffix("_FILE").unwrap_or(name);
    matches!(
        secret,
        "SOURCE_CLIENT_ID"
            | "SOURCE_CLIENT_SECRET"
            | "SOURCE_PASSWORD"
            | "DEST_CLIENT_ID"
            | "DEST_CLIENT_SECRET"
            | "DEST_PASSWORD"
            | "BACKUP_PASSPHRASE"
            | "NOTIFY_SUCCESS_URL"
            | "NOTIFY_FAILURE_URL"
    ) || matches!(
        name,
        "SOURCE_SERVER"
            | "SOURCE_ORGANIZATION_ID"
            | "DEST_SERVER"
            | "DEST_ORGANIZATION_ID"
            | "BACKUPS_DIR"
            | "RETENTION_DAYS"
            | "INCLUDE_FOLDERS"
            | "EXCLUDE_FOLDERS"
            | "INCLUDE_COLLECTIONS"
            | "EXCLUDE_COLLECTIONS"
            | "SKIP_ATTACHMENTS"
            | "ALLOW_ORGANIZATION_METADATA_LOSS"
            | "CA_CERT_FILE"
            | "SCHEDULE"
            | "TZ"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![
            ("SOURCE_SERVER", "https://vault.bitwarden.com"),
            ("SOURCE_CLIENT_ID", "user.source"),
            ("SOURCE_CLIENT_SECRET", "source-secret"),
            ("SOURCE_PASSWORD", " source password "),
            ("DEST_SERVER", "https://vw.example.com"),
            ("DEST_CLIENT_ID", "user.dest"),
            ("DEST_CLIENT_SECRET", "dest-secret"),
            ("DEST_PASSWORD", "dest password"),
            ("BACKUP_PASSPHRASE", "backup passphrase"),
        ]
    }

    fn with(extra: &[(&'static str, &'static str)]) -> HashMap<String, String> {
        let mut pairs = base();
        pairs.extend_from_slice(extra);
        env(&pairs)
    }

    fn without(names: &[&str]) -> HashMap<String, String> {
        let pairs: Vec<_> = base()
            .into_iter()
            .filter(|(name, _)| !names.contains(name))
            .collect();
        env(&pairs)
    }

    fn error(env: HashMap<String, String>) -> String {
        Config::from_env(&env).err().expect("must be rejected").0
    }

    #[test]
    fn the_required_variables_are_enough() {
        let config = Config::from_env(&with(&[])).unwrap();
        assert_eq!(config.source.server, "https://vault.bitwarden.com");
        assert_eq!(config.source.password, " source password ");
        assert_eq!(config.dest.as_ref().unwrap().client_id, "user.dest");
        assert_eq!(config.backup.passphrase, "backup passphrase");
        assert_eq!(config.backup.retention_days, 30);
        assert!(config.schedule.is_none());
        assert_eq!(config.backups_dir, PathBuf::from("backups"));
    }

    #[test]
    fn organization_metadata_loss_requires_a_boolean_setting() {
        for value in ["yes", "1", "TRUE", "maybe"] {
            let message = error(with(&[("ALLOW_ORGANIZATION_METADATA_LOSS", value)]));
            assert!(message.contains("ALLOW_ORGANIZATION_METADATA_LOSS has an invalid value"));
        }
        for (value, expected) in [("false", false), ("true", true)] {
            let config =
                Config::from_env(&with(&[("ALLOW_ORGANIZATION_METADATA_LOSS", value)])).unwrap();
            assert_eq!(config.allow_organization_metadata_loss, expected);
        }
    }

    #[test]
    fn organization_metadata_loss_permission_combines_flags_and_daemon_settings() {
        use crate::app::{RunFlags, mirror_options};

        let config = Config::from_env(&with(&[])).unwrap();
        assert!(!mirror_options(&config, RunFlags::default()).allow_organization_metadata_loss);
        let flags = RunFlags {
            allow_organization_metadata_loss: true,
            ..RunFlags::default()
        };
        assert!(mirror_options(&config, flags).allow_organization_metadata_loss);

        let config =
            Config::from_env(&with(&[("ALLOW_ORGANIZATION_METADATA_LOSS", "true")])).unwrap();
        assert!(mirror_options(&config, RunFlags::default()).allow_organization_metadata_loss);
    }

    #[test]
    fn a_setup_without_a_destination_only_backs_up() {
        let config = Config::from_env(&without(&[
            "DEST_SERVER",
            "DEST_CLIENT_ID",
            "DEST_CLIENT_SECRET",
            "DEST_PASSWORD",
        ]))
        .unwrap();
        assert!(config.dest.is_none());
    }

    #[test]
    fn a_half_configured_vault_names_what_is_missing() {
        assert_eq!(
            error(without(&["DEST_PASSWORD"])),
            "DEST_PASSWORD is not set"
        );
        assert_eq!(
            error(without(&["SOURCE_CLIENT_SECRET"])),
            "SOURCE_CLIENT_SECRET is not set"
        );
        assert!(error(env(&[("BACKUP_PASSPHRASE", "p")])).contains("no source vault configured"));
    }

    #[test]
    fn organizations_are_mirrored_as_a_pair() {
        assert!(error(with(&[("SOURCE_ORGANIZATION_ID", "org")])).contains("both"));
        let config = Config::from_env(&with(&[
            ("SOURCE_ORGANIZATION_ID", "a"),
            ("DEST_ORGANIZATION_ID", "b"),
        ]))
        .unwrap();
        assert_eq!(config.source.organization_id.as_deref(), Some("a"));
        assert_eq!(config.dest.unwrap().organization_id.as_deref(), Some("b"));
    }

    #[test]
    fn filters_match_the_kind_of_vault() {
        let config = Config::from_env(&with(&[
            ("INCLUDE_FOLDERS", "Work, Family/Shared/ "),
            ("EXCLUDE_FOLDERS", "Work/Private"),
        ]))
        .unwrap();
        assert_eq!(config.filter.include, ["Work", "Family/Shared"]);
        assert_eq!(config.filter.exclude, ["Work/Private"]);
        assert!(error(with(&[("EXCLUDE_COLLECTIONS", "x")])).contains("apply to organizations"));
        let organization = [
            ("SOURCE_ORGANIZATION_ID", "a"),
            ("DEST_ORGANIZATION_ID", "b"),
        ];
        let config = Config::from_env(&with(
            &[&organization[..], &[("INCLUDE_COLLECTIONS", "Team")]].concat(),
        ))
        .unwrap();
        assert_eq!(config.filter.include, ["Team"]);
        assert!(
            error(with(
                &[&organization[..], &[("INCLUDE_FOLDERS", "x")]].concat()
            ))
            .contains("without folders")
        );
    }

    #[test]
    fn a_present_filter_cannot_be_empty_or_have_empty_names() {
        for setting in [
            "INCLUDE_FOLDERS",
            "EXCLUDE_FOLDERS",
            "INCLUDE_COLLECTIONS",
            "EXCLUDE_COLLECTIONS",
        ] {
            for value in ["", "  ", ",", "Work,", "Work,,Home", "/"] {
                let message = error(with(&[(setting, value)]));
                assert!(message.contains(setting));
                assert!(!message.contains("Work") && !message.contains("Home"));
            }
        }
    }

    #[test]
    fn settings_files_recognize_only_supported_names_and_secret_file_variants() {
        for name in [
            "SOURCE_SERVER",
            "SOURCE_CLIENT_ID_FILE",
            "DEST_PASSWORD_FILE",
            "BACKUP_PASSPHRASE",
            "BACKUP_PASSPHRASE_FILE",
            "NOTIFY_FAILURE_URL_FILE",
            "CA_CERT_FILE",
            "ALLOW_ORGANIZATION_METADATA_LOSS",
            "TZ",
        ] {
            assert!(is_setting(name), "{name}");
        }
        for name in [
            "SOUCRE_PASSWORD",
            "SOURCE_SERVER_FILE",
            "RETENTION_DAYS_FILE",
            "BACKUP_PASSPHRASE_FILE_FILE",
            "PATH",
            "",
        ] {
            assert!(!is_setting(name), "{name}");
        }
        Config::from_env(&with(&[("PATH", "/bin"), ("UNRELATED_HOST_SETTING", "x")])).unwrap();
    }

    #[test]
    fn secrets_can_come_from_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("password");
        fs::write(&path, "from a file\n").unwrap();
        let mut vars = without(&["SOURCE_PASSWORD"]);
        vars.insert("SOURCE_PASSWORD_FILE".into(), path.display().to_string());
        assert_eq!(
            Config::from_env(&vars).unwrap().source.password,
            "from a file"
        );

        vars.insert("SOURCE_PASSWORD".into(), "inline".into());
        assert!(error(vars.clone()).contains("not both"));
        vars.remove("SOURCE_PASSWORD");
        vars.insert("SOURCE_PASSWORD_FILE".into(), "/nonexistent/secret".into());
        assert!(error(vars).contains("cannot read"));
    }

    #[test]
    fn passwords_from_the_environment_keep_their_spaces() {
        let config = Config::from_env(&with(&[
            ("DEST_SERVER", " https://vw.example.com/ "),
            ("DEST_CLIENT_SECRET", " api secret\n"),
            ("NOTIFY_FAILURE_URL", " https://ntfy.example.com/topic "),
            ("DEST_PASSWORD", "  "),
            ("BACKUP_PASSPHRASE", " backup passphrase\t"),
        ]))
        .unwrap();
        assert_eq!(config.source.password, " source password ");
        let dest = config.dest.unwrap();
        assert_eq!(dest.password, "  ");
        assert_eq!(dest.server, "https://vw.example.com/");
        assert_eq!(dest.client_secret, "api secret");
        assert_eq!(
            config.notify_failure_url.as_deref(),
            Some("https://ntfy.example.com/topic")
        );
        assert_eq!(config.backup.passphrase, " backup passphrase\t");
        assert!(error(with(&[("DEST_PASSWORD", "")])).contains("DEST_PASSWORD is not set"));
    }

    #[test]
    fn backups_need_a_passphrase() {
        assert_eq!(
            error(without(&["BACKUP_PASSPHRASE"])),
            "BACKUP_PASSPHRASE is not set"
        );
    }

    #[test]
    fn tuning_values_are_validated() {
        let config = Config::from_env(&with(&[("RETENTION_DAYS", "7")])).unwrap();
        assert_eq!(config.backup.retention_days, 7);
        assert!(error(with(&[("RETENTION_DAYS", "soon")])).contains("RETENTION_DAYS"));
    }

    #[test]
    fn attachments_are_copied_unless_skipped_explicitly() {
        assert!(!Config::from_env(&with(&[])).unwrap().skip_attachments);
        let config = Config::from_env(&with(&[("SKIP_ATTACHMENTS", "true")])).unwrap();
        assert!(config.skip_attachments);
        let config = Config::from_env(&with(&[("SKIP_ATTACHMENTS", "false")])).unwrap();
        assert!(!config.skip_attachments);
        assert_eq!(
            error(with(&[("SKIP_ATTACHMENTS", "yes")])),
            "SKIP_ATTACHMENTS has an invalid value"
        );
        assert!(is_setting("SKIP_ATTACHMENTS"));
        assert!(!is_setting("SKIP_ATTACHMENTS_FILE"));
    }
}
