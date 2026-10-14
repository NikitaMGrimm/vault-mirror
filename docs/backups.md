# Backups and restoring

[← Documentation](README.md)

Backups are on by default. Every run backs up the source and the destination
before it changes anything.

The backups are ordinary files. You can read them with the standard
[`age`](https://age-encryption.org) tool and import them into any Bitwarden
app, without vault-mirror.

## What's in the backup directory

```text
backups/
├── key.age                         # the backup key, encrypted with BACKUP_PASSPHRASE
├── last-run.json                   # the result of the last run; no vault content
├── state.json                      # the device ids vault-mirror logs in with
├── run.lock                        # held while a run or restore uses the directory
├── source/
│   ├── snapshots/
│   │   ├── vault-20261014T030000Z.json.age
│   │   └── vault-20261014T030000Z.meta.json   # item count and attachment ids; no vault content
│   └── attachments/
│       └── 6f1c2a9e-….age
└── dest/
    └── …
```

A **snapshot** is a full export of one vault, without the trash, in Bitwarden's
JSON export format. A run only saves a new snapshot when the vault has changed.

Each attachment is stored once per vault, no matter how many snapshots use it.
Pruning old snapshots also removes attachments that no snapshot needs anymore.

### The backup key

With `BACKUP_PASSPHRASE`, vault-mirror creates a key on its first run and stores
it in `key.age`, encrypted with your passphrase.

With `BACKUP_RECIPIENTS`, it encrypts to your own age public keys instead, and
there's no `key.age`.

> [!WARNING]
> Changing `BACKUP_PASSPHRASE` doesn't re-encrypt `key.age`, and vault-mirror
> won't start with a passphrase that can't open it. Keep the old passphrase, or
> use a new backup directory.

## Restoring without vault-mirror

You need `age` and your passphrase. Run these commands in the directory that
holds `backups`.

> [!WARNING]
> These commands write your secrets to disk unencrypted. Delete the files when
> you're done.

1. Decrypt the backup key. `age` asks for your passphrase.

   ```sh
   age -d -o key.txt backups/key.age
   ```

   With `BACKUP_RECIPIENTS`, skip this step and use your own identity file in
   place of `key.txt` below.

2. Decrypt the newest snapshot of the source into `vault.json`:

   ```sh
   age -d -i key.txt -o vault.json "$(ls backups/source/snapshots/*.json.age | tail -n 1)"
   ```

   For the destination's backups, use `backups/dest` instead.

3. In any Bitwarden app, choose **Import data**, select the **Bitwarden (json)**
   format, and import `vault.json`.

4. Decrypt the attachments into an `attachments` directory:

   ```sh
   mkdir -p attachments
   for file in backups/source/attachments/*.age; do
     age -d -i key.txt -o "attachments/$(basename "$file" .age)" "$file"
   done
   ```

   Each file is named after its attachment id. The `vaultMirror.attachments` list
   at the end of `vault.json` gives each id's item and file name.

5. Delete the decrypted files:

   ```sh
   rm -r key.txt vault.json attachments
   ```

## Restoring with vault-mirror

vault-mirror can list snapshots, export one with its attachments, or write one
back into a vault. Run these commands in the directory with `compose.yaml`.

To list the source's snapshots:

```sh
docker compose run --rm vault-mirror snapshots
```

To decrypt the newest snapshot and its attachments into `backups/recovered`:

```sh
docker compose run --rm vault-mirror export --output /data/recovered
```

To write the newest snapshot of the source into the destination:

```sh
docker compose run --rm vault-mirror restore --to dest
```

`export` writes unencrypted files, so delete them when you're done. `restore`
recreates folders, collections, items, archive state and attachments, within
[server limits](limitations.md).

These options change what the commands use:

| Option | Effect |
| --- | --- |
| `--from dest` | Use the destination's backups instead of the source's. |
| `--snapshot NAME` | Use this snapshot instead of the newest. Not for `snapshots`. |
| `--identity PATH` | Your age identity file, for `BACKUP_RECIPIENTS`. Not for `snapshots`. |
| `--allow-non-empty` | Let `restore` write into a vault that already has items. Restoring the same snapshot twice this way creates duplicates. |

Without Docker, use `vault-mirror --env-file vault-mirror.env` in place of
`docker compose run --rm vault-mirror`, and any output directory you like.
