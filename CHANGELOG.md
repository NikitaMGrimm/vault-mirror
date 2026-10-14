# Changelog

Notable changes for users. The format follows
[Keep a Changelog](https://keepachangelog.com), and versions follow
[Semantic Versioning](https://semver.org).

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
