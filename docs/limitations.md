# Limitations

[← Documentation](README.md)

## Supported servers

Either side can be any of these:

- Bitwarden's cloud, US or EU
- Bitwarden's self-hosted server (tested with 2026.10)
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
item types where it has turned them on. If either server reports them hidden,
even in a vault with only logins, runs and checks stop. This includes
Bitwarden's cloud while these types are disabled. Vaultwarden always shows them.
The read fails before saving or pruning that vault's snapshots. Existing
snapshots remain available for recovery and export. A run also stops if
visibility changes around a vault read or the destination becomes restricted
before writing or during final verification.

**Organization credential leasing isn't supported.** Disable collection access
rules before using an organization as a source or destination. Reads stop before
saving or pruning that vault's snapshots while any collection has an enabled
access rule.

**Organization items need a collection on Bitwarden.** When the destination is
a Bitwarden server, a run stops while source items are in no collection.

**Existing organization copies on Bitwarden cannot be updated safely.** Item
updates can erase members' private folder, favorite and archive data, which
the API does not expose for backup. Backups cannot restore this metadata.
Updates stop by default. You can explicitly
[accept this loss](configuration.md#organizations) for runs and checks,
including scheduled runs. New copies need no opt-in. Do not edit them until
the run finishes. An interrupted copy that needs an update on retry also needs
permission. Use a Vaultwarden destination to preserve members' private metadata.

**Archived items** need a destination that supports archiving. A run stops if
the destination refuses to archive a copy.

## Server address changes

Markers identify the exact server addresses, accounts and scopes used to make
copies. If these change, a run stops before creating a copy linked to an item
from the previous pair. Safety flags do not bypass this check.

When moving a server, keep the original configured URL reachable through DNS
or a reverse proxy with valid TLS. If you cannot keep that URL, stop the daemon
and use a fresh empty destination account or organization. Retain the previous
vault and its backups, including any independent edits. Run `check` before
copying to the new destination.

## Changes during a run

Make lasting edits in the source. Avoid editing destination copies while a run
is in progress.

Visibility checks are separate requests from each vault read. A setting
can turn off and back on between the checks without being detected. The checks
cannot guarantee that the server showed every item during that interval.

A run plans from the reads its backups saved. A change made in the destination
after that, to a copy the run then writes, can be lost without a backup. Before
it deletes copies, a run reads them again and stops if one changed, but a
change made between that read and the deletion still goes to the trash with
the copy.

## Large vaults

Bitwarden's cloud rate-limits new items, so a large first run can take a while,
especially into an organization. Attachments are held in memory while they're
copied, so a large one needs a few times its size in free memory.
