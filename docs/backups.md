# Backups and restoring

[← Documentation](README.md)

Backups are on by default. Every run backs up the whole source and destination
scope before changing either vault. Folder and collection filters limit copying,
not backups. The encrypted files use the standard [age](https://age-encryption.org)
format, so recovery does not require vault-mirror or either server.

## What's in the backup directory

```text
backups/
├── key.age                          # needed with BACKUP_PASSPHRASE
├── last-run.json                    # last actual run's outcome
├── state.json                       # login device ids
├── run.lock                         # serializes access
├── unfinished-pull                  # present while a pull has not finished
├── source/
│   ├── snapshots/
│   │   ├── vault-20261014T030000Z.json.age
│   │   └── vault-20261014T030000Z.meta.json
│   └── attachments/
│       └── vault-20261014T030000Z/
│           └── attachment-id.age
└── dest/
    └── …
```

A **snapshot** holds a full vault export without the trash. Each new snapshot
owns its attachment files. Its encrypted document records their paths and
ciphertext hashes. Older snapshots may share flat `attachments/<id>.age` files.
Those files stay readable and are never overwritten by a new backup.

With a local decryption key, a run can skip an unchanged vault after checking
its newest matching snapshot and every promised attachment. With
`BACKUP_RECIPIENTS`, each run writes a new recovery point because the runner
cannot decrypt earlier backups.

Each changed recovery point downloads and stores another complete attachment
set. If a vault has 1 GiB of attachments, ten retained points need about 10 GiB
plus their JSON exports. Recipient-only backups download that set on every run.
Plan disk space and network use for both vaults.

Backup writes synchronize encrypted file contents. Unix runs also synchronize
the key's publication and backup directory entries before allowing vault
changes. Directory durability barriers are not implemented on Windows. Keep
independent copies of your recovery material on every platform.

### How long backups are kept

After a verified backup, pruning applies to snapshots of the same vault and
encryption setup:

- Keep every snapshot from the last 24 hours.
- For older days, keep the newest snapshot of each day in UTC.
- Delete snapshots older than `RETENTION_DAYS`, 30 by default.
- Always keep at least `MIN_FILES` newest snapshots, 10 by default.

Each removed snapshot takes its own attachment directory with it. Repeated
identical recipient-only recovery points count toward `MIN_FILES`. That setting
does not guarantee ten distinct vault states.

Legacy snapshots, unfinished publications, and inactive encryption groups are
preserved. Their storage is additional to current retention. Review them
manually after testing recovery. Malformed or unknown metadata stops automatic
pruning. `ENABLE_PRUNING=false` keeps everything.

### Keep the recovery key

With `BACKUP_PASSPHRASE`, the first run generates a random key and encrypts it
into `key.age`. You need **both** that file and the passphrase. The passphrase
cannot recreate a lost key. Keep a separate copy of both.

A missing `key.age` in a populated directory stops new backups. Restore the
original file or choose a new empty `BACKUPS_DIR`. Changing the passphrase does
not re-encrypt the existing key file.

With `BACKUP_RECIPIENTS`, keep the private identity off the runner and keep
recovery copies elsewhere. New recipient settings apply to new snapshots.
Older snapshots still need their original identities and are kept outside
the new encryption group's retention.

An upgrade removes old plaintext snapshot fingerprints from local metadata.
Copies already held by rclone, restic, filesystem snapshots, or another backup
service keep those fingerprints until you remove them or their retention ends.

## Restore with vault-mirror

Stop the daemon and every other scheduler using these vaults before recovery:

```sh
docker compose stop vault-mirror
```

List source snapshots:

```sh
docker compose run --rm vault-mirror snapshots
```

Export the latest complete snapshot and its attachments:

```sh
docker compose run --rm vault-mirror export --output /data/recovered
```

The output is unencrypted. Import its `export.json` with a Bitwarden client,
then attach the recovered files to their items. Delete the output when done.
Export removes operative mirror markers, including in old snapshots.

To restore the source's latest complete snapshot into the destination:

```sh
docker compose run --rm vault-mirror restore --to dest
```

Restore reads every attachment file from the backup before it writes anything,
so a damaged backup restores nothing. Afterwards it reads the vault again and
checks every restored item. Omitted attachments and
[unsupported features](limitations.md) are reported as warnings.

Restored items have no mirror markers, even from a destination backup, so a run
into a restored destination copies the source next to them. To rebuild a lost
destination, mirror into an empty one instead. A restore that fails partway
leaves what it created; retrying with `--allow-non-empty` can create duplicates.

| Option | Effect |
| --- | --- |
| `--from dest` | Read destination backups. |
| `--snapshot NAME` | Select a named recovery point instead of `latest`. |
| `--identity PATH` | Use an age private identity file. All supported identities in it are tried. |
| `--allow-non-empty` | Add recovered items to a vault that already has items. |

`snapshots` needs only `BACKUPS_DIR`. `export` additionally needs the existing
backup passphrase or `--identity`. Restore additionally needs only the chosen
target account and transport settings. Unavailable credentials for the other
vault do not block recovery. `BACKUP_ENABLED=false` does not prevent reading.

Without Docker, replace `docker compose run --rm vault-mirror` with
`vault-mirror --env-file vault-mirror.env`. Mount an identity file read-only
when using it in a container.

## Restore without vault-mirror

Stop all scheduling first. These commands write secrets unencrypted. Use a
private directory and delete decrypted files when done.

1. Decrypt the key. `age` asks for the passphrase:

   ```sh
   umask 077
   age -d -o key.txt backups/key.age
   ```

   With `BACKUP_RECIPIENTS`, use your private identity instead of `key.txt`.

2. Choose an explicit snapshot filename. Same-second counters sort numerically.
   Do not select the newest with a lexical `ls | tail` pipeline. For example:

   ```sh
   age -d -i key.txt -o vault.json backups/source/snapshots/vault-20261014T030000Z.json.age
   ```

3. Import `vault.json` as **Bitwarden (json)**. New format-2 snapshots have no
   operative markers. Old destination snapshots can retain them. Use the
   upgraded `export` command to sanitize those before importing into an active
   mirror vault.

4. In `vaultMirror.attachments`, find each stored record's `snapshotId`,
   `attachmentId`, and `fileName`. Decrypt the matching file:

   ```sh
   age -d -i key.txt -o recovered-file backups/source/attachments/vault-20261014T030000Z/attachment-id.age
   ```

   Legacy records have no `snapshotId`. Their file is
   `backups/source/attachments/<attachmentId>.age`. A record with `stored:false`
   has no backed-up content. Attach each recovered file to its item in a client.

5. Delete decrypted files.
