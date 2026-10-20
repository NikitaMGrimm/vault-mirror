# Limitations

[← Documentation](README.md)

## Supported servers

Either side can be any of these:

- Bitwarden's cloud, US or EU
- Bitwarden's self-hosted server (tested with 2026.9)
- Vaultwarden (tested with 1.37)

## Accounts

vault-mirror needs an account with a master password. An account that unlocks
with single sign-on and trusted devices or Key Connector can't be used.

## What isn't copied

- Sends and account settings.
- Organization members, groups and collection permissions. Only items and
  collections are copied.
- Items in the source's trash.

The destination's trash is left alone. The server empties it on its own
schedule.

## Names and damaged items

Folders and collections are matched by name, so two with the same name in the
source share one in the destination.

A run stops before it changes anything if it can't decrypt an item, if item
metadata has an invalid type, or if an attachment has no readable file, such
as after a failed upload. For an unreadable attachment, the error gives the
item and attachment ids; see [An attachment without a file](backups.md#an-attachment-without-a-file).
Items stored as one
encrypted data blob aren't supported yet and stop the run too.

## Server restrictions

A run fails and sends the failure notification when the destination can't hold
something selected for copying. Except for archiving, the run checks these
restrictions before it changes anything. A failed run keeps the backups it
made at its start.

**Attachments need Premium on Bitwarden.** A run into a free personal
destination stops when it has attachments to copy. Organizations don't need
Premium. To mirror items without their attachments, set
[`SKIP_ATTACHMENTS=true`](configuration.md#what-to-mirror). Encrypted backups
still include the attachments.

**Bank accounts, driver's licences and passports.** Bitwarden only shows these
item types where it has turned them on. Until it does on the destination's
server, a run stops while the source has any. Vaultwarden always shows them. If
the source reports them hidden when its vault is read, the run stops rather
than delete their copies. If the server reports different visibility before
and after the read, the run stops before saving or pruning that vault's
snapshots.

**Organization items need a collection on Bitwarden.** When the destination is
a Bitwarden server, a run stops while source items are in no collection.

**Existing organization copies on Bitwarden cannot be updated safely.** Item
updates can erase members' private folder, favorite and archive data, which
the API does not expose for backup. A run that would update an existing copy
stops before any vault changes, even with safety overrides. New copies are
supported. Do not edit them until the run finishes. An interrupted copy that
needs an update on retry also stops the run. Use a Vaultwarden destination
for ongoing organization mirroring.

**Archived items** need a destination that supports archiving. A run stops if
the destination refuses to archive a copy.

## Changes during a run

Make lasting edits in the source. Avoid editing destination copies while a run
is in progress.

Source visibility checks are separate requests from the vault read. A setting
can turn off and back on between the checks without being detected. The checks
cannot guarantee that the source showed every item during that interval.

A run plans from the reads its backups saved. A change made in the destination
after that, to a copy the run then writes, can be lost without a backup. Before
it deletes copies, a run reads them again and stops if one changed, but a
change made between that read and the deletion still goes to the trash with
the copy.

## Large vaults

Bitwarden's cloud rate-limits new items, so a large first run can take a while,
especially into an organization. Attachments are held in memory while they're
copied, so a large one needs a few times its size in free memory.
