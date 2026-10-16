# Questions

[← Documentation](README.md)

## Does vault-mirror change my source vault?

No. It only reads the source. The exceptions are the ones you ask for: a restore
with `restore --to source`, and `run --pull`, which copies changes from the
destination back into the source.

## What happens to items I add or change in the destination?

Items you add there stay. vault-mirror changes and deletes only its copies, which
it recognizes by their [marker](how-it-works.md#how-copies-are-recognized).

If you edit a copy, the next run puts the source version back and warns you.
With backups on, your edit is in that run's destination backup. To change an
item for good, change it in the source.

## Can it sync in both directions?

No. The source always wins. With two-way sync, an item changed on both sides has
no right answer, and a mistake on either side would spread to the other. Make
your changes in the source, and use the destination to read from when the
source is unavailable.

If you used the destination while the source was down, `run --pull` copies what
you changed there back into the source once, and then mirrors as usual. See
[After an outage](how-it-works.md#after-an-outage).

You can also switch which vault is the source for good, for example to move to
the other server. vault-mirror then updates the items that already exist
instead of copying everything again. See [Switching the source and
destination](how-it-works.md#switching-the-source-and-destination).

## What happens when I delete an item in the source?

The next run deletes its copy. The copy is still in the destination backup from
that run. A run that would delete every copy stops, because that usually means
an empty or wrong source. See [safety checks](how-it-works.md#safety-checks).

## What happens when the source server is down?

The run fails before it changes anything, and the destination keeps what the last
run copied. That's what the destination is for. The next run that reaches the
source catches up.

## Are backups optional?

They're on by default, and every run backs up both vaults before it changes
anything. They need `BACKUP_PASSPHRASE` or `BACKUP_RECIPIENTS`. Set
`BACKUP_ENABLED=false` to mirror without them. Without a destination,
vault-mirror only makes backups. See [Backups and restoring](backups.md).

## Do backups pile up?

Current snapshots thin out to one a day and expire after `RETENTION_DAYS`,
subject to `MIN_FILES`. New snapshots each hold their own attachments. An
unchanged vault can be skipped when the runner can authenticate its earlier
backup. Public-recipient backups save a point every run. Legacy snapshots and
inactive encryption groups are preserved. See
[How long backups are kept](backups.md#how-long-backups-are-kept).

## Where are my master passwords stored?

In the settings file, `vault-mirror.env`. On Unix, `init` makes it readable only
by its owner. On Windows, create it in a directory restricted to your account
and check the file's access permissions. To keep secrets out of it, put each one
in its own file and point to it with `NAME_FILE`. See
[Secrets in files](configuration.md#secrets-in-files).

## Does it work with two-step login?

Yes. vault-mirror logs in with each account's API key, which Bitwarden doesn't
ask a second step for. It still needs the master password to decrypt the vault.

## Does storing my master passwords make the vault less secure?

It adds a machine that can unlock both vaults without you present. Anyone who
can read its settings or control the running process can obtain your vault
content. Two-step login does not protect the stored API keys.

Use a trusted host, protect both accounts, and restrict access to the settings
and secret files. `NAME_FILE` keeps secrets out of the environment, but the
process still has to read them. Public-recipient backups let you keep their
private decryption key off the runner. They cannot protect a vault from a
compromised process that is currently decrypting it.

Read [How automation changes your vault's security](../SECURITY.md#how-automation-changes-your-vaults-security)
before choosing where to run it.

## Can I mirror more than one vault?

Each settings file mirrors one vault, a personal vault or one organization, into
one other. For more, give each pair its own settings file and backup directory,
and run vault-mirror once for each.

## How do I know it's working?

- `vault-mirror healthcheck` fails when the last actual run failed or is too old. The
  container image uses it as its health check.
- `NOTIFY_FAILURE_URL` gets a message when a run fails.
- `last-run.json` in the backup directory has the time, result and counts of the
  last actual run. Dry runs leave it unchanged.

See [Commands and scheduling](running.md).
