# Backups and restoring

[← Documentation](README.md)

Each run backs up both vaults before it changes anything.

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

On first use, vault-mirror generates an age key and stores it only in
`key.age`, encrypted with your passphrase. Changing `BACKUP_PASSPHRASE` later
does not re-encrypt `key.age`, so keep the old passphrase or start a new
backup directory.

A snapshot is a complete vault export, secrets included, encrypted as a whole.
A new one is written only when the vault changed since the last.

Each attachment is stored once, however many snapshots refer to it, and
pruning deletes the ones no remaining snapshot needs. The source and the
destination keep separate backups, so a mirrored attachment is stored twice.

## Recovering without vault-mirror

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

## Restoring with vault-mirror

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
- `--snapshot NAME` picks a snapshot other than the newest, by the name
  `snapshots` lists.
