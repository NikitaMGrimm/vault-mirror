# Security

## Reporting a vulnerability

Please report vulnerabilities privately: open **Security → Report a
vulnerability** in this repository and submit a report. Don't use public
issues for them. Fixes go into the latest release only.

## What vault-mirror can access

vault-mirror needs each vault's API key and master password, because it has to
decrypt items to compare and copy them.

> [!WARNING]
> Anyone who can read vault-mirror's settings can open your vaults. Protect the
> settings file, and any secret files, as carefully as the vaults themselves.

Environment variables can be visible to other processes on the same host. To
keep secrets out of them, put each one in a file and point to it with
`NAME_FILE`, for example with Docker secrets or systemd credentials. See
[Secrets in files](docs/configuration.md#secrets-in-files).

vault-mirror never writes to the source, except when you run
`restore --to source`. In the destination, it only changes and deletes the
copies it made.

## What it stores

| File | Contents | Protection |
| --- | --- | --- |
| `key.age` | The backup key, when you use `BACKUP_PASSPHRASE`. | Encrypted with your passphrase, using scrypt. |
| `*/snapshots/*.json.age` | Complete vault exports, secrets included. | Encrypted with age. |
| `*/attachments/*.age` | Attachment content. | Encrypted with age. |
| `*/snapshots/*.meta.json` | Creation time, item count, attachment ids, and a salted fingerprint. | None. No vault content. |
| `last-run.json` | Time, result and counts of the last run. | None. No vault content. |
| `state.json` | The device ids vault-mirror logs in with. | None. |
| `run.lock` | Nothing. It's locked while a run or restore is using the directory. | None. |

On Unix, backup and state files are readable only by their owner. With
`BACKUP_PASSPHRASE`, snapshots and attachments are encrypted to the backup key in
`key.age`. With `BACKUP_RECIPIENTS`, they're encrypted to your own age public
keys, so the machine making the backups can't read them back.

> [!WARNING]
> You need `BACKUP_PASSPHRASE`, or your age identity file when you use
> `BACKUP_RECIPIENTS`, to read the backups. Without it nobody can decrypt them.
> Keep it somewhere other than the backup disk.

## What leaves the machine

vault-mirror connects only to your two servers, the attachment storage they
point it to, and, if you configure them, your notification URLs.

| Destination | What is sent |
| --- | --- |
| The source and destination servers | API requests and attachment transfers. |
| Attachment storage named by those servers, such as the Azure storage that Bitwarden's cloud uses | Encrypted attachment files. |
| `NOTIFY_SUCCESS_URL` and `NOTIFY_FAILURE_URL`, if set | A one-line summary of the run, with counts and error messages. No vault content. |

Error messages and logs never include request URLs, because attachment download
URLs contain access tokens. Server error responses are cut down to their message,
with URLs and the credentials that were sent removed.

## The marker on each copy

Every copy carries a marker: a custom field called `vault-mirror`, of
Bitwarden's **Hidden** type, so apps show it masked until you reveal it. It
holds the source item's id and hashes of the item's content and of both items'
attachments.

The marker is encrypted with the destination's keys like any other field, so
only people who can open the destination can read it. What it adds to what they
can already see is the source item's id.
