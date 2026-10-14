# Configuration

[← Documentation](README.md)

vault-mirror is configured entirely with environment variables. `vault-mirror
init` writes them into a **settings file**, `vault-mirror.env`, which Docker
Compose and the binary both read. If you'd rather write the file yourself, see
[Setting up by hand](#setting-up-by-hand).

## The settings file

Each line is `NAME=value`. Lines starting with `#` are comments, and blank lines
are ignored. Values are taken literally, with no quotes and no `$VARIABLES`, so
a password can contain any character that fits on one line:

```sh
SOURCE_PASSWORD=it's "quoted" and $not a variable
```

Every tool reads it the same way:

| Program | How to use the settings file |
| --- | --- |
| Docker Compose | `env_file` with `format: raw` (as `init` writes it) |
| `docker run` | `--env-file vault-mirror.env` |
| vault-mirror binary | `vault-mirror --env-file vault-mirror.env <command>` |

A variable set in the environment takes precedence over the same name in the
settings file.

### Secrets in files

Instead of putting a secret in the settings file, you can put it in a file of
its own and set `NAME_FILE` to that file's path. For example,
`SOURCE_PASSWORD_FILE=/run/secrets/source_password`. This works with Docker
secrets and systemd credentials. A trailing newline in the file is ignored.

Passwords and passphrases are used exactly as given, including leading and
trailing spaces. API keys and URLs are trimmed.

## Variables

### Accounts

| Variable | Default | Meaning |
| --- | --- | --- |
| `SOURCE_SERVER` | required | The source server. Use `https://vault.bitwarden.com` or `https://vault.bitwarden.eu` for Bitwarden's cloud, or your own server's address. |
| `SOURCE_CLIENT_ID`, `SOURCE_CLIENT_SECRET` | required | The source account's personal API key. |
| `SOURCE_PASSWORD` | required | The source account's master password. |
| `SOURCE_ORGANIZATION_ID` | | Mirror this organization instead of the personal vault. See [Organizations](#organizations). |
| `DEST_SERVER`, `DEST_CLIENT_ID`, `DEST_CLIENT_SECRET`, `DEST_PASSWORD`, `DEST_ORGANIZATION_ID` | | The destination, in the same form. Leave them all out to only back up the source. |

### What to mirror

| Variable | Default | Meaning |
| --- | --- | --- |
| `INCLUDE_FOLDERS`, `EXCLUDE_FOLDERS` | | Comma-separated folder names. See [Mirroring part of a vault](#mirroring-part-of-a-vault). |
| `INCLUDE_COLLECTIONS`, `EXCLUDE_COLLECTIONS` | | The same, for collections in an organization. |
| `MAX_DELETE_PERCENT` | off | Stop a run that would delete more than this share of the destination. See [Safety checks](how-it-works.md#safety-checks). |

### Backups

| Variable | Default | Meaning |
| --- | --- | --- |
| `BACKUP_PASSPHRASE` | | The passphrase that protects the backup key. Backups need this or `BACKUP_RECIPIENTS`. |
| `BACKUP_RECIPIENTS` | | age public keys (`age1…`), separated by commas or spaces. Backups are encrypted to these keys, so the machine running vault-mirror can't read them back. Restoring then needs `--identity`. |
| `BACKUP_ENABLED` | `true` | Set to `false` to mirror without backups. |
| `BACKUP_ATTACHMENTS` | `true` | Store attachment content in the backups, not only their names. |
| `ENABLE_PRUNING` | `true` | Delete old snapshots. |
| `RETENTION_DAYS` | `30` | Delete snapshots older than this… |
| `MIN_FILES` | `10` | …but always keep at least this many of the newest snapshots of each vault. |
| `BACKUPS_DIR` | `backups` (`/data` in the image) | Where backups, `last-run.json` and login state live. |

### Running

| Variable | Default | Meaning |
| --- | --- | --- |
| `SCHEDULE` | | When `daemon` runs, as a cron expression such as `0 3 * * *`. |
| `INTERVAL` | | Or how often `daemon` runs, such as `30m`, `6h` or `1d`. The first run starts right away. |
| `TZ` | UTC | The time zone `SCHEDULE` is read in, such as `Europe/Berlin`. |
| `NOTIFY_SUCCESS_URL`, `NOTIFY_FAILURE_URL` | | After each run, `POST` a one-line summary here. Works with [ntfy](https://ntfy.sh), [healthchecks.io](https://healthchecks.io), Gotify and anything else that accepts a plain-text `POST`. The summary has counts and error messages, never vault content. |
| `CONCURRENCY` | `4` | How many items are written at once, from 1 to 32. |
| `VERIFY_ATTACHMENTS` | `true` | Download every copied attachment again and compare its SHA-256 hash. |
| `CA_CERT_FILE` | | A PEM file of extra certificate authorities to trust, for a server with a private CA. |
| `BITWARDEN_CLIENT_VERSION` | a recent version | The client version reported to servers. Only change this if a server rejects the default. |

## Setting up by hand

If you can't run `init`, for example because there's no terminal:

1. Write `vault-mirror.env` with at least the required variables above,
   `BACKUP_PASSPHRASE`, and either `SCHEDULE` or `INTERVAL` for the daemon.
2. Create the `backups` directory next to it.
3. Save this as `compose.yaml`, replacing `1000:1000` with the user and group id
   that own `backups` (`id -u` and `id -g`):

   ```yaml
   services:
     vault-mirror:
       image: ghcr.io/nikitamgrimm/vault-mirror:0.2
       command: daemon
       # The image runs unprivileged, as the owner of ./backups.
       user: "1000:1000"
       env_file:
         - path: vault-mirror.env
           format: raw
       volumes:
         - ./backups:/data
       restart: unless-stopped
       # Lets a run in progress finish when the container is stopped.
       stop_grace_period: 5m
   ```

4. Check the logins and see what the first run would do:

   ```console
   $ docker compose run --rm vault-mirror check
   ```

   `check` never changes either vault. It does create `backups` and
   `backups/state.json`, which remembers the device id vault-mirror logs in
   with.

5. Start it with `docker compose up -d`.

## Organizations

To mirror one organization into another, set both `SOURCE_ORGANIZATION_ID` and
`DEST_ORGANIZATION_ID`. You'll find an organization's id in the web vault's
address bar while you have the organization open.

Bitwarden's servers only show an owner the collections they're assigned to, and
they hide items that aren't in any collection. Hidden items would look deleted
to vault-mirror, so it stops instead if the account can't see everything. For
each of the two organizations:

1. Give the account the **Owner** or **Admin** role.
2. Assign the account to every collection, with **Can manage** permission.
3. Make sure every item is in a collection.

Members, groups, permissions, favorites and folders aren't copied between
organizations.

## Mirroring part of a vault

To mirror only some folders, list them:

```sh
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
