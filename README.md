# vault-mirror

Keeps a second Bitwarden or Vaultwarden vault in sync with your main one, and
keeps encrypted, restorable backups of both.

- **A mirror you can rely on.** Every item type, custom field, password
  history entry, passkey, folder, archive state, and attachment is copied. The
  tool re-reads the destination after every change and fails loudly if the two
  vaults differ.
- **Incremental.** An unchanged vault costs two syncs and changes nothing on
  either server. A changed
  item is updated in place, so its id, and links to it, stay stable.
- **Complete backups.** Each run writes an [age](https://age-encryption.org)-
  encrypted snapshot of each vault, including attachment content. A snapshot is
  only written when the vault changed. Snapshots are Bitwarden's own
  unencrypted export format, so any Bitwarden client can import one after
  decryption. Decryption needs only the stock `age` tool.
- **Hard to misuse.** The tool refuses to run when:
  - the source looks like a mirror;
  - a first run would delete items it did not create;
  - a run would delete more than half the destination.

  The destination is backed up before it is changed.
- **Self-contained.** One static binary or a 14 MB container image. It speaks
  the Bitwarden API directly and needs neither the `bw` CLI nor Node.js.

> [!IMPORTANT]
> vault-mirror is tested end to end against Vaultwarden and against a
> self-hosted copy of Bitwarden's own server, each as source and as
> destination. It has not yet run against Bitwarden's cloud. Try a
> `--dry-run` first.

## Contents

- [Quick start](#quick-start)
- [How a run works](#how-a-run-works)
- [Configuration](#configuration)
- [Backups](#backups)
- [Commands](#commands)
- [Scheduling](#scheduling)
- [Migrating from the Python vault-mirror](#migrating-from-the-python-vault-mirror)
- [Limitations](#limitations)
- [Development](#development)

## Quick start

Create an API key for each account: open the web vault and go to **Settings →
Security → Keys → View API key**. Then write an env file:

```sh
# vault-mirror.env
SOURCE_SERVER=https://vault.bitwarden.com
SOURCE_CLIENT_ID=user.xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
SOURCE_CLIENT_SECRET=...
SOURCE_PASSWORD=...          # master password, to decrypt the vault

DEST_SERVER=https://vaultwarden.example.com
DEST_CLIENT_ID=user....
DEST_CLIENT_SECRET=...
DEST_PASSWORD=...

BACKUP_PASSPHRASE=...        # protects the backup key; keep it somewhere else too
SCHEDULE=0 3 * * *
TZ=Europe/Berlin
```

Use a dedicated, empty destination account. The tool deletes everything in it
that does not come from the source.

```yaml
# compose.yaml
services:
  vault-mirror:
    image: ghcr.io/nikitamgrimm/vault-mirror:0.1
    command: daemon
    env_file: vault-mirror.env
    volumes:
      - ./backups:/data
    restart: unless-stopped
```

Check the setup first. `check` logs in to both vaults and prints the plan
without writing anything:

```sh
docker compose run --rm vault-mirror check
docker compose up -d
```

You can run the binary from the
[releases](https://github.com/NikitaMGrimm/vault-mirror-rs/releases) the same
way. It reads the same environment variables and keeps its backups in
`./backups` unless `BACKUPS_DIR` is set.

## How a run works

1. **Source backup.** Log in to the source and back it up with its attachments.
2. **Destination backup.** Log in to the destination and back it up. Its
   attachments are copies of the source's, so their content is not stored
   again.
3. **Plan.** Compare the two vaults and check the plan against the safety
   rules.
4. **Change the destination.**
   - Create folders and collections.
   - Create and update items.
   - Copy attachments that are new or changed.
   - Delete what is no longer in the source.
5. **Verify.** Read the destination again and compare it with the source,
   including a SHA-256 of every copied attachment.
6. **Report.** Write `last-run.json` and send the notification, if one is
   configured.

If any step fails, the run stops there. A failed backup means the destination
is not touched.

Each mirrored item carries a hidden custom field, `__source`. It holds:

- the id of the source item;
- a hash of the item's content;
- a hash of its attachments.

The field links each copy to its original, so a later run knows exactly what
changed. The field holds no secrets. The comparison uses the decrypted content
itself, not only the hash, so an edit made directly in the destination is
undone on the next run.

### Safety rules

| Situation | Behavior | Override |
| --- | --- | --- |
| Source items carry `__source` markers: source and destination look swapped | Refuses | none |
| Source and destination are the same vault | Refuses | none |
| The destination has items, none of which this tool created (a first run) | Refuses | `--allow-initial-overwrite` |
| The run would delete more than `MAX_DELETE_PERCENT` of the destination, and more than 10 items | Refuses | `--allow-mass-delete` |

Deletions in the destination are permanent. They do not go to the trash. The
destination snapshot taken earlier in the same run keeps a copy.

## Configuration

All configuration comes from environment variables. You can pass any secret
as a file instead: set `NAME_FILE` to a path and the file's content is used,
which suits Docker and systemd credentials.

| Variable | Default | Meaning |
| --- | --- | --- |
| `SOURCE_SERVER` | required | Server URL. `https://vault.bitwarden.com` and `https://vault.bitwarden.eu` select Bitwarden's cloud. |
| `SOURCE_CLIENT_ID`, `SOURCE_CLIENT_SECRET` | required | Personal API key. |
| `SOURCE_PASSWORD` | required | Master password. |
| `SOURCE_ORGANIZATION_ID` | — | Mirror this organization's vault instead of the personal vault. |
| `DEST_SERVER`, `DEST_CLIENT_ID`, `DEST_CLIENT_SECRET`, `DEST_PASSWORD`, `DEST_ORGANIZATION_ID` | — | The destination, in the same form as the source. Without a destination, runs only back up the source. Organizations are mirrored to organizations: set both organization ids or neither. |
| `BACKUP_PASSPHRASE` | — | Passphrase that protects the backup key. Either this or `BACKUP_RECIPIENTS` is required while backups are on. `ENCRYPTION_PASSWORD` is accepted as an older name. |
| `BACKUP_RECIPIENTS` | — | age public keys (`age1…`), separated by commas or spaces, to encrypt backups to instead. Restoring then needs `--identity`. |
| `BACKUP_ENABLED` | `true` | Set `false` to mirror without backups. |
| `BACKUP_ATTACHMENTS` | `true` | Store attachment content in backups. |
| `ENABLE_PRUNING` | `true` | Delete old snapshots. |
| `RETENTION_DAYS` | `30` | Age after which snapshots are deleted… |
| `MIN_FILES` | `10` | …except the newest this many, per vault. |
| `BACKUPS_DIR` | `backups` (`/data` in the image) | Where backups and `last-run.json` are kept. |
| `MAX_DELETE_PERCENT` | `50` | Mass-deletion limit; see [Safety rules](#safety-rules). |
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

## Backups

```text
backups/
├── key.age                         # backup key, encrypted with BACKUP_PASSPHRASE
├── last-run.json                   # result of the last run; no vault data
├── source/
│   ├── snapshots/
│   │   ├── vault-20261014T030000Z.json.age
│   │   └── vault-20261014T030000Z.meta.json   # item count and attachment ids; no vault data
│   └── attachments/
│       └── <attachment id>.age
└── dest/
    └── …
```

- **Key.** On first use, the tool generates an age key. It is stored only in
  `key.age`, encrypted with your passphrase. Changing `BACKUP_PASSPHRASE`
  later does not re-encrypt `key.age`. Keep the old passphrase, or start a
  new backup directory.
- **Snapshots.** Each snapshot is a complete vault export with all secrets in
  the clear, encrypted as a whole. A snapshot is only written when the vault
  changed since the last one.
- **Attachments.** Each attachment is stored once, however many snapshots
  refer to it. Pruning deletes attachments that no remaining snapshot needs.

### Recovering without vault-mirror

Only the standard [`age`](https://github.com/FiloSottile/age) tool and your
passphrase are needed:

```sh
age -d -i backups/key.age backups/source/snapshots/vault-20261014T030000Z.json.age > vault.json
age -d -i backups/key.age backups/source/attachments/<attachment id>.age > attachment.bin
```

`age` asks for the passphrase to unlock `key.age`. `vault.json` imports into
any Bitwarden client: **Import data → Bitwarden (json)**. The
`vaultMirror.attachments` list inside it maps each attachment id to its item
and file name.

### Restoring with vault-mirror

```sh
vault-mirror snapshots                       # list the source vault's snapshots
vault-mirror export --output ./recovered     # newest snapshot as export.json plus attachment files, in the clear
vault-mirror restore --to dest               # write the newest source snapshot into the destination vault
```

`restore` writes into one of the configured vaults:

- It recreates folders, collections, items, archive state, and attachments.
- By default it refuses a vault that already has items. Pass
  `--allow-non-empty` to add the items anyway.
- `--from dest` reads the destination's backups instead of the source's.
- `--snapshot FILE` picks a snapshot other than the newest.

## Commands

| Command | Does |
| --- | --- |
| `vault-mirror` or `vault-mirror run` | One run. Flags: `--dry-run`, `--allow-initial-overwrite`, `--allow-mass-delete`. |
| `vault-mirror daemon` | Runs on `SCHEDULE` or every `INTERVAL`, and stops cleanly on `SIGTERM` between runs. |
| `vault-mirror check` | Logs in, counts items, and prints the plan. Writes nothing. |
| `vault-mirror snapshots [--vault dest]` | Lists snapshots. |
| `vault-mirror export --output DIR` | Decrypts a snapshot and its attachments into `DIR`. |
| `vault-mirror restore --to source\|dest` | Writes a snapshot into a vault. |
| `vault-mirror healthcheck [--max-age-hours 26]` | Exits non-zero unless the last run succeeded within the given time. The image uses it as its health check. |

A dry run makes backups and prints the plan, but does not change the
destination.

## Scheduling

Either let the container run `daemon`, as in the quick start, or have your
scheduler start a single run. A systemd example:

```ini
# /etc/systemd/system/vault-mirror.service
[Service]
Type=oneshot
ExecStart=/usr/bin/docker run --rm --env-file /etc/vault-mirror.env \
  -v /var/lib/vault-mirror:/data ghcr.io/nikitamgrimm/vault-mirror:0.1

# /etc/systemd/system/vault-mirror.timer
[Timer]
OnCalendar=*-*-* 03:00:00
RandomizedDelaySec=15m
Persistent=true

[Install]
WantedBy=timers.target
```

The container runs as user 65532. A bind-mounted backup directory must be
writable by that user, for example after `chown 65532:65532
/var/lib/vault-mirror`.

## Migrating from the Python vault-mirror

The environment variables are the same, so you can replace the image and keep
your env file.

- **The first run adopts the existing mirror in place.** It recognises the
  older `__source` markers and keeps every destination item and its id. The
  run rewrites each marker in the new form and copies attachments once more,
  because the old tool did not record which copies were current. Later runs
  change nothing until the source changes.
- **Old backups stay where they are.** The existing `*.json.enc` files are
  neither read nor pruned. Decrypt them with `openssl` as before, and delete
  them once you no longer need them.
- **Point `BACKUPS_DIR` at the same directory if you like.** New backups go to
  `source/snapshots` and `dest/snapshots` inside it, next to the old files.
- **`PUID` and `PGID` no longer apply.** The image runs as user 65532. Either
  give that user the backup directory, or start the container as the
  directory's owner, for example with `--user 1000:1000`.
- **Going back is costly.** If you switch back to the Python tool afterwards,
  it does not recognise the new markers and recreates every destination item.

## Limitations

- Sends, organization members, groups, collection permissions, and account
  settings are not mirrored.
- Items in the source's trash are not mirrored. Trashed items in the
  destination are deleted.
- Archived items need a server that supports archiving. If the destination
  does not, items are mirrored unarchived, and the run reports a warning.
- Tested against Vaultwarden 1.37 and Bitwarden's self-hosted server 2026.9.
  Bitwarden's cloud runs the same server code but has not been tested yet. The
  cloud stores attachments in Azure, and that upload path is tested against
  Microsoft's Azurite emulator.
- Organization mode is tested against Vaultwarden only. A self-hosted
  Bitwarden server creates organizations only from a license file, which the
  test setup does not have.
- Attachments are held in memory while they are copied, so a large attachment
  needs a few times its size in free memory.

## Development

The sandbox runs two throwaway Vaultwarden servers and a TLS proxy in Docker,
so you can develop without a Bitwarden account:

```sh
sandbox/up.sh                   # http://127.0.0.1:18081 and :18082, https on :18443 and :18444
cargo test                      # unit and end-to-end tests
cargo run -- sandbox --seed     # prints an env file for two new accounts with sample data
docker compose -f sandbox/compose.yaml --profile official down
```

When the sandbox is not running, the end-to-end tests are skipped. In CI they
are required: `VAULT_MIRROR_REQUIRE_SANDBOX=1`. One test checks that a
snapshot imports into the official Bitwarden CLI. Set `VAULT_MIRROR_BW` to a
`bw` binary to run it.

`sandbox/up.sh --official` also starts Bitwarden's own server, which needs a
2.7 GB image. The server runs entirely locally: its installation id is made
up, and it has no installation key, so it never contacts Bitwarden. Like
Bitwarden's cloud, it stores attachments in Azure blob storage, here provided
by the Azurite emulator. The sandbox makes every account on it premium,
because attachments need premium.
Point the tests at it by URL:

```sh
VAULT_MIRROR_SANDBOX_SOURCE=http://127.0.0.1:18085 cargo test
VAULT_MIRROR_SANDBOX_DEST=http://127.0.0.1:18085 VAULT_MIRROR_SANDBOX_DEST_TLS=https://127.0.0.1:18445 cargo test
```

`cargo test --release --test scale -- --ignored --nocapture` measures a vault
of 2000 items. Set `VAULT_MIRROR_SCALE_ITEMS` to change the size.

## License

[GPL-3.0-only](LICENSE). vault-mirror uses Bitwarden's `bitwarden-crypto`
crate, which is published under the GPL.
