# Backups and restoring

[← Documentation](README.md)

With backups on, which is the default, every run backs up the source and the
destination before it changes anything. The backups are ordinary files you can
read with the standard [`age`](https://age-encryption.org) tool and import into
any Bitwarden client, so you don't need vault-mirror to get your data back.

## What's in the backup directory

```text
backups/
├── key.age                         # the backup key, encrypted with BACKUP_PASSPHRASE
├── last-run.json                   # the result of the last run; no vault content
├── state.json                      # the device ids vault-mirror logs in with
├── run.lock                        # held while a run or restore is using the directory
├── source/
│   ├── snapshots/
│   │   ├── vault-20261014T030000Z.json.age
│   │   └── vault-20261014T030000Z.meta.json   # item count and attachment ids; no vault content
│   └── attachments/
│       └── <attachment id>.age
└── dest/
    └── …
```

A **snapshot** is a complete export of a vault's items (everything but the
trash), in Bitwarden's own JSON export format and encrypted as a whole. A new
snapshot is only written when the vault has changed since the newest one, so an
idle vault doesn't fill the disk.

Attachments are stored once per vault, however many snapshots refer to them.
When old snapshots are pruned, attachment files that no remaining snapshot needs
are deleted too. The source and destination are backed up separately, so a
copied attachment is stored in both.

### The backup key

With `BACKUP_PASSPHRASE`, vault-mirror generates an age key the first time it
runs and stores it only in `key.age`, encrypted with your passphrase. With
`BACKUP_RECIPIENTS`, it encrypts straight to your own age public keys instead,
and there's no `key.age`.

> [!WARNING]
> Changing `BACKUP_PASSPHRASE` doesn't re-encrypt `key.age`, and vault-mirror
> refuses to start with a passphrase that can't open it. Keep using the old
> passphrase, or start a new backup directory.

## Restoring without vault-mirror

All you need is `age` and your passphrase. These commands write your secrets to
disk unencrypted, so do this somewhere private and delete the files afterwards.

1. Decrypt the snapshot you want:

   ```console
   $ age -d -i backups/key.age backups/source/snapshots/vault-20261014T030000Z.json.age > vault.json
   ```

   `age` asks for the passphrase to open `key.age`. If you use
   `BACKUP_RECIPIENTS`, pass your own identity file to `-i` instead.

2. In any Bitwarden client, choose **Import data**, then the
   **Bitwarden (json)** format, and import `vault.json`.

3. Decrypt any attachments you need:

   ```console
   $ age -d -i backups/key.age backups/source/attachments/<attachment id>.age > attachment.bin
   ```

   To find out which item and file name an attachment id belongs to, look it up
   in the `vaultMirror.attachments` list near the end of `vault.json`.

## Restoring with vault-mirror

vault-mirror can list snapshots, export one, or write one straight back into a
vault, attachments and all:

```console
$ vault-mirror snapshots                      # list the source vault's snapshots
$ vault-mirror export --output ./recovered    # the newest snapshot, decrypted, with its attachments
$ vault-mirror restore --to dest              # write the newest source snapshot into the destination
```

`export` writes `export.json` and the attachment files into a new directory.
They aren't encrypted, so delete them when you're done.

`restore` writes into the configured source or destination vault. It recreates
folders, collections, items, archive state and stored attachments, within
[what the server allows](limitations.md). It won't write into a vault that
already has items unless you pass `--allow-non-empty`. Restoring the same
snapshot twice that way creates duplicates.

These options work with `snapshots`, `export` and `restore`:

| Option | Effect |
| --- | --- |
| `--from dest` | Use the destination's backups instead of the source's. |
| `--snapshot NAME` | Use this snapshot, as `snapshots` lists it, instead of the newest. (Not for `snapshots`.) |
| `--identity PATH` | The age identity file, for backups made with `BACKUP_RECIPIENTS`. (Not for `snapshots`.) |
