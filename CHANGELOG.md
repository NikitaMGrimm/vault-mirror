# Changelog

Notable changes for users. The format follows
[Keep a Changelog](https://keepachangelog.com), and versions follow
[Semantic Versioning](https://semver.org).

## [0.8.1](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.8.0...v0.8.1) - 2026-10-18

### Added

- *(mirror)* allow skipping attachments ([#66](https://github.com/NikitaMGrimm/vault-mirror/pull/66))

## [0.8.0](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.7.0...v0.8.0) - 2026-10-18

### Fixed

- *(organization)* preserve member favorites ([#65](https://github.com/NikitaMGrimm/vault-mirror/pull/65))
- preserve copy safety and convergence after reversals ([#64](https://github.com/NikitaMGrimm/vault-mirror/pull/64))
- [**breaking**] protect vault data during one-way mirroring ([#62](https://github.com/NikitaMGrimm/vault-mirror/pull/62))

## [0.7.0](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.6.1...v0.7.0) - 2026-10-17

### Changed

- [**breaking**] Each snapshot is now one directory in the backup directory.
  Backups made by older versions are no longer read; `key.age` still works.
  Snapshots older than a day thin out to one a day and expire after
  `RETENTION_DAYS`, except the newest. An unchanged vault gets no new snapshot.
- [**breaking**] Attachments are always backed up and checked after upload.
- [**breaking**] Old snapshots are always pruned.

### Removed

- [**breaking**] `MAX_DELETE_PERCENT`. A run still stops when it would delete
  every copy or when the source and destination look swapped.
- [**breaking**] `BACKUP_RECIPIENTS`, `--identity`, `run --dry-run`,
  `INTERVAL` (use `SCHEDULE`), `CONCURRENCY`, `BITWARDEN_CLIENT_VERSION`,
  `ENABLE_PRUNING`, `MIN_FILES`, the hold files after a failed pull or a
  restore, and the warning about `http://` server addresses. After a stopped
  `run --pull`, runs ask you to run it again. `init` no longer mounts
  `CA_CERT_FILE` in `compose.yaml`.

### Fixed

- `run --pull` copies back only what changed in the destination, only items the
  filter selects there, and removes only source attachments a backup holds. It
  backs up the destination again afterwards, and refuses to run without a
  destination.
- Restore checks uploaded attachments by content.
- Errors about a damaged snapshot no longer quote its content.
- A run turned away because another one holds the lock no longer overwrites the
  status file or sends a failure notification.
- `init` recognizes the source account under any server address.
- `check` no longer says your destination edits are backed up.

([#55](https://github.com/NikitaMGrimm/vault-mirror/pull/55))

## [0.6.1](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.6.0...v0.6.1) - 2026-10-16

### Fixed

- A failed or interrupted `run --pull` now pauses mirroring until you
  reconcile the vaults. Retrying can no longer copy a partial source write
  over intact destination notes or attachments. This also stops a pull at
  unfinished source items left by an older version.
- Invalid `BACKUP_RECIPIENTS` values no longer appear in errors, the status
  file, or failure notifications. This protects a private age key pasted into
  that setting by mistake.
- Malformed values in known encrypted vault fields now stop the run instead of
  being copied as plaintext.

### Documentation

- Setup now tells you to keep both `BACKUP_PASSPHRASE` and a separate copy of
  `backups/key.age`. Recovery needs both. The guide also recommends testing an
  export before relying on the backups.

### After a failed pull

The recovery hold remains even after a temporary upload failure or a refusal
before any write. Keep scheduling stopped, preserve both vaults and their
backups, then follow the recovery steps before removing
`pull-cleanup-hold.json`. A hold protects runs using the same backup
directory.

## [0.6.0](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.5.3...v0.6.0) - 2026-10-16

### Fixed

- [**breaking**] keep vault data recoverable and protect credentials ([#49](https://github.com/NikitaMGrimm/vault-mirror/pull/49))

## [0.5.3](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.5.2...v0.5.3) - 2026-10-16

### Fixed

- warn about attachments changed on both sides before run --pull replaces them ([#46](https://github.com/NikitaMGrimm/vault-mirror/pull/46))
- keep an archived item archived when run --pull copies its files ([#45](https://github.com/NikitaMGrimm/vault-mirror/pull/45))
- leave out of run --pull what the source server would not show ([#44](https://github.com/NikitaMGrimm/vault-mirror/pull/44))
- check run --pull against the safety rules before it changes the source ([#43](https://github.com/NikitaMGrimm/vault-mirror/pull/43))
- don't delete a copy that was edited during the run ([#42](https://github.com/NikitaMGrimm/vault-mirror/pull/42))

## [0.5.2](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.5.1...v0.5.2) - 2026-10-15

### Fixed

- `run --pull --dry-run` now says what it would pull, for example "Would pull
  1 new item and 1 changed item into the source", and that neither vault was
  changed.

## [0.5.1](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.5.0...v0.5.1) - 2026-10-15

### Added

- `run --pull` brings back what you changed in the destination while the
  source was down. After backing up both vaults, it copies edited copies and
  new items into the source, then mirrors as usual. It deletes nothing, keeps
  edits made only in the source, and leaves no `vault-mirror` fields in the
  source. A pull that stops halfway finishes when run again. See
  [After an outage](docs/how-it-works.md#after-an-outage).
- Windows binaries for x86-64 and Arm, as zip files on the release page and
  through `cargo binstall`.

### Documentation

- [Limitations](docs/limitations.md#accounts) now says which accounts can't be
  used: vault-mirror needs an API key and a master password, so accounts that
  sign in only through SSO with trusted devices or Key Connector don't work.

## [0.5.0](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.4.0...v0.5.0) - 2026-10-15

### Changed

- **Breaking:** the hidden field on each copy is now called `vault-mirror`
  instead of `__source`, and records the source and the copy's attachments
  separately. Copies made by earlier versions aren't recognized, so empty the
  destination before the first run, or each item is copied a second time.
- Adding, removing or replacing an attachment on a copy now counts as a
  change in the destination and gets the same warning as an edit.
- A custom field you name `vault-mirror` yourself is copied like any other
  field.

### Added

- Switching the source and destination. Swap the `SOURCE_` and `DEST_`
  settings and run once with `run --reverse`: the existing items are updated
  in place instead of copied again, and attachments that still match aren't
  copied. After that, either direction runs without the flag, and no item
  ever gets more than one marker. Without `--reverse`, the first run after a
  swap stops, because swapped settings are usually a mistake. See
  [Switching the source and destination](docs/how-it-works.md#switching-the-source-and-destination).
- Clones of a copy. If you clone a copy and change the clone, runs leave the
  clone alone and warn that it still carries the `vault-mirror` field.

### Fixed

- A run that stops while removing a copy's attachments no longer makes the
  next run report a change in the destination.
- A run stops instead of overwriting a copy that someone edits while the run
  copies its attachments.

## [0.4.0](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.3.0...v0.4.0) - 2026-10-15

### Changed

- **Breaking:** vault-mirror changes and deletes only the copies it made in
  the destination. Items without a `__source` marker stay as they are, and
  so do the folders and collections that hold them, so you can keep your own
  items there. `--allow-initial-overwrite` is gone, because a first run no
  longer deletes anything. If the destination holds older copies of the
  source, such as from another tool, delete them to avoid duplicates; the
  first run warns about them.
- `MAX_DELETE_PERCENT` counts copies only.
- A run warns before it replaces copies that were changed in the
  destination.
- Backups stay small. Past the last 24 hours, only the newest snapshot of
  each UTC day is kept, for each vault on its own. With the defaults and
  hourly runs, that is at most about 54 snapshots per vault instead of about
  720. Pruning only touches files vault-mirror wrote.

### Added

- A warning when `SOURCE_SERVER` or `DEST_SERVER` uses `http://` for a host
  outside your network, where the API key would cross the internet
  unencrypted.
- [Questions](docs/questions.md): short answers to what people ask first.

### Fixed

- Error messages no longer contain folder or collection names.

## [0.3.0](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.2.0...v0.3.0) - 2026-10-14

### Changed

- **Breaking:** vault-mirror needs a command. Without one it prints its
  help instead of doing a run. Use `vault-mirror run` for one run, or
  `daemon` to run on a schedule. Compose files written by `init` already
  set `command: daemon`.
- **Breaking:** compatibility with the Python version is gone.
  `ENCRYPTION_PASSWORD` and `CRON_SCHEDULE` are no longer read; use
  `BACKUP_PASSPHRASE` and `SCHEDULE`. Copies made by the Python version are
  no longer recognized, so the first run stops at the safety check for a
  destination without copies. Run it once with `--allow-initial-overwrite`
  to replace them.
- Every command ends with one `Finished` line in a fixed shape, so scripts
  can match it.
- Compose files written by `init` give a run in progress five minutes to
  finish when the container stops.

### Fixed

- vault-mirror no longer crashes when any environment variable holds text
  that isn't UTF-8.
- `healthcheck` reads `BACKUPS_DIR` from the file given with `--env-file`.

## [0.2.0](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.1.0...v0.2.0) - 2026-10-14

### Added

- `vault-mirror init` asks for the two accounts, checks that both logins
  work, and writes `vault-mirror.env` and `compose.yaml`. The binary reads
  the same file with `--env-file`.
- At a terminal, each step is one line with an aligned, coloured verb, and
  long steps show a progress bar. Outside a terminal, log lines start with
  the same verb.
- Release binaries for macOS, on Apple silicon and Intel.

### Changed

- **Breaking:** the default limit of deleting half the destination is gone.
  A run now stops only when it would delete every copy it made before, which
  is what an empty or wrong source looks like. `MAX_DELETE_PERCENT` still
  sets a limit, but only when you set it. `--allow-mass-delete` overrides
  both.
- Messages use proper plurals, and several are reworded. Anything that
  matches log lines by their text may need updating.

### Fixed

- The compose setup in the README could not write its backups on a fresh
  machine, because Docker created `./backups` as root. The compose file
  from `init` runs the container as the owner of that directory, and the
  hand-written example shows how.

## [0.1.0] - 2026-10-14

First release.

- Mirrors a personal vault or an organization between Bitwarden and
  Vaultwarden servers, in any direction, incrementally and with verification.
- Mirrors only selected folders or collections, if configured.
- Writes age-encrypted, restorable backups of both vaults, in Bitwarden's
  export format.
- Runs once, or as a daemon on a cron schedule or interval, with optional
  notifications and a health check.
