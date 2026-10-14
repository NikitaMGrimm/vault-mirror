# Limitations

[← Documentation](README.md)

The test suite runs against Vaultwarden 1.37 and Bitwarden's self-hosted server
2026.9, each as source and as destination. vault-mirror has also been
used with Bitwarden's cloud.

## What isn't copied

- **Sends and account settings.**
- **Organization members, groups and collection permissions.** Only the items
  and collections themselves are copied.
- **The source's trash.** Items in the trash are skipped.
- **The destination's trash** is left alone. The server empties it on its own
  schedule.

## Server restrictions

**Attachments on Bitwarden need Premium.** Bitwarden only accepts new
attachments from Premium accounts (organizations pay for their own). If the
destination is a free personal account, vault-mirror copies everything else and
warns you how many attachments it skipped. Attachments it copied earlier stay,
and the skipped ones follow once the account has Premium.

**Bank accounts, driver's licences and passports.** Bitwarden only shows these
newer item types to accounts where it has turned them on. Copies would be
invisible in a destination without them, so vault-mirror leaves these items out,
with a warning, until Bitwarden turns them on for that server. Vaultwarden shows
them everywhere.

**Organization items need a collection on Bitwarden.** Bitwarden's servers
require every organization item to be in at least one collection. Source items
in no collection are left out, with a warning, when the destination is a
Bitwarden server.

**Archived items** need a destination that supports archiving. Otherwise the
items are copied without their archive state, with a warning.

## Large vaults

Bitwarden's cloud accepts only about 60 new items a minute when they're created
one at a time. vault-mirror imports large batches of new personal items in bulk
instead, and waits whenever the server asks it to slow down. A big first run can
still take a while. New organization items are always created one at a time.

Attachments are held in memory while they're copied. A large attachment needs a
few times its size in free memory.
