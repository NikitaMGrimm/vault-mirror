# Questions

[← Documentation](README.md)

## Does vault-mirror change my source vault?

No. It only reads the source. The one exception is a restore you ask for with
`restore --to source`.

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

No. A run saves a snapshot only when the vault changed. Old snapshots thin out
to one a day and are deleted after `RETENTION_DAYS`. Each attachment is stored
once, however many snapshots use it. See [How long backups are
kept](backups.md#how-long-backups-are-kept).

## Where are my master passwords stored?

In the settings file, `vault-mirror.env`. `init` writes it readable only by you.
To keep secrets out of it, put each one in its own file and point to it with
`NAME_FILE`. See [Secrets in files](configuration.md#secrets-in-files).

## Does it work with two-step login?

Yes. vault-mirror logs in with each account's API key, which Bitwarden doesn't
ask a second step for. It still needs the master password to decrypt the vault.

## Can I mirror more than one vault?

Each settings file mirrors one vault, a personal vault or one organization, into
one other. For more, give each pair its own settings file and backup directory,
and run vault-mirror once for each.

## How do I know it's working?

- `vault-mirror healthcheck` fails when the last run failed or is too old. The
  container image uses it as its health check.
- `NOTIFY_FAILURE_URL` gets a message when a run fails.
- `last-run.json` in the backup directory has the time, result and counts of the
  last run.

See [Commands and scheduling](running.md).
