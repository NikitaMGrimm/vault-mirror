# Questions

[← Documentation](README.md)

## Does vault-mirror change my source vault?

Only when you ask: with `restore --to source`, or with `run --pull`, which copies
changes made in the destination back into the source.

## What happens to items I add or change in the destination?

Items you add there stay. vault-mirror changes and deletes only its copies, which
it recognizes by their [marker](how-it-works.md#how-copies-are-recognized).

If you edit a copy, the next run puts the source version back and warns you.
Your edit is in that run's destination backup. To change an item for good,
change it in the source.

## Can it sync in both directions?

No. The source always wins. If you used the destination while the source was
down, `run --pull` copies your changes back once. See
[After an outage](how-it-works.md#after-an-outage).

To make the other vault the source for good, see [Switching the source and
destination](how-it-works.md#switching-the-source-and-destination).

## What happens when I delete an item in the source?

The next run deletes its copy, which stays in that run's destination backup. A
run that would delete every copy stops. See
[safety checks](how-it-works.md#safety-checks).

## What happens when the source server is down?

The run fails before it changes anything, and the destination keeps what the
last run copied. The next run that reaches the source catches up.

## Do backups pile up?

No. Snapshots older than a day thin out to one a day and expire after
`RETENTION_DAYS`, and an unchanged vault gets no new snapshot. See
[Backups and restoring](backups.md).

## Where are my master passwords stored?

In the settings file, `vault-mirror.env`. On Unix, `init` makes it readable only
by you. To keep secrets out of it, use [secret files](configuration.md#secrets-in-files).
Anyone who can read them can open your vaults; see [SECURITY.md](../SECURITY.md).

## Does it work with two-step login?

Yes. vault-mirror logs in with each account's API key, which skips the second
step. It still needs the master password to decrypt the vault.

## Can I mirror more than one vault?

Each settings file mirrors one vault, a personal vault or one organization, into
one other. For more, give each pair its own settings file and backup directory.

## How do I know it's working?

- `vault-mirror healthcheck` fails when the last run failed or is too old. The
  container image uses it as its health check.
- `NOTIFY_FAILURE_URL` gets a message when a run fails.
- `last-run.json` in the backup directory holds the last run's result.
