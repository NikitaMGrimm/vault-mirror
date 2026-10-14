# Security

## Reporting a vulnerability

1. Open **Security → Report a vulnerability** in this GitHub repository.
2. Submit a private vulnerability report.

Do not report vulnerabilities in public issues.
Only the latest release receives fixes.

## What vault-mirror has access to

vault-mirror needs each configured vault's API key and master password.
It decrypts items to compare and copy their content.

> [!WARNING]
> Anyone who reads vault-mirror's secrets can open the configured vaults.
> Protect the settings file and secret files as carefully as the vaults.

Pass secrets through files with `*_FILE`, such as Docker or systemd credentials.
Other host processes might read secrets from environment variables.

> [!WARNING]
> Use a dedicated destination account.
> vault-mirror permanently deletes destination items that do not match the source.

## What is stored, and how

| File | Contents | Protection |
| --- | --- | --- |
| `key.age` | Backup key in passphrase mode. | The tool encrypts the key with `BACKUP_PASSPHRASE` and scrypt. |
| `*/snapshots/*.json.age` | Complete vault exports, including unencrypted secrets inside the encrypted file. | The tool encrypts snapshots with age. |
| `*/attachments/*.age` | Attachment content. | The tool encrypts attachments with age. |
| `*/snapshots/*.meta.json` | Creation time, item count, attachment ids, and a salted fingerprint. | No encryption. The file contains no vault content. |
| `last-run.json` | Time, result, and counts from the last run. | No encryption. The file contains no vault content. |
| `state.json` | Login device ids for this installation. | No encryption. |
| `run.lock` | No content. The tool locks this file during a run or restore. | No encryption. |

vault-mirror creates backup and state files with access limited to their owner on Unix.
In passphrase mode, it encrypts snapshots and attachments to the backup key.
With `BACKUP_RECIPIENTS`, it encrypts directly to the specified age public keys.

> [!WARNING]
> Recovery requires `BACKUP_PASSPHRASE` or the age identity file for `BACKUP_RECIPIENTS` backups.
> Without the required secret, you cannot decrypt backups.
> Keep the passphrase or identity file separate from the backup disk.

## What leaves the host

vault-mirror contacts only the configured servers and notification URLs.

| Destination | Data |
| --- | --- |
| Configured source and destination servers | API requests and attachment transfers. |
| `NOTIFY_SUCCESS_URL` or `NOTIFY_FAILURE_URL`, when configured | A one-line run summary with counts and error messages. The summary contains no vault content. |

vault-mirror omits request URLs from errors and logs.
Attachment download URLs contain access tokens.
The tool reduces server error responses to their message and removes URLs and request credentials.

## The mirror marker

Each copy has a marker: the `__source` custom field.
The marker uses Bitwarden's **Hidden** type, which masks the value until you reveal the value.
The marker contains the source item's id and hashes of item content and attachments.

The tool encrypts the marker with the destination vault's keys, like other fields.
Only users who can open the destination can read the marker.
The marker adds source item ids to information that these users can already see.
