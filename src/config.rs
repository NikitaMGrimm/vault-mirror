//! Configuration, read from environment variables.

use std::{collections::HashMap, fs, path::PathBuf, str::FromStr, time::Duration};

use crate::{
    api::{ClientOptions, DEFAULT_CLIENT_VERSION},
    mirror::Filter,
    plan::Safety,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// A five- or six-field cron expression, evaluated in `timezone`.
    Cron(String),
    Every(Duration),
}

#[derive(Clone)]
pub struct Config {
    pub source: VaultConfig,
    /// Absent for a backup-only setup.
    pub dest: Option<VaultConfig>,
    pub backups_dir: PathBuf,
    pub backup: BackupConfig,
    pub safety: Safety,
    pub concurrency: usize,
    pub filter: Filter,
    pub client_version: String,
    pub ca_cert_file: Option<PathBuf>,
    pub schedule: Option<Schedule>,
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
                .map_err(|_| ConfigError(format!("{name} has an invalid value: {raw}"))),
        }
    }

    /// A comma-separated list of folder or collection names.
    fn names(&self, name: &str) -> Vec<String> {
        self.get(name)
            .map(|list| {
                list.split(',')
                    .map(|part| part.trim().trim_end_matches('/').to_string())
                    .filter(|part| !part.is_empty())
                    .collect()
            })
            .unwrap_or_default()
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

    fn concurrency(&self) -> Result<usize> {
        let concurrency = self.parse("CONCURRENCY", 4)?;
        if !(1..=32).contains(&concurrency) {
            return Err(ConfigError("CONCURRENCY must be between 1 and 32".into()));
        }
        Ok(concurrency)
    }
}

impl Config {
    /// The configured vaults with their roles: `source`, then `dest`.
    pub fn vaults(&self) -> impl Iterator<Item = (&VaultConfig, &'static str)> {
        std::iter::once((&self.source, "source")).chain(self.dest.iter().map(|dest| (dest, "dest")))
    }

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

        let max_delete_percent: Option<u32> = env
            .get("MAX_DELETE_PERCENT")
            .map(|_| env.parse("MAX_DELETE_PERCENT", 0))
            .transpose()?;
        if max_delete_percent.is_some_and(|percent| percent > 100) {
            return Err(ConfigError(
                "MAX_DELETE_PERCENT must be between 0 and 100".into(),
            ));
        }
        let folders = Filter {
            include: env.names("INCLUDE_FOLDERS"),
            exclude: env.names("EXCLUDE_FOLDERS"),
        };
        let collections = Filter {
            include: env.names("INCLUDE_COLLECTIONS"),
            exclude: env.names("EXCLUDE_COLLECTIONS"),
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
        let concurrency = env.concurrency()?;

        let schedule = match (env.get("SCHEDULE"), env.get("INTERVAL")) {
            (Some(_), Some(_)) => {
                return Err(ConfigError(
                    "set either SCHEDULE or INTERVAL, not both".into(),
                ));
            }
            (Some(cron), None) => Some(Schedule::Cron(cron)),
            (None, Some(interval)) => Some(Schedule::Every(parse_interval(&interval)?)),
            (None, None) => None,
        };

        Ok(Self {
            source,
            dest,
            backups_dir: backups_dir(env.0),
            backup,
            safety: Safety {
                max_delete_percent,
                ..Safety::default()
            },
            concurrency,
            filter,
            client_version: env
                .get("BITWARDEN_CLIENT_VERSION")
                .unwrap_or_else(|| DEFAULT_CLIENT_VERSION.into()),
            ca_cert_file: env.get("CA_CERT_FILE").map(PathBuf::from),
            schedule,
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
            .unwrap_or_else(|| "backups".into()),
    )
}

pub fn backup_passphrase(env: &HashMap<String, String>) -> Result<Option<String>> {
    Env(env).secret("BACKUP_PASSPHRASE")
}

pub fn vault_config(env: &HashMap<String, String>, prefix: &str) -> Result<VaultConfig> {
    Env(env)
        .vault(prefix)?
        .ok_or_else(|| ConfigError(format!("no {} vault is configured", prefix.to_lowercase())))
}

pub fn concurrency(env: &HashMap<String, String>) -> Result<usize> {
    Env(env).concurrency()
}

pub fn client_options(env: &HashMap<String, String>) -> Result<ClientOptions> {
    let env = Env(env);
    let mut options = ClientOptions {
        client_version: env
            .get("BITWARDEN_CLIENT_VERSION")
            .unwrap_or_else(|| DEFAULT_CLIENT_VERSION.into()),
        ..ClientOptions::default()
    };
    if let Some(path) = env.get("CA_CERT_FILE") {
        options.extra_root_certs.push(
            fs::read(&path)
                .map_err(|error| ConfigError(format!("reading CA_CERT_FILE {path}: {error}")))?,
        );
    }
    Ok(options)
}

/// Parses an interval such as `90s`, `30m`, `6h` or `1d`.
fn parse_interval(raw: &str) -> Result<Duration> {
    let invalid = || ConfigError(format!("INTERVAL must look like 30m, 6h or 1d, not {raw}"));
    let split = raw
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(invalid)?;
    let (number, unit) = raw.split_at(split);
    let number: u64 = number.parse().map_err(|_| invalid())?;
    let seconds = match unit.trim() {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(invalid()),
    };
    let total = number.checked_mul(seconds).ok_or_else(invalid)?;
    if total < 60 {
        return Err(ConfigError("INTERVAL must be at least one minute".into()));
    }
    Ok(Duration::from_secs(total))
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
        assert_eq!(config.safety, Safety::default());
        assert_eq!(config.concurrency, 4);
        assert!(config.schedule.is_none());
        assert_eq!(config.backups_dir, PathBuf::from("backups"));
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
            ("INCLUDE_FOLDERS", "Work, Family/Shared/ ,"),
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
    fn recovery_reads_only_the_requested_settings() {
        let mut values = with(&[]);
        values.remove("SOURCE_PASSWORD");
        values.insert(
            "SOURCE_PASSWORD_FILE".into(),
            "/unavailable-source-secret".into(),
        );
        values.insert(
            "NOTIFY_SUCCESS_URL_FILE".into(),
            "/unavailable-notification-secret".into(),
        );
        assert!(Config::from_env(&values).is_err());
        let target = vault_config(&values, "DEST").unwrap();
        assert_eq!(target.client_id, "user.dest");
        assert_eq!(
            backup_passphrase(&values).unwrap().as_deref(),
            Some("backup passphrase")
        );
        assert!(client_options(&values).is_ok());
        assert_eq!(concurrency(&values).unwrap(), 4);
        values.insert("CONCURRENCY".into(), "0".into());
        assert!(concurrency(&values).is_err());
    }

    #[test]
    fn tuning_values_are_validated() {
        let config = Config::from_env(&with(&[
            ("RETENTION_DAYS", "7"),
            ("MAX_DELETE_PERCENT", "20"),
            ("CONCURRENCY", "8"),
        ]))
        .unwrap();
        assert_eq!(config.backup.retention_days, 7);
        assert_eq!(config.safety.max_delete_percent, Some(20));
        assert_eq!(config.concurrency, 8);

        assert!(error(with(&[("MAX_DELETE_PERCENT", "101")])).contains("between 0 and 100"));
        assert!(error(with(&[("CONCURRENCY", "0")])).contains("between 1 and 32"));
        assert!(error(with(&[("RETENTION_DAYS", "soon")])).contains("RETENTION_DAYS"));
    }

    #[test]
    fn schedules_are_a_cron_expression_or_an_interval() {
        let cron =
            Config::from_env(&with(&[("SCHEDULE", "0 3 * * *"), ("TZ", "Europe/Berlin")])).unwrap();
        assert_eq!(cron.schedule, Some(Schedule::Cron("0 3 * * *".into())));
        assert_eq!(cron.timezone.as_deref(), Some("Europe/Berlin"));
        let every = Config::from_env(&with(&[("INTERVAL", "6h")])).unwrap();
        assert_eq!(
            every.schedule,
            Some(Schedule::Every(Duration::from_secs(6 * 3600)))
        );

        assert!(error(with(&[("SCHEDULE", "0 3 * * *"), ("INTERVAL", "6h")])).contains("not both"));
        assert!(error(with(&[("INTERVAL", "10s")])).contains("at least one minute"));
        for invalid in ["6", "h", "6 hours", "-1h"] {
            assert!(
                error(with(&[("INTERVAL", invalid)])).contains("INTERVAL must look like"),
                "{invalid}"
            );
        }
    }
}
