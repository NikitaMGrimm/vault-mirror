# Limitations

[← Documentation](README.md)

## Supported servers

Either side can be any of these:

- Bitwarden's cloud, US or EU
- Bitwarden's self-hosted server (tested with 2026.9)
- Vaultwarden (tested with 1.37)

## Accounts

vault-mirror logs in with an account's API key and unlocks the vault with its
master password, like Bitwarden's own command-line client. An account without
a master password can't be used: one in an organization that unlocks with
single sign-on and trusted devices or Key Connector. Use an account that has a
master password on that side.

## What isn't copied

- Sends and account settings.
- Organization members, groups and collection permissions. Only items and
  collections are copied.
- Items in the source's trash.

The destination's trash is left alone. The server empties it on its own
schedule.

## Server restrictions

**Attachments need Premium on Bitwarden.** If the destination is a free personal
account, vault-mirror copies everything else and warns how many attachments it
skipped. They follow once the account has Premium. Organizations don't need it.

**Bank accounts, driver's licences and passports.** Bitwarden only shows these
item types where it has turned them on. Until it does on the destination's
server, vault-mirror leaves them out with a warning. Vaultwarden always shows
them.

**Organization items need a collection on Bitwarden.** Source items in no
collection are left out, with a warning, when the destination is a Bitwarden
server.

**Archived items** need a destination that supports archiving. Otherwise they're
copied unarchived, with a warning.

## Large vaults

Bitwarden's cloud accepts about 60 new items a minute when they're created one
at a time. vault-mirror imports new personal items in bulk and waits when the
server asks it to, but a large first run can still take a while. New
organization items are always created one at a time.

Attachments are held in memory while they're copied, so a large attachment needs
a few times its size in free memory.
