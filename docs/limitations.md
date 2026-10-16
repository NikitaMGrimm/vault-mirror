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

## Fidelity

Folders and collections are matched by name. Two source containers with the
same name can merge into one destination container. An existing collection's
external ID is kept, even when the source uses another value. Organization
permissions and membership are not reconstructed.

Item data includes stored passkey fields. The tests check that those fields
survive copying. They do not demonstrate a successful WebAuthn sign-in with a
copied passkey. Test any recovery method you rely on with the relevant service
and Bitwarden client.

A run verifies item content and attachments that it transfers. It does not
download every unchanged destination attachment for a complete content audit.

An unsuccessful attachment upload keeps its new server registration because
the file may have arrived despite the error. Old files stay in place. If the
new registration has no readable file, the next run stops during backup before
it can repair the mirror. The upload error names the allocated attachment and
item IDs. Stop scheduling, inspect that registration in a trusted client, and
preserve every readable file. Remove only a registration confirmed to be empty
before retrying the run.

## Server restrictions

**Attachments need Premium on Bitwarden.** If the destination is a free personal
account, vault-mirror copies everything else and warns how many attachments it
skipped. They follow once the account has Premium. Organizations don't need it.

**Bank accounts, driver's licences and passports.** Bitwarden only shows these
item types where it has turned them on. Until it does on the destination's
server, vault-mirror leaves them out with a warning, and `run --pull` does the
same for the source's server. Vaultwarden always shows them.

If the source stops showing a type whose copies still exist, the run stops
before removing those copies. Restore visibility on the source before
reconciling them.

**Organization items need a collection on Bitwarden.** Source items in no
collection are left out, with a warning, when the destination is a Bitwarden
server. `run --pull` does the same the other way round.

**Archived items** need a destination that supports archiving. Otherwise they're
copied unarchived, with a warning.

## Large vaults

Bitwarden's cloud accepts about 60 new items a minute when they're created one
at a time. vault-mirror imports new personal items in bulk and waits when the
server asks it to, but a large first run can still take a while. New
organization items are always created one at a time.

Attachments are held in memory while they're copied, so a large attachment needs
a few times its size in free memory.
