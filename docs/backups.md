# Backups and restoring

[← Documentation](README.md)

Every run backs up the whole source and destination before changing either
vault. The files use the standard [age](https://age-encryption.org) format, so
recovery needs neither vault-mirror nor either server.

## What's in the backup directory

```text
backups/
├── key.age                    # the backup key, encrypted with BACKUP_PASSPHRASE
├── last-run.json              # the last run's outcome
├── state.json                 # login device ids
├── run.lock                   # held while a command uses the directory
├── folders/                   # ids of the folders runs created
├── source/
│   └── 20261014T030000Z/      # one snapshot
│       ├── vault.json.age
│       └── attachments/
│           └── <attachment id>.age
└── dest/
    └── …
```

A **snapshot** contains the configured personal vault or organization without
the trash, with its own copy of every attachment, so ten snapshots of a vault
with 1 GiB of attachments take about 10 GiB. An unchanged vault gets no new
snapshot.

Snapshots are kept like this:

- everything from the last 24 hours,
- the newest of each older day, for `RETENTION_DAYS` (30 by default),
- and always the newest snapshot.

## Keep the key

The first run creates a random backup key and stores it in `key.age`, encrypted
with `BACKUP_PASSPHRASE`. You need **both** to read the backups, so keep copies
of both somewhere other than the backup disk. If `key.age` goes missing, runs
stop until you put it back.

## Restore with vault-mirror

Stop the daemon first, so no run changes the vaults while you recover:

```sh
docker compose stop vault-mirror
```

List the snapshots of the source, or of the destination with `--from dest`:

```sh
docker compose run --rm vault-mirror snapshots
```

Export the newest snapshot and its attachments, unencrypted. With the generated
Compose file, this writes to `backups/recovered` on the host. Pause anything
that copies or syncs the backup directory before exporting:

```sh
docker compose run --rm vault-mirror export --output /data/recovered
```

Import `backups/recovered/export.json` into an empty vault with any Bitwarden
client, then attach the files from `attachments/<attachment id>/attachment`
to their items. Each entry in `vaultMirror.attachments` in `export.json` gives
the attachment id, item id and original file name. Use that name when you
attach the file again.
Delete `backups/recovered` when done, before resuming backup copies or syncs.
Bitwarden imports do not restore the time a password last changed.

If the export fails, unencrypted files may remain in the output directory.
Inspect them and choose a new `--output` directory to retry.

The export leaves out the hidden `vault-mirror` fields, so imported items are
not copies. To rebuild a lost destination, mirror into an empty one instead.

Snapshots record the vault before a run changes it. A destination snapshot can
therefore be older than the current mirror. Choose the snapshot you want to
recover before importing it.

| Option | Effect |
| --- | --- |
| `--from dest` | Read the destination's snapshots. |
| `--snapshot ID` | Use this snapshot instead of the newest. |

Listing snapshots needs only `BACKUPS_DIR`. Exporting also needs
`BACKUP_PASSPHRASE` or `BACKUP_PASSPHRASE_FILE`; neither command needs access to
the servers. Without Docker, use a path on your own machine for the output:

```sh
vault-mirror --env-file vault-mirror.env snapshots
vault-mirror --env-file vault-mirror.env export --output ./recovered
```

The native export writes to `recovered` in your working directory. This output
also contains unencrypted vault data; delete it when done.

You can list and export backups from a read-only directory if it contains a
readable `run.lock`. Keep that file when copying backups. If it is missing,
the commands create it in a writable backup directory. Otherwise, copy the
backups to a writable directory before recovering them.

If you restored into the same source account, the imported items have new ids.
The next run can stop because it would replace more than half of the old copies.
Preview that replacement, then apply it if the plan matches your recovery:

```sh
docker compose run --rm vault-mirror check --allow-mass-delete
docker compose run --rm vault-mirror run --allow-mass-delete
```

The flag applies to that run only. When you're done, start the daemon again with
`docker compose up -d`.

## An attachment without a file

An upload that stopped part way can leave an attachment that has no file. A
backup can't download it, so every run stops before it changes anything, with
an error such as:

```text
error: vault-mirror FAILED; during destination backup: downloading attachment <attachment id> of item <item id>: …
```

Until you fix it, no new snapshot of that vault is made and none is pruned, so
the last complete one stays. To export it, find its id with `snapshots` and pass
it to `export`:

```sh
docker compose run --rm vault-mirror snapshots --from dest
docker compose run --rm vault-mirror export --from dest --snapshot ID --output /data/recovered
```

For the source, use `--from source` instead.

To fix it, open the item in a Bitwarden client you trust, delete the attachment
the error names, and attach the file again if you have it, for example from the
export. If a later run stops at that copy because its attachments changed, see
[How copies are recognized](how-it-works.md#how-copies-are-recognized).

## Restore without vault-mirror

These commands write secrets unencrypted. Work in a private directory and
delete the files when done.

1. Decrypt the key. `age` asks for the passphrase:

   ```sh
   umask 077
   age -d -o key.txt backups/key.age
   ```

2. Decrypt a snapshot:

   ```sh
   age -d -i key.txt -o vault.json backups/source/20261014T030000Z/vault.json.age
   ```

3. Import `vault.json` as **Bitwarden (json)**.

4. `vaultMirror.attachments` in `vault.json` lists each attachment's item,
   id and file name. Decrypt each file and attach it to its item in a client:

   ```sh
   age -d -i key.txt -o recovered-file backups/source/20261014T030000Z/attachments/<attachment id>.age
   ```
