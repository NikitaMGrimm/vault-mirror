# How it works

[← Documentation](README.md)

vault-mirror makes one vault, the **destination**, match another, the
**source**. Each time it does this is a **run**.

## A run, step by step

1. **Back up the source** as an encrypted **snapshot**, with its attachments.
   An unchanged vault gets no new snapshot.
2. **Back up the destination** the same way.
3. **Compare.** It prints what it will create, update and delete, and runs the
   [safety checks](#safety-checks).
4. **Change the destination**: create folders or collections, create and
   update items, copy attachments out of the source snapshot, then move
   removed copies to the trash.
5. **Verify.** It reads the destination back and compares it with the source.
   Then it deletes the [folders it no longer needs](#folders).
6. **Report.** It writes the result to `last-run.json` and to your notification
   URL, if you set one.

A failed step stops the run. The result says whether no destination writes
started or the destination may contain partial changes. Counts in a failed
result are the planned work. Run again to recheck and finish the copies.
Without a destination, a run only backs up the source.

In `last-run.json`, `mirror.state` is `complete` or `incomplete`. Failed runs
keep counts in `mirror.planned` and write uncertainty in `mirror.writes`.
The first run after upgrading replaces the old status format. Until then,
`healthcheck` cannot read an older untagged status file.

Folder cleanup is best effort. If a deletion cannot be confirmed, required
mirroring can still succeed. The result then records `changed: null` and the
notification reports the cleanup uncertainty.

## How copies are recognized

Each item vault-mirror creates in the destination is a **copy**. Each copy has a
**marker**: a hidden custom field called `vault-mirror`. It names the two
vaults the copy was made between, its source item, and hashes of the state the
run left the copy in. It holds no secrets. A marker that names another pair of
vaults, such as on an item mirrored on from a third vault, doesn't make an item
a copy here.

Items without a marker aren't copies. vault-mirror never changes or deletes
them, so you can keep your own items in the destination. It never deletes
collections, or folders it didn't make; delete an empty one yourself.

- If you change a copy's content, attachments, archive state, folder in a
  personal vault or collections in an organization, the next run stops before
  it changes anything and lists the copy's id. Make lasting changes in the
  source. To put the source version back, run once with `--allow-overwrite`;
  that run's destination backup keeps your change.
  Moving the source item to the trash, excluding it with a filter, or switching
  the source and destination does not remove this protection.
- In an organization, folders are each member's own. A copy you file in one
  stays there.
- If you delete a copy's marker, the item is no longer a copy, and the next run
  makes a new one next to it.
- A clone keeps the copy's marker. Remove its `vault-mirror` field to keep
  it independent. Runs delete duplicates only when both copies are complete,
  unchanged, and record the same source version.

### Folders

A run deletes a personal destination folder only if it created it for this pair.
No selected source folder may have that name, and no item may refer to it,
including items in the trash.
It keeps folders that were there before, folders you renamed, and folders that
hold anything. While the destination account is in an organization, or its
server doesn't show every item type, obsolete folders stay with a warning. `check`
shows how many folders a run would delete.

Runs record the folders they create in `folders/` in the backup directory.
Without that record, they delete no folder.

### After an outage

The destination is there for when the source is down. Runs don't overwrite
copies you change in it meanwhile: they stop and list them. Items you add there
stay.

Once the source is back, make the same changes there. A copy that now matches
its source item no longer stops runs. For the rest, such as a copy with an
attachment you added, check what a run would replace, then let it:

```sh
docker compose run --rm vault-mirror check --allow-overwrite
docker compose run --rm vault-mirror run --allow-overwrite
```

Before replacing destination changes, a run backs up their current state.
See [Backups and restoring](backups.md).

### Switching the source and destination

Use the updated vault-mirror binary for runs in both directions. Do not use an
older version after upgrading. Unchanged copies keep their existing markers.
If older markers leave the last copied state unclear, a run may need
`--allow-overwrite`.

To make the destination your main vault, swap the `SOURCE_` and `DEST_`
settings. The first run after the swap stops, because swapped settings are
usually a mistake. If you meant it, run once with `--reverse`:

```sh
docker compose run --rm vault-mirror run --reverse
```

The markers let it update the existing items instead of copying everything
again.

## Safety checks

Before it changes anything, vault-mirror checks the plan. If a check fails, the
run stops and the destination stays as it was.

| The run stops when | Likely cause | To go ahead anyway |
| --- | --- | --- |
| Items in the source are copies of destination items | Source and destination are swapped. | Fix the settings, or `run --reverse` if you [switched them on purpose](#switching-the-source-and-destination). |
| A new copy is linked to an existing destination item by a marker of another pair | A server address, account or scope changed. | Check the settings and follow [server address changes](limitations.md#server-address-changes). |
| Source and destination are the same vault | A settings mistake. | Fix the settings. |
| Copies changed in the destination since a run wrote them | Someone edited, moved, archived or attached a file to a copy. | Make the change in the source, or `run --allow-overwrite` to put the source version back. |
| The run would delete more than half of the copies | The source is empty, the wrong account, or filtered too much. | `run --allow-mass-delete` |
| A filter name matches no folder or collection in the source | A misspelt name. | Fix the name. |

A flag applies to one run. `check` takes the same flags and shows what that run
would do. The daemon takes none. Removed copies go to the server's trash, and
the run's destination backup holds them too.
