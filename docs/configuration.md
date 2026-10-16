# Configuration

[← Documentation](README.md)

vault-mirror is configured with environment variables. `vault-mirror init`
writes them into a **settings file**, `vault-mirror.env`. To write it yourself,
see [Setting up by hand](#setting-up-by-hand).

## The settings file

Each line is `NAME=value`. Lines starting with `#` are comments.

Values are taken literally, without quote or `$VARIABLES` expansion. Quotes,
spaces, and dollar signs stay part of the password:

```dotenv
SOURCE_PASSWORD=it's "quoted" and $not a variable
```

Each setting occupies one line. To supply a password containing a newline,
set the environment variable directly. Secret files preserve internal newlines
but trim trailing newline characters.

How each tool reads it:

| Program | How to use the settings file |
| --- | --- |
| Docker Compose | `env_file` with `format: raw` (as `init` writes it) |
| `docker run` | `--env-file vault-mirror.env` |
| vault-mirror binary | `vault-mirror --env-file vault-mirror.env <command>` |

A variable set in the environment wins over the same name in the settings file.

### Secrets in files

To keep a secret out of the settings file, put it in a file of its own and set
`NAME_FILE` to that file's path, for example
`SOURCE_PASSWORD_FILE=/run/secrets/source_password`. This works with Docker
secrets and systemd credentials.

Passwords and passphrases are used exactly as given, including spaces at the
ends. API keys and URLs are trimmed, and so is a trailing newline in a secret
file.

## Variables

### Accounts

| Variable | Default | Meaning |
| --- | --- | --- |
| `SOURCE_SERVER` | required | The source server. Use `https://vault.bitwarden.com` or `https://vault.bitwarden.eu` for Bitwarden's cloud, or your own server's address. vault-mirror warns about `http://` for a host outside your network, because the API key would cross the internet unencrypted. |
| `SOURCE_CLIENT_ID`, `SOURCE_CLIENT_SECRET` | required | The source account's personal API key. |
| `SOURCE_PASSWORD` | required | The source account's master password. |
| `SOURCE_ORGANIZATION_ID` | | Mirror this organization instead of the personal vault. See [Organizations](#organizations). |
| `DEST_SERVER`, `DEST_CLIENT_ID`, `DEST_CLIENT_SECRET`, `DEST_PASSWORD`, `DEST_ORGANIZATION_ID` | | The destination, in the same form. Leave them all out to only back up the source. |

### What to mirror

| Variable | Default | Meaning |
| --- | --- | --- |
| `INCLUDE_FOLDERS`, `EXCLUDE_FOLDERS` | | Comma-separated folder names. See [Mirroring part of a vault](#mirroring-part-of-a-vault). |
| `INCLUDE_COLLECTIONS`, `EXCLUDE_COLLECTIONS` | | The same, for collections in an organization. |
| `MAX_DELETE_PERCENT` | off | Stop a run that would delete more than this percentage of the copies. See [Safety checks](how-it-works.md#safety-checks). |

### Backups

| Variable | Default | Meaning |
| --- | --- | --- |
| `BACKUP_PASSPHRASE` | | The passphrase that protects the backup key. Backups need this or `BACKUP_RECIPIENTS`. |
| `BACKUP_RECIPIENTS` | | age public keys (`age1…`), separated by commas or spaces. Backups are encrypted to these keys, so the machine running vault-mirror can't read them back. Restoring then needs `--identity`. |
| `BACKUP_ENABLED` | `true` | Set to `false` to mirror without backups. |
| `BACKUP_ATTACHMENTS` | `true` | Store attachment content in the backups, not only their names. |
| `ENABLE_PRUNING` | `true` | Delete old snapshots. |
| `RETENTION_DAYS` | `30` | Delete current-group snapshots older than this. Past the last 24 hours, keep the newest of each UTC day. Legacy and inactive encryption groups are preserved. |
| `MIN_FILES` | `10` | Always keep at least this many newest snapshots of the active vault and encryption group. Identical recipient-only points count separately. |
| `BACKUPS_DIR` | `backups` (`/data` in the image) | Where backups, `last-run.json` and login state live. |

### Running

| Variable | Default | Meaning |
| --- | --- | --- |
| `SCHEDULE` | | When `daemon` runs, as a cron expression such as `0 3 * * *`. |
| `INTERVAL` | | Or how often `daemon` runs, such as `30m`, `6h` or `1d`. The first run starts right away. |
| `TZ` | UTC | The time zone `SCHEDULE` is read in, such as `Europe/Berlin`. |
| `NOTIFY_SUCCESS_URL`, `NOTIFY_FAILURE_URL` | | After each actual run, `POST` a one-line summary here. Dry runs send no notification. Works with [ntfy](https://ntfy.sh), [healthchecks.io](https://healthchecks.io), Gotify and anything else that accepts a plain-text `POST`. The summary has counts and error messages, never vault content. A delivery failure is a warning. |
| `CONCURRENCY` | `4` | How many items are written at once, from 1 to 32. |
| `VERIFY_ATTACHMENTS` | `true` | Download newly copied attachments again and compare their SHA-256 hashes. Replacements are always checked before old files are removed. Pull and restore always verify uploaded bytes. |
| `CA_CERT_FILE` | | A PEM file of extra certificate authorities to trust, for a server with a private CA. |
| `BITWARDEN_CLIENT_VERSION` | a recent version | The client version reported to servers. Only change this if a server rejects the default. |

When `CA_CERT_FILE` is set during `init`, the settings file keeps its absolute
path for native runs. Generated Compose settings mount it read-only at
`/ca.pem` and override the path inside the container. If you run `init` in
Docker, keep the certificate inside the mounted working directory. For example,
pass `-e CA_CERT_FILE=/work/ca.pem` when that directory is mounted at `/work`.

## Setting up by hand

If you can't run `init`, for example because there's no terminal:

1. Write `vault-mirror.env` with the required variables above,
   `BACKUP_PASSPHRASE`, and `SCHEDULE` or `INTERVAL`.

2. In the same directory, create `backups` and `compose.yaml`. The container
   runs as you, so it can write to `backups`.

   ```sh
   mkdir -p backups
   cat > compose.yaml <<EOF
   services:
     vault-mirror:
       image: ghcr.io/nikitamgrimm/vault-mirror
       command: daemon
       user: "$(id -u):$(id -g)"
       env_file:
         - path: vault-mirror.env
           format: raw
       volumes:
         - ./backups:/data
       restart: unless-stopped
       # Gives a run in progress up to 5 minutes to finish when the container stops.
       stop_grace_period: 5m
   EOF
   ```

3. Check the logins and see what the first run would do. This doesn't change
   either vault.

   ```sh
   docker compose run --rm vault-mirror check
   ```

4. Start it:

   ```sh
   docker compose up -d
   ```

## Organizations

To mirror one organization into another, set both `SOURCE_ORGANIZATION_ID` and
`DEST_ORGANIZATION_ID`. An organization's id is in the web vault's address bar
while the organization is open.

The account must see everything in each organization. Otherwise hidden items
would look deleted, so vault-mirror stops. On Bitwarden's servers, that means,
for both organizations:

1. Give the account the **Owner** or **Admin** role.
2. Assign the account to every collection, with **Can manage** permission.
3. Make sure every item is in a collection.

Members, groups, permissions, favorites and folders aren't copied between
organizations.

## Mirroring part of a vault

To mirror only some folders, list them in the settings file:

```dotenv
# Only these folders and the folders inside them…
INCLUDE_FOLDERS=Work, Family
# …except this one.
EXCLUDE_FOLDERS=Work/Private
```

- A name includes the folders nested in it: `Work` also selects `Work/Infra`.
- With `INCLUDE_FOLDERS`, items that aren't in any folder are left out.
- A name that matches no folder gives a warning, so a typo doesn't quietly
  select nothing.
- If you change the filter, copies that it no longer selects are deleted on the
  next run, subject to the [safety checks](how-it-works.md#safety-checks).
- Backups always cover the whole vault, whatever the filter.

`INCLUDE_COLLECTIONS` and `EXCLUDE_COLLECTIONS` work the same way for an
organization. An item in an excluded collection is left out even if it's also in
an included one.
