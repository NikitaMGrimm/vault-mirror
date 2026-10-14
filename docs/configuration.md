# Configuration

[← Documentation](README.md)

All configuration comes from environment variables. You can pass any secret
as a file instead: set `NAME_FILE` to a path and the file's content is used,
which suits Docker and systemd credentials. Passwords and passphrases are
used exactly as given, spaces included; a file's final newline is dropped.

| Variable | Default | Meaning |
| --- | --- | --- |
| `SOURCE_SERVER` | required | Server URL. `https://vault.bitwarden.com` and `https://vault.bitwarden.eu` select Bitwarden's cloud. |
| `SOURCE_CLIENT_ID`, `SOURCE_CLIENT_SECRET` | required | Personal API key. |
| `SOURCE_PASSWORD` | required | Master password. |
| `SOURCE_ORGANIZATION_ID` | — | Mirror this organization instead of the personal vault; see [Organizations](#organizations). |
| `DEST_SERVER`, `DEST_CLIENT_ID`, `DEST_CLIENT_SECRET`, `DEST_PASSWORD`, `DEST_ORGANIZATION_ID` | — | The destination, in the same form as the source. Without a destination, runs only back up the source. Organizations are mirrored to organizations: set both organization ids or neither. |
| `INCLUDE_FOLDERS`, `EXCLUDE_FOLDERS` | — | Comma-separated folder names; see [Mirroring part of a vault](#mirroring-part-of-a-vault). |
| `INCLUDE_COLLECTIONS`, `EXCLUDE_COLLECTIONS` | — | The same for collections, in organization mode. |
| `BACKUP_PASSPHRASE` | — | Passphrase that protects the backup key. Either this or `BACKUP_RECIPIENTS` is required while backups are on. `ENCRYPTION_PASSWORD` is accepted as an older name. |
| `BACKUP_RECIPIENTS` | — | age public keys (`age1…`), separated by commas or spaces, to encrypt backups to instead. Restoring then needs `--identity`. |
| `BACKUP_ENABLED` | `true` | Set `false` to mirror without backups. |
| `BACKUP_ATTACHMENTS` | `true` | Store attachment content in backups. |
| `ENABLE_PRUNING` | `true` | Delete old snapshots. |
| `RETENTION_DAYS` | `30` | Age after which snapshots are deleted… |
| `MIN_FILES` | `10` | …except the newest this many, per vault. |
| `BACKUPS_DIR` | `backups` (`/data` in the image) | Where backups and `last-run.json` are kept. |
| `MAX_DELETE_PERCENT` | — | Stop a run that would delete more than this percentage of the destination's items, if it deletes more than 10. Off unless set; see [Safety checks](how-it-works.md#safety-checks). |
| `CONCURRENCY` | `4` | Parallel item writes, 1–32. |
| `VERIFY_ATTACHMENTS` | `true` | Download copied attachments again and compare their SHA-256. |
| `SCHEDULE` | — | Cron expression for `daemon`, such as `0 3 * * *`. `CRON_SCHEDULE` is accepted as an older name. |
| `INTERVAL` | — | Alternative to `SCHEDULE`, such as `30m`, `6h`, or `1d`. With an interval, the first run starts immediately. |
| `TZ` | UTC | Time zone for `SCHEDULE`. |
| `NOTIFY_SUCCESS_URL`, `NOTIFY_FAILURE_URL` | — | URL that receives a one-line summary by `POST` after each run. Works with [ntfy](https://ntfy.sh), [healthchecks.io](https://healthchecks.io), Gotify webhooks, and similar services. The summary never contains vault data. |
| `CA_CERT_FILE` | — | PEM file with additional trusted certificate authorities, for servers with a private CA. |
| `BITWARDEN_CLIENT_VERSION` | a recent CLI version | Client version reported to the server. Change it only if a server rejects the default. |

`SOURCE_ACCOUNT`, `DEST_ACCOUNT`, `PUID`, `PGID`, `TEMP_DIR`, and
`DEST_DELETE_WORKERS` belong to earlier tools. They are accepted and ignored.

## Organizations

Set `SOURCE_ORGANIZATION_ID` and `DEST_ORGANIZATION_ID` to mirror one
organization into another. The organization id is in the web vault's address
when the organization is open. Bitwarden's servers show an owner only the
collections they are assigned to, and no item outside a collection. So that
nothing looks deleted, the run stops unless the account can see everything:

- Make the account an **owner** or **admin** of both organizations.
- Assign it to every collection, with **Can manage** permission.
- Put every item in a collection.

Members, groups, permissions, favorites, and folders are not mirrored.

## Mirroring part of a vault

```sh
# Only these folders and their subfolders...
INCLUDE_FOLDERS=Work, Family
# ...except these.
EXCLUDE_FOLDERS=Work/Private
```

- A name includes its subfolders, so `Work` covers `Work/Infra`.
- With `INCLUDE_FOLDERS`, items in no folder are not mirrored.
- A name that matches no folder produces a warning.
- When the filter changes, the destination follows: copies the filter no
  longer selects are deleted, within the [safety checks](how-it-works.md#safety-checks).
- Backups always hold the whole vault.

`INCLUDE_COLLECTIONS` and `EXCLUDE_COLLECTIONS` work the same way for
organizations. An item in an excluded collection is left out even if it is
also in an included one.
