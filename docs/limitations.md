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

A run stops if it can't decrypt an item, or if a destination attachment has no
readable file, such as after a failed upload. The error gives the id of the
item or attachment.

## Server restrictions

**Attachments need Premium on Bitwarden.** A free personal destination gets
everything else, with a warning. Organizations don't need Premium.

**Bank accounts, driver's licences and passports.** Bitwarden only shows these
item types where it has turned them on. Until it does on the destination's
server, vault-mirror leaves them out with a warning, and `run --pull` does the
same for the source's server. Vaultwarden always shows them. If the source
stops showing them, the run stops rather than delete their copies.

**Organization items need a collection on Bitwarden.** Source items in no
collection are left out, with a warning, when the destination is a Bitwarden
server. `run --pull` does the same the other way round.

**Archived items** need a destination that supports archiving. Otherwise they're
copied unarchived, with a warning.

## Large vaults

Bitwarden's cloud rate-limits new items, so a large first run can take a while,
especially into an organization. Attachments are held in memory while they're
copied, so a large one needs a few times its size in free memory.
