# Limitations

[← Documentation](README.md)

- Sends, organization members, groups, collection permissions, and account
  settings are not mirrored.
- Items in the source's trash are not mirrored. Items in the destination's
  trash are left alone; the server empties its trash itself.
- Bank accounts, driver's licences and passports are new item types. Bitwarden
  shows them only to accounts it has enabled them for. Copies made in a
  destination that hides them would be invisible, so they are left out of the
  mirror, with a warning, until Bitwarden enables them.
- Archived items need a server that supports archiving. If the destination
  does not, items are mirrored unarchived, and the run reports a warning.
- Tested against Vaultwarden 1.37, Bitwarden's self-hosted server 2026.9,
  and Bitwarden's cloud.
- Bitwarden accepts new attachments only from premium accounts. Into a
  personal vault without premium, everything else is mirrored, new attachments
  are left out with a warning, and attachments already mirrored stay. They
  follow once the account has premium.
- Bitwarden's cloud limits how fast a client may write: about 60 single item
  creations a minute. vault-mirror imports large numbers of new items in bulk
  and waits out rate limits, so a large first run takes longer but completes.
  New organization items are still created one at a time.
- Bitwarden's servers require every organization item to be in a collection.
  Source items in no collection are left out of such a destination, with a
  warning.
- Attachments are held in memory while they are copied, so a large attachment
  needs a few times its size in free memory.
