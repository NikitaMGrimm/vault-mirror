# Configuration

[← Documentation](README.md)

vault-mirror reads settings from environment variables.
You can supply any secret through a file.
Set `NAME_FILE` to its path instead of setting `NAME`.

vault-mirror reads the secret from that file.
Secret files support Docker and systemd credentials.
It uses passwords and passphrases exactly as given, including spaces.
It removes trailing newline characters from secret files.

## Settings file

`vault-mirror init` writes the settings file, `vault-mirror.env`.
Each line has the form `NAME=value`.
The tool reads values literally, without processing quotes or variables.

Passwords can contain any character that fits on one line.
Lines that start with `#` are comments.
An environment variable takes precedence over the same setting in the settings file.

| Program | Settings file option |
| --- | --- |
| Docker Compose | `format: raw` |
| `docker run` | `--env-file` |
| vault-mirror binary | `vault-mirror --env-file PATH` |

For manual setup:

1. Write the settings file with the required variables from the table below.
2. Create the `backups` directory.
3. Create `compose.yaml` with this content.
4. Replace `1000:1000` with the backup directory owner's user id and group id.
5. Run `docker compose run --rm vault-mirror check` to check logins and view the plan.

```yaml
services:
  vault-mirror:
    image: ghcr.io/nikitamgrimm/vault-mirror:0.2
    command: daemon
    # The image runs unprivileged. Run it as the owner of ./backups: create
    # the directory first, and put your `id -u`:`id -g` here.
    user: "1000:1000"
    env_file:
      - path: vault-mirror.env
        format: raw
    volumes:
      - ./backups:/data
    restart: unless-stopped
```

`check` does not change vault data or write backups.
`check` can create the backup directory and `state.json` for login device ids.

## Variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `SOURCE_SERVER` | required | Source server URL. `https://vault.bitwarden.com` and `https://vault.bitwarden.eu` select Bitwarden's cloud. |
| `SOURCE_CLIENT_ID`, `SOURCE_CLIENT_SECRET` | required | Source personal API key. |
| `SOURCE_PASSWORD` | required | Source master password. |
| `SOURCE_ORGANIZATION_ID` | — | Organization to copy instead of the personal source vault. See [Organizations](#organizations). |
| `DEST_SERVER`, `DEST_CLIENT_ID`, `DEST_CLIENT_SECRET`, `DEST_PASSWORD`, `DEST_ORGANIZATION_ID` | — | Destination settings use the same form as source settings. Without a destination, runs back up only the source. Organization copies require both organization ids; personal copies require neither. |
| `INCLUDE_FOLDERS`, `EXCLUDE_FOLDERS` | — | Comma-separated folder names. See [Mirroring part of a vault](#mirroring-part-of-a-vault). |
| `INCLUDE_COLLECTIONS`, `EXCLUDE_COLLECTIONS` | — | Comma-separated collection names for organization mode. |
| `BACKUP_PASSPHRASE` | — | Passphrase for the backup key. Enabled backups require this variable or `BACKUP_RECIPIENTS`. `ENCRYPTION_PASSWORD` is an accepted older name. |
| `BACKUP_RECIPIENTS` | — | age public keys (`age1…`), separated by commas or spaces. The tool encrypts backups directly to these keys. Restoration requires `--identity`. |
| `BACKUP_ENABLED` | `true` | `false` permits copying without backups. |
| `BACKUP_ATTACHMENTS` | `true` | The tool stores attachment content in backups. |
| `ENABLE_PRUNING` | `true` | The tool deletes old snapshots. |
| `RETENTION_DAYS` | `30` | The tool deletes snapshots older than this number of days, except those protected by `MIN_FILES`. |
| `MIN_FILES` | `10` | The tool keeps at least this many newest snapshots per vault. |
| `BACKUPS_DIR` | `backups` (`/data` in the image) | Directory for backups and `last-run.json`. |
| `MAX_DELETE_PERCENT` | — | Deletion percentage limit, disabled by default. The limit applies above 10 deletions when the destination has recognized copies. See [Safety checks](how-it-works.md#safety-checks). |
| `CONCURRENCY` | `4` | Number of parallel item writes, from 1 to 32. |
| `VERIFY_ATTACHMENTS` | `true` | The tool downloads copied attachments again and compares their SHA-256 hashes. |
| `SCHEDULE` | — | Cron expression for `daemon`, such as `0 3 * * *`. `CRON_SCHEDULE` is an accepted older name. |
| `INTERVAL` | — | Alternative to `SCHEDULE`, such as `30m`, `6h`, or `1d`. The first interval run starts immediately. |
| `TZ` | UTC | Time zone for `SCHEDULE`. |
| `NOTIFY_SUCCESS_URL`, `NOTIFY_FAILURE_URL` | — | Notification URL for a one-line summary by `POST` after each run. Supported services include [ntfy](https://ntfy.sh), [healthchecks.io](https://healthchecks.io), Gotify webhooks, and similar services that accept these requests. Summaries contain no vault data. |
| `CA_CERT_FILE` | — | PEM file with additional trusted certificate authorities for servers with a private CA. |
| `BITWARDEN_CLIENT_VERSION` | a recent CLI version | Client version that the tool reports to the server. Change this only if a server rejects the default. |

vault-mirror accepts and ignores these older variables:
`SOURCE_ACCOUNT`, `DEST_ACCOUNT`, `PUID`, `PGID`, `TEMP_DIR`, and `DEST_DELETE_WORKERS`.

## Organizations

Set `SOURCE_ORGANIZATION_ID` and `DEST_ORGANIZATION_ID` to copy one organization into another.
The web vault's address contains the organization id when you open the organization.

Bitwarden's servers show owners only their assigned collections.
These servers hide organization items that have no collection.
The tool stops the run if the account cannot see all organization data.
This prevents hidden items from appearing deleted.

For both organizations:

1. Give the account the **owner** or **admin** role.
2. Assign the account to every collection with **Can manage** permission.
3. Put every item in a collection.

Organization copies exclude members, groups, permissions, favorites, and folders.

## Mirroring part of a vault

```sh
# Only these folders and their subfolders...
INCLUDE_FOLDERS=Work, Family
# ...except these.
EXCLUDE_FOLDERS=Work/Private
```

| Filter rule | Effect |
| --- | --- |
| Folder name | The name includes subfolders. `Work` includes `Work/Infra`. |
| `INCLUDE_FOLDERS` | The filter excludes items without a folder. |
| Unmatched name | An unmatched name produces a warning. |
| Changed filter | The tool deletes copies that the filter no longer selects, subject to the [safety checks](how-it-works.md#safety-checks). |
| Backups | Backups include the whole configured vault, except trash items, regardless of filters. |

`INCLUDE_COLLECTIONS` and `EXCLUDE_COLLECTIONS` use the same rules for organizations.
The tool excludes an item in an excluded collection even when an included collection also contains that item.
