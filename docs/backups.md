# Backups and restoring

[← Documentation](README.md)

With backups enabled, each run backs up both configured vaults before changing the destination.
Without a destination, vault-mirror backs up only the source.

## Layout

```text
backups/
├── key.age                         # backup key, encrypted with BACKUP_PASSPHRASE
├── last-run.json                   # result of the last run; no vault data
├── run.lock                        # held while a run or restore uses this directory
├── source/
│   ├── snapshots/
│   │   ├── vault-20261014T030000Z.json.age
│   │   └── vault-20261014T030000Z.meta.json   # item count and attachment ids; no vault data
│   └── attachments/
│       └── <attachment id>.age
└── dest/
    └── …
```

With `BACKUP_PASSPHRASE`, vault-mirror generates an age key on first use.
vault-mirror stores this key in `key.age`, encrypted with your passphrase.
With `BACKUP_RECIPIENTS`, vault-mirror encrypts directly to your public keys and does not create `key.age`.

> [!WARNING]
> Changing `BACKUP_PASSPHRASE` does not encrypt `key.age` with the new passphrase.
> vault-mirror rejects a passphrase that cannot decrypt the existing key.
> Keep the old passphrase, or use a new backup directory.

A snapshot contains a complete export of the vault's items outside the trash, including secrets.
vault-mirror encrypts the whole snapshot.
vault-mirror writes a new snapshot only when the vault changes.

vault-mirror stores each attachment once per vault, regardless of snapshot count.
Pruning deletes attachment files that no remaining snapshot needs.
The source and destination have separate backups, so vault-mirror stores a copied attachment twice.

## Recovering without vault-mirror

Recovery from passphrase-protected backups requires the standard [`age`](https://github.com/FiloSottile/age) tool and your passphrase.

> [!WARNING]
> These commands write decrypted secrets to disk.
> Restrict access to the output files.

1. Decrypt the required snapshot into `vault.json`.
2. Decrypt each required attachment into a separate file.

```sh
age -d -i backups/key.age backups/source/snapshots/vault-20261014T030000Z.json.age > vault.json
age -d -i backups/key.age backups/source/attachments/<attachment id>.age > attachment.bin
```

`age` asks for the passphrase to decrypt `key.age`.
For recipient-key backups, use your age identity file instead of `backups/key.age`.

Import `vault.json` into any Bitwarden client through **Import data → Bitwarden (json)**.
The `vaultMirror.attachments` list maps each attachment id to its item and file name.

## Restoring with vault-mirror

> [!WARNING]
> `export` writes decrypted secrets and attachment content to disk.
> Keep the output directory private.

```sh
vault-mirror snapshots                       # list the source vault's snapshots
vault-mirror export --output ./recovered     # newest snapshot as export.json plus attachment files, in the clear
vault-mirror restore --to dest               # write the newest source snapshot into the destination vault
```

`export` requires an output directory that does not exist.
`restore` writes into the configured source or destination.

| Option | Effect |
| --- | --- |
| `--allow-non-empty` | vault-mirror adds snapshot items to existing items. Without this flag, vault-mirror refuses a vault with items outside the trash. |
| `--from dest` | vault-mirror reads destination backups instead of source backups. |
| `--snapshot NAME` | vault-mirror selects the snapshot name that `snapshots` lists. The default is the newest snapshot. |
| `--identity PATH` | vault-mirror reads the age identity for `BACKUP_RECIPIENTS` backups. |

`restore` recreates folders, collections, items, archive state, and stored attachments, subject to [server limitations](limitations.md).
Restoring the same snapshot again with `--allow-non-empty` creates duplicate items.
