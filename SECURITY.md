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

vault-mirror writes to the source only when you run `restore --to source` or
`run --pull`. A normal run changes and deletes only the copies it made in the
destination.

## How automation changes your vault's security

An unattended mirror needs to unlock both vaults without you present. The
machine running it becomes another place an attacker could obtain your vault
content. Anyone who controls that machine or the vault-mirror process can read
the master passwords, use the API keys, and capture decrypted items and
attachments from both vaults. Folder and collection filters limit copying,
but backups and account access cover the whole configured scope.

API-key login does not ask for your interactive two-step login. Keep two-step
login enabled for your normal clients, but do not expect it to protect stored
automation credentials. The master passwords are used locally to decrypt the
vaults. They do not make a compromised runner safe.

Secrets in files reduce exposure through environment variables and container
inspection. They remain readable by the process that needs them and by an
administrator. Use a trusted, maintained host and protect access to both
accounts. Copying a password into a second account also gives anyone who can
unlock that account access to the password.

Use a separate backup passphrase. Keep a recoverable copy of `key.age` as well
as its passphrase. This protects encrypted backups if the disk or runner is stolen,
but a compromised running process can still capture the vault it decrypts.

## What it stores

| File | Contents | Protection |
| --- | --- | --- |
| `key.age` | The backup key. | Encrypted with your passphrase, using scrypt. |
| `*/snapshots/*.json.age` | Complete vault exports, secrets included. | Encrypted with age. |
| `*/attachments/<snapshot-id>/*.age` | Attachment content. Legacy files are directly under `attachments/`. | Encrypted with age. |
| `*/snapshots/*.meta.json` | Creation time, counts, opaque origin and encryption ids, and a ciphertext hash. | None. No plaintext vault fingerprint. |
| `last-run.json` | Time, result and counts of the last actual run. | None. No vault content. |
| `state.json` | The device ids vault-mirror logs in with. | None. |
| `run.lock` | Nothing. It's locked while a run or restore is using the directory. | None. |
| `unfinished-pull` | Nothing. It's there while a pull has not finished. | None. |

On Unix, backup and state files are readable only by their owner. Snapshots
and attachments are encrypted to the backup key in `key.age`.

New snapshots own separate attachment files and bind their ciphertext hashes
inside the encrypted document.

On upgrade, old plaintext fingerprints are removed from local sidecars. Those
fingerprints could help test a guessed item password if an attacker could
reconstruct every other byte of the exported document. They did not reveal the
master password. Historical copies outside this directory retain the fields
until you remove them or their retention ends.

> [!WARNING]
> You need both `key.age` and `BACKUP_PASSPHRASE` to read the backups. The passphrase cannot
> recreate a lost `key.age`. Keep recovery material somewhere other than the
> backup disk.

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
with URLs and the credentials that were sent removed. Attachment storage errors
omit the response body, which could echo a signed token. HTTP redirects are
refused. Configure each server's final address.

Server-supplied key derivation costs are bounded before use. Supported ranges
are 5,000 to 2,000,000 PBKDF2 iterations, or Argon2id with 2 to 10 iterations,
16 to 1,024 MiB of memory, and parallelism of 1 to 16. These include legacy
PBKDF2 accounts and Bitwarden's configurable ranges.

## The marker on each copy

Every copy carries a marker: a custom field called `vault-mirror`, of
Bitwarden's **Hidden** type, so apps show it masked until you reveal it. It
holds the source item's id and hashes of the item's content and of both items'
attachments.

The marker is encrypted with the destination's keys like any other field, so
only people who can open the destination can read it. What it adds to what they
can already see is the source item's id.
