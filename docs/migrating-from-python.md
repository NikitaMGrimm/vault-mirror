# Migrating from the Python vault-mirror

[← Documentation](README.md)

vault-mirror accepts the Python version's environment variable names.
You can keep the settings file when replacing the image.

The first run recognizes older `__source` markers in existing copies.
vault-mirror updates matching copies and keeps their destination item ids.
vault-mirror rewrites each marker in the new format.

vault-mirror copies attachments again because the Python version did not record attachment state.
Later runs make no changes until the source changes or a copy differs.
The usual [filters and safety checks](how-it-works.md#safety-checks) still apply.

vault-mirror does not read or prune old `*.json.enc` backups.
Decrypt these files with `openssl`, as before.
Delete these files only when you no longer need them.

You can keep the same `BACKUPS_DIR`.
vault-mirror writes new snapshots to `source/snapshots` and `dest/snapshots` inside that directory.

vault-mirror ignores `PUID` and `PGID`.
The image runs as user 65532.
Give user 65532 write access to the backup directory, or run the container as the directory owner.

For example, use `--user 1000:1000` when the owner's user id and group id are both 1000.

> [!WARNING]
> The Python version does not recognize the new markers.
> Switching back to Python recreates every destination item.
