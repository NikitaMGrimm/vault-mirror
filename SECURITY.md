# Security

## Reporting a vulnerability

Report vulnerabilities privately through GitHub: **Security → Report a
vulnerability** on this repository. Please do not open a public issue.

Only the latest release receives fixes.

## What vault-mirror has access to

vault-mirror needs each vault's API key and master password, because it
decrypts items to compare and copy them. Anyone who can read its
configuration can open both vaults. Treat the configuration like the vaults
themselves:

- Pass secrets as files (`*_FILE`), for example Docker or systemd
  credentials, rather than as environment variables that other processes on the
  host can read.
- Use a dedicated destination account. vault-mirror deletes everything in it
  that does not come from the source.

## What is stored, and how

| File | Contents | Protection |
| --- | --- | --- |
| `key.age` | The backup key | Encrypted with `BACKUP_PASSPHRASE` (scrypt) |
| `*/snapshots/*.json.age` | Complete vault exports, all secrets in the clear inside | Encrypted to the backup key (age) |
| `*/attachments/*.age` | Attachment content | Encrypted to the backup key (age) |
| `*/snapshots/*.meta.json` | Creation time, item count, attachment ids, a salted fingerprint | None. No vault content |
| `last-run.json` | Time, result and counts of the last run | None. No vault content |
| `state.json` | The device id this installation logs in with | None |
| `run.lock` | Nothing; locked while a run or restore is in progress | None |

All files are created readable by their owner only.

Without `BACKUP_PASSPHRASE`, or the identity file when `BACKUP_RECIPIENTS` is
used, the backups cannot be decrypted. Keep the passphrase somewhere other
than the backup disk.

## What leaves the host

- Requests to the two configured servers.
- When configured, a one-line run summary to `NOTIFY_SUCCESS_URL` or
  `NOTIFY_FAILURE_URL`. It contains counts and error messages, never vault
  content.

Error messages and logs leave out request URLs, because attachment download
links carry access tokens. Server error bodies are reduced to their message,
with any URL and any credential sent with the request removed.

## The mirror marker

Each mirrored item carries a custom field, `__source`, of Bitwarden's Hidden
type, which masks the value until it is revealed. It holds the source item's
id and hashes of the item's content and attachments. Like every other
field, it is encrypted with the destination vault's keys. Only someone who
can open the destination vault can read it, and all it adds to what they can
already see are the source item ids.
