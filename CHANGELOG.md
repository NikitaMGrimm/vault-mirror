# Changelog

Notable changes for users. The format follows
[Keep a Changelog](https://keepachangelog.com), and versions follow
[Semantic Versioning](https://semver.org).

## [0.2.0](https://github.com/NikitaMGrimm/vault-mirror/compare/v0.1.0...v0.2.0) - 2026-10-14

### Added

- add init, which writes the settings and compose file after checking the logins ([#10](https://github.com/NikitaMGrimm/vault-mirror/pull/10))
- show each step as an aligned verb, with progress for long steps ([#9](https://github.com/NikitaMGrimm/vault-mirror/pull/9))
- [**breaking**] stop only when a run would delete every earlier copy ([#6](https://github.com/NikitaMGrimm/vault-mirror/pull/6))
- *(cli)* colour warnings, errors and the run result in a terminal

## [0.1.0] - 2026-10-14

First release.

- Mirrors a personal vault or an organization between Bitwarden and
  Vaultwarden servers, in any direction, incrementally and with verification.
- Mirrors only selected folders or collections, if configured.
- Writes age-encrypted, restorable backups of both vaults, in Bitwarden's
  export format.
- Runs once, or as a daemon on a cron schedule or interval, with optional
  notifications and a health check.
