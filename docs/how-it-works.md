# How it works

[← Documentation](README.md)

vault-mirror makes one vault, the **destination**, match another, the
**source**. Each time it does this is a **run**. If a run fails, the next run
picks up where it left off.

## A run, step by step

```mermaid
flowchart LR
    source[("Source vault")] -- "1. back up" --> backups[("Encrypted backups")]
    dest[("Destination vault")] -- "2. back up" --> backups
    source -- "3. compare" --> mirror["vault-mirror"]
    dest -- "3. compare" --> mirror
    mirror -- "4. change, 5. verify" --> dest
```

1. **Back up the source.** It saves an encrypted **snapshot** of the source
   with its own attachment files. With a local decryption key, it can skip an
   unchanged vault after authenticating the earlier recovery point. Public
   recipient backups save a point every run.
2. **Back up the destination**, the same way. Anything the run removes later is
   in this backup.
3. **Compare.** It works out what to create, update and delete, prints that
   plan, and runs the [safety checks](#safety-checks). If a check fails, the run
   stops here.
4. **Change the destination**, in this order:
   - create folders or collections,
   - create and update items,
   - copy new or changed attachments,
   - move copies whose source item is gone to the trash.
5. **Verify.** It reads the destination back and compares it with the source.
   Each copied attachment is downloaded again and checked by its SHA-256 hash
   when `VERIFY_ATTACHMENTS` is enabled. Replacements are always checked before
   the old files are removed.
6. **Report.** It writes the result to `last-run.json` and sends it to your
   notification URL, if you set one.

A failed backup stops the run before the destination changes. Without backups,
steps 1 and 2 are skipped. Without a destination, the run stops after step 1.

Item removals come last. Replacement attachments are uploaded and checked
before the captured old files are removed. A failed upload keeps those old
files. The backup from step 2 holds the earlier state.

## How copies are recognized

Each item vault-mirror creates in the destination is a **copy**. Each copy has a
**marker**: a hidden custom field called `vault-mirror`. It holds:

- the id of the source item,
- a hash of the item's content when vault-mirror last wrote it,
- a hash of the source item's attachments when they were copied,
- a hash of the copy's attachments as vault-mirror left them.

The marker holds no secrets. It lets the next run update a copy in place instead
of deleting and recreating it.

Items without a marker aren't copies. vault-mirror never changes or deletes them,
or the folders and collections they're in. So you can keep your own items in the
destination, and anything you add there while the source is unreachable stays.

vault-mirror also compares the content itself, so:

- if you edit a copy or its attachments, the next run puts the source version
  back and warns you; with backups on, your edit is in that run's destination
  backup. Archiving a copy is undone too, without a warning, because servers
  that can't archive would otherwise warn on every run;
- if you delete a copy's marker, the item is no longer a copy, and the next run
  makes a new one next to it;
- if you clone a copy and change the clone, runs leave the clone alone and warn
  that it still has the marker. Remove the `vault-mirror` field from it to keep
  it for good; otherwise it is deleted with its source item.

### After an outage

The destination is there for when the source is down. If you add or change
items in it meanwhile, stop the daemon before making those fallback edits:

```sh
docker compose stop vault-mirror
```

Pause any other scheduler too. Otherwise an ordinary run can overwrite your
edits as soon as the source returns. Bring them back with one explicit pull
before restarting scheduling:

```sh
docker compose run --rm vault-mirror run --pull
```

After backing up both vaults, the run copies into the source what changed in
the destination since vault-mirror wrote it:

- a copy you edited replaces its original, with a warning if the original
  changed too. Its folder comes along; changes to its collections don't;
- an item you added in the destination is added to the source, in a folder or
  collections of the same name. So is any other item there that vault-mirror
  didn't make, such as one that was in the destination before the first run.
  With a filter, only selected items and collection memberships are added.

The complete pull is checked before either vault changes. An edited tracked
copy stops it if its original is absent or trashed, its current or proposed
placement is outside the filter, or the source cannot preserve the recovered
data. Required uploads into a personal source need Premium. A warning and skip
is safe only for untouched untracked additions that the mirror leaves alone.

Export the destination before resolving a refused recovery. To preserve a copy
independently, remove its marker deliberately. A run without `--pull` still
lets the source win. After source writes, the pull verifies its intended
content and placement before the forward mirror can use it. If the source drops
a new item's folder or collection membership, a resumed pull retries that
placement. An interrupted pull with pending files must be completed with
`--pull` before ordinary mirroring resumes, even when a later run's filter
excludes the unfinished item.

Then it mirrors as usual. A pull removes no items: an item you deleted in the
destination is still in the source, and the mirror copies it back, so delete it
in the source if you meant it. Recovered source items have their completed
pull markers removed. Excluded items are left alone. With `--dry-run`, the run
only counts what it would copy into the source. After verifying the actual
run's result, resume with
`docker compose up -d`.

Marker cleanup is verified too. An interrupted or failed cleanup leaves
`pull-cleanup-hold.json`, which pauses mirroring until you
[reconcile the recovered items](backups.md#resume-after-interrupted-pull-cleanup).

### Switching the source and destination

To make the destination your main vault for good, swap the `SOURCE_` and `DEST_`
settings. The new source's items are copies, and their markers name the items
they came from, so vault-mirror can update those items in place instead of
making new ones. The first run after the swap stops, because swapped settings
are usually a mistake. If you meant it, run once with `--reverse`:

```sh
docker compose run --rm vault-mirror run --reverse
```

Changes made in the new destination since its last run as the source are
replaced, with a warning, and are in that run's backup. Items you deleted in the
new source stay in the new destination, with a warning, because nothing marks
them as copies. Attachments that still match on both sides aren't copied
again. After that, either direction runs without `--reverse`. If items were
added since the last switch, switching back stops once more, for those items.

## Safety checks

Before it changes anything, vault-mirror checks the plan. If a check fails, the
run stops and the destination stays as it was.

| The run stops when | Likely cause | To go ahead anyway |
| --- | --- | --- |
| Items in the source are copies of destination items | Source and destination are swapped. | Fix the settings, or `run --reverse` if you [switched them on purpose](#switching-the-source-and-destination). |
| Source and destination are the same vault | A settings mistake. | Fix the settings. |
| The run would delete every copy | The source is empty, the wrong account, or filtered to nothing. | `run --allow-mass-delete` |
| The run would delete more than `MAX_DELETE_PERCENT` of the copies | You set a limit. | `run --allow-mass-delete` |

`MAX_DELETE_PERCENT` is off unless you set it. It applies only above 10
deletions, and only once the destination has copies. Extra copies of the same
source item are always deleted and don't count towards it.

A flag applies to one run. Later runs are checked as usual. With Docker Compose:

```sh
docker compose run --rm vault-mirror run --allow-mass-delete
```

> [!WARNING]
> Removed copies go to the server's trash. Its recovery window depends on the
> server. Keep encrypted backups too. The final read and removal are separate
> requests, so a concurrent late edit can still be moved to the trash. It is
> recoverable there, but the operation is not atomic.

### Why there's no default deletion limit

Any fixed limit would be arbitrary: deleting 40% of a vault by mistake isn't
safer than deleting 60%. So vault-mirror only stops on the case that's almost
never intended, losing every copy at once. For smaller mistakes, there's the
backup from the start of each run. To set a limit anyway, use
`MAX_DELETE_PERCENT`.

## Compared with other approaches

| | vault-mirror | Scripts around `bw export` and `bw import` | Copying the server's data directory |
| --- | --- | --- | --- |
| Works with Bitwarden's cloud | yes | yes | no |
| Keeps a usable second vault | yes | yes, rebuilt on every run | no |
| Changes only what changed | yes | no | not applicable |
| Copies attachments | yes | usually not | yes |
| Readable without the server | yes, with `age` | yes, if you keep the exports | no |
| Needs | one binary | the `bw` CLI and Node.js | access to the server |
