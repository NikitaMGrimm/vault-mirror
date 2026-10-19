# Configuration

[← Documentation](README.md)

vault-mirror is configured with environment variables. `vault-mirror init`
writes them into a **settings file**, `vault-mirror.env`. To write it yourself,
see [Setting up by hand](#setting-up-by-hand).

## The settings file

Each line is `NAME=value`. Lines starting with `#` are comments. When the
binary reads the file with `--env-file`, a name outside the
[variables](#variables) below stops the command. The generated Compose file
uses this check too. Unknown names passed only through the environment, such
as with `docker run --env-file`, are ignored.

Values are taken literally, without quote or `$VARIABLES` expansion. Quotes,
spaces, and dollar signs stay part of the value:

```dotenv
SOURCE_PASSWORD=it's "quoted" and $not a variable
```

How each tool reads it:

| Program | How to use the settings file |
| --- | --- |
| Docker Compose | Raw `env_file`, a read-only file mount, and `--env-file` in the entrypoint (as `init` writes them) |
| `docker run` | `--env-file vault-mirror.env` |
| vault-mirror binary | `vault-mirror --env-file vault-mirror.env <command>` |

A variable set in the environment wins over the same name in the settings file.
`--env-file` reports unknown setting names as errors.

### Secrets in files

To keep a secret out of the settings file, put it in a file of its own and set
`NAME_FILE` to that file's path, for example
`SOURCE_PASSWORD_FILE=/run/secrets/source_password`. This works with Docker
secrets and systemd credentials. Trailing newlines in the file are ignored.

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
| `SKIP_ATTACHMENTS` | `false` | Set to `true` to mirror items without copying or removing their attachments. Backups still include all attachments. |

With `SKIP_ATTACHMENTS=true`, `run`, `check` and `daemon` leave existing
destination attachments in place. New copies have none. Each run warns that
attachment copying is disabled. Set it to `false` or remove it to copy
attachments again. Destination attachment edits still need `--allow-overwrite`
before a run replaces those files or deletes their item.

This setting does not prevent obsolete copies from going to the trash with
their attachments. The destination backup keeps them.

### Backups

| Variable | Default | Meaning |
| --- | --- | --- |
| `BACKUP_PASSPHRASE` | required | The passphrase that protects the backup key. |
| `RETENTION_DAYS` | `30` | How many days of daily snapshots to keep. See [Backups](backups.md#whats-in-the-backup-directory). |
| `BACKUPS_DIR` | `backups` (`/data` in the image) | Where backups, `last-run.json` and login state live. |

### Running

| Variable | Default | Meaning |
| --- | --- | --- |
| `SCHEDULE` | | When `daemon` runs, as a cron expression such as `0 3 * * *`. |
| `TZ` | UTC | The time zone `SCHEDULE` is read in, such as `Europe/Berlin`. |
| `NOTIFY_SUCCESS_URL`, `NOTIFY_FAILURE_URL` | | After a successful or a failed run, `POST` a one-line summary to the matching URL, such as an [ntfy](https://ntfy.sh) topic or a [healthchecks.io](https://healthchecks.io) check. It holds counts and errors, never vault content. |
| `CA_CERT_FILE` | | A PEM file of extra certificate authorities to trust. In Docker, mount it into the container and give its path there. |

## Setting up by hand

Instead of `init`:

1. Write `vault-mirror.env` with the required variables above and
   `SCHEDULE`.

2. In the same directory, create `backups` and `compose.yaml`:

   ```sh
   mkdir -p backups
   cat > compose.yaml <<EOF
   services:
     vault-mirror:
       image: ghcr.io/nikitamgrimm/vault-mirror:0.8
       entrypoint: ["/vault-mirror", "--env-file", "/run/vault-mirror.env"]
       command: daemon
       user: "$(id -u):$(id -g)"
       env_file:
         - path: vault-mirror.env
           format: raw
       volumes:
         - ./backups:/data
         - ./vault-mirror.env:/run/vault-mirror.env:ro
       restart: unless-stopped
       # Gives a run in progress up to 5 minutes to finish when the container stops.
       stop_grace_period: 5m
   EOF
   ```

3. Check the logins and see what the first run would do:

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

The account must see everything in each organization, or vault-mirror stops.
On Bitwarden's servers, that means, for both organizations:

1. Give the account the **Owner** or **Admin** role.
2. Assign the account to every collection, with **Can manage** permission.
3. Make sure every item is in a collection.

Members, groups, permissions, favorites and folders aren't copied between
organizations. A destination member's favorites and folders stay as they set
them.

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
- A name that matches no folder in the source stops the run before it changes
  anything.
- Leave out a filter you don't use. An empty value, or an empty name such as a
  trailing comma, is an error.
- Copies the filter no longer selects are deleted on the next run.
- Backups always cover the whole vault.

`INCLUDE_COLLECTIONS` and `EXCLUDE_COLLECTIONS` work the same way for an
organization. An item in an excluded collection is left out even if it's also in
an included one.
