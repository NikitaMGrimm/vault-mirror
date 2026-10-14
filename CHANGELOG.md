# Changelog

Notable changes for users. The format follows
[Keep a Changelog](https://keepachangelog.com), and versions follow
[Semantic Versioning](https://semver.org).

## Unreleased

First release.

- Mirrors a personal vault or an organization between Bitwarden and
  Vaultwarden servers, in any direction, incrementally and with verification.
- Mirrors only selected folders or collections, if configured.
- Writes age-encrypted, restorable backups of both vaults, in Bitwarden's
  export format.
- Runs once, or as a daemon on a cron schedule or interval, with optional
  notifications and a health check.
