# Migrating from the Python version

[← Documentation](README.md)

vault-mirror started as a Python tool. This version reads the same environment
variables, so you can keep your settings file and swap the image. The new image
needs a command, though: give it `daemon` to run on `SCHEDULE` or `INTERVAL`
(in Compose, `command: daemon`), or `run` if something else schedules it.

## What happens on the first run

The first run recognizes the `__source` markers the Python version wrote. It
updates those copies in place, keeping their destination ids, and rewrites each
marker in the new format. The usual [safety checks](how-it-works.md#safety-checks)
apply.

Attachments are copied once more, because the Python version didn't record which
attachments it had copied. After that, runs change nothing until the source does.

> [!WARNING]
> The Python version doesn't understand the new markers. If you switch back to
> it, it recreates every item in the destination.

## Backups

The old `*.json.enc` backups are neither read nor pruned. Decrypt them with
`openssl` as before, and delete them once you no longer need them.

You can keep the same `BACKUPS_DIR`. New snapshots go into `source/snapshots`
and `dest/snapshots` inside it.

## File ownership

`PUID` and `PGID` no longer do anything. The image runs as user 65532, so either
give that user write access to the backup directory or run the container as the
directory's owner, for example with `--user 1000:1000`.
