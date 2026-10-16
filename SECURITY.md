# Security

## Reporting a vulnerability

Please report vulnerabilities privately: open **Security → Report a
vulnerability** in this repository and submit a report. Don't use public
issues for them. Fixes go into the latest release only.

## What vault-mirror can access

vault-mirror needs each vault's API key and master password, because it has to
decrypt items to compare and copy them.

> [!WARNING]
> Anyone who can read vault-mirror's settings or control its process can open
> both vaults. Two-step login doesn't protect API keys. Run it on a host you
> trust, and protect the settings file and any secret files like the vaults
> themselves.

Environment variables can be visible to other processes on the same host. To
keep secrets out of them, use [secret files](docs/configuration.md#secrets-in-files).

vault-mirror writes to the source only when you run `restore --to source` or
`run --pull`. A normal run changes and deletes only the copies it made in the
destination.

## What it stores

| File | Contents | Protection |
| --- | --- | --- |
| `key.age` | The backup key. | Encrypted with `BACKUP_PASSPHRASE`, using scrypt. |
| `*/<snapshot>/vault.json.age` | Complete vault exports, secrets included. | Encrypted to the backup key with age. |
| `*/<snapshot>/attachments/*.age` | Attachment content. | Encrypted to the backup key with age. |
| `last-run.json` | Time, result and counts of the last run. | None. No vault content. |
| `state.json` | The device ids vault-mirror logs in with. | None. |
| `run.lock`, `unfinished-pull` | Nothing. | None. |

On Unix, these files are readable only by their owner. Backups cover the whole
vault, whatever the folder and collection filters.

## What leaves the machine

vault-mirror connects only to your two servers, the attachment storage they
point it to, and, if you configure them, your notification URLs.

| Destination | What is sent |
| --- | --- |
| The source and destination servers | API requests and attachment transfers. |
| Attachment storage named by those servers, such as the Azure storage that Bitwarden's cloud uses | Encrypted attachment files. |
| `NOTIFY_SUCCESS_URL` and `NOTIFY_FAILURE_URL`, if set | A one-line summary of the run, with counts and error messages. No vault content. |

Logs and errors never include request URLs, which can carry access tokens, and
reduce server error responses to their message. HTTP redirects are refused, so
configure each server's final address.

vault-mirror refuses key derivation settings outside Bitwarden's ranges: 5,000
to 2,000,000 PBKDF2 iterations, or Argon2id with 2 to 10 iterations, 16 to
1,024 MiB of memory and parallelism of 1 to 16.

## The marker on each copy

Every copy carries a [marker](docs/how-it-works.md#how-copies-are-recognized):
a hidden custom field, encrypted like any other. It adds only the source item's
id to what someone who can open the destination already sees.
