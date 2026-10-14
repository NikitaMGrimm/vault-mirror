# Migrating from the Python vault-mirror

The environment variables are the same, so you can replace the image and keep
your env file.

- **The first run adopts the existing mirror in place.** It recognises the
  older `__source` markers and keeps every destination item and its id. The
  run rewrites each marker in the new form and copies attachments once more,
  because the old tool did not record which copies were current. Later runs
  change nothing until the source changes.
- **Old backups stay where they are.** The existing `*.json.enc` files are
  neither read nor pruned. Decrypt them with `openssl` as before, and delete
  them once you no longer need them.
- **Point `BACKUPS_DIR` at the same directory if you like.** New backups go to
  `source/snapshots` and `dest/snapshots` inside it, next to the old files.
- **`PUID` and `PGID` no longer apply.** The image runs as user 65532. Either
  give that user the backup directory, or start the container as the
  directory's owner, for example with `--user 1000:1000`.
- **Going back is costly.** If you switch back to the Python tool afterwards,
  it does not recognise the new markers and recreates every destination item.
