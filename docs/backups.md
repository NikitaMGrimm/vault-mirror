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

A **snapshot** is a full vault export without the trash, with its own copy of
every attachment, so ten snapshots of a vault with 1 GiB of attachments take
about 10 GiB. An unchanged vault gets no new snapshot.

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

Export the newest snapshot and its attachments, unencrypted:

```sh
docker compose run --rm vault-mirror export --output /data/recovered
```

Import `export.json` with any Bitwarden client, then attach the files from
`attachments/<attachment id>/` to their items. Delete the output when done.
The export leaves out the hidden `vault-mirror` fields, so imported items are
not copies. To rebuild a lost destination, mirror into an empty one instead.

| Option | Effect |
| --- | --- |
| `--from dest` | Read the destination's snapshots. |
| `--snapshot ID` | Use this snapshot instead of the newest. |

These commands need only `BACKUPS_DIR` and `BACKUP_PASSPHRASE`. Without Docker,
replace `docker compose run --rm vault-mirror` with
`vault-mirror --env-file vault-mirror.env`.

When you're done, start the daemon again with `docker compose up -d`.

## An attachment without a file

An upload that stopped part way can leave an attachment that has no file. A
backup can't download it, so every run stops before it changes anything, with
an error such as:

```text
error: the run failed during destination backup: downloading attachment <attachment id> of item <item id>: …
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
