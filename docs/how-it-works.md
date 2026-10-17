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
6. **Report.** It writes the result to `last-run.json` and to your notification
   URL, if you set one.

A failed step stops the run. Without a destination, a run only backs up the
source.

## How copies are recognized

Each item vault-mirror creates in the destination is a **copy**. Each copy has a
**marker**: a hidden custom field called `vault-mirror`. It names the two
vaults the copy was made between, its source item, and hashes of the state the
run left the copy in. It holds no secrets. A marker that names another pair of
vaults, such as on an item mirrored on from a third vault, doesn't make an item
a copy here.

Items without a marker aren't copies. vault-mirror never changes or deletes
them, so you can keep your own items in the destination. It never deletes
folders or collections either; delete an empty one yourself.

- If you change a copy's content, attachments, archive state, folder in a
  personal vault or collections in an organization, the next run stops before
  it changes anything and lists the copy's id. Make lasting changes in the
  source. To put the source version back, run once with `--allow-overwrite`;
  that run's destination backup keeps your change.
- In an organization, folders are each member's own. A copy you file in one
  stays there.
- If you delete a copy's marker, the item is no longer a copy, and the next run
  makes a new one next to it.
- If you clone a copy and change the clone, runs warn that it still has the
  marker. Remove its `vault-mirror` field to keep it. An unchanged clone is
  removed as a duplicate.

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

The destination backups hold every version of your changes; see [Backups and
restoring](backups.md).

### Switching the source and destination

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
| Source and destination are the same vault | A settings mistake. | Fix the settings. |
| Copies changed in the destination since a run wrote them | Someone edited, moved, archived or attached a file to a copy. | Make the change in the source, or `run --allow-overwrite` to put the source version back. |
| The run would delete more than half of the copies | The source is empty, the wrong account, or filtered too much. | `run --allow-mass-delete` |
| A filter name matches no folder or collection in the source | A misspelt name. | Fix the name. |

A flag applies to one run. `check` takes the same flags and shows what that run
would do. The daemon takes none. Removed copies go to the server's trash, and
the run's destination backup holds them too.
