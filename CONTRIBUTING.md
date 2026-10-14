# Contributing

Thanks for helping. vault-mirror handles password vaults, so changes are
reviewed for safety first and features second.

## Before you start

- **Questions and ideas** go to
  [Discussions](https://github.com/NikitaMGrimm/vault-mirror-rs/discussions).
- **Bugs** go to [Issues](https://github.com/NikitaMGrimm/vault-mirror-rs/issues).
  Never paste secrets or vault content. vault-mirror's own logs contain
  neither, but check anything else you paste.
- **Security problems** are reported privately; see [SECURITY.md](SECURITY.md).

Small pull requests are welcome without asking first: typo and documentation
fixes, and bug fixes with a test. Anything substantial needs the maintainer's
approval in an issue or discussion **before** you open a pull request. That
includes new features, new settings, behaviour changes, new dependencies, and
refactors. Pull requests for substantial changes without prior approval are
closed.

The project aims to stay small. A feature that most users would not need, or
that other tools already do well, will probably be declined. Remote storage
for backups is an example: point rclone or restic at the backup directory.

## AI-assisted contributions

AI-assisted contributions are welcome if they are disclosed. Say in the pull
request which tool or model you used and for what. Agent-written issues and
comments must say so at the top. You are responsible for every line you
submit: read it, run it, and be ready to explain it.

Agents should read [AGENTS.md](AGENTS.md).

## Development setup

You need Rust (stable, at least the `rust-version` in `Cargo.toml`) and
Docker. You don't need a Bitwarden account: the sandbox runs throwaway servers
locally.

```sh
sandbox/up.sh                   # two Vaultwarden servers, plus a TLS proxy
cargo test                      # unit and end-to-end tests
cargo run -- sandbox --seed     # prints an env file for two new accounts with sample data
```

When the sandbox is not running, the end-to-end tests are skipped. CI requires
them with `VAULT_MIRROR_REQUIRE_SANDBOX=1`. One test checks that a snapshot
imports into the official Bitwarden CLI; set `VAULT_MIRROR_BW` to a `bw`
binary to run it.

### Bitwarden's own server

`sandbox/up.sh --official` also starts Bitwarden's self-hosted server, from a
2.7 GB image. It runs entirely locally and never contacts Bitwarden. Like
Bitwarden's cloud, it stores attachments in Azure blob storage, here the
Azurite emulator. The sandbox makes every account premium, because
attachments need premium. Point the tests at it by URL:

```sh
VAULT_MIRROR_SANDBOX_SOURCE=http://127.0.0.1:18085 cargo test
VAULT_MIRROR_SANDBOX_DEST=http://127.0.0.1:18085 VAULT_MIRROR_SANDBOX_DEST_TLS=https://127.0.0.1:18445 cargo test
```

Changes to how vaults are read or written should pass with Vaultwarden and
with Bitwarden's server on each side. Re-run `sandbox/up.sh --official` after
restarting its containers; it reinstalls the database settings the tests rely
on. Stop everything with:

```sh
docker compose -f sandbox/compose.yaml --profile official down
```

### Performance

`cargo test --release --test scale -- --ignored --nocapture` mirrors a vault
of 2000 items. Set `VAULT_MIRROR_SCALE_ITEMS` to change the size.

## Pull requests

- Keep each pull request to one change, with a test that fails without it.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` with the sandbox running.
- Update the README when behaviour or configuration changes, and add a line
  to the `Unreleased` section of [CHANGELOG.md](CHANGELOG.md) for anything
  users notice.
- Use [Conventional Commits](https://www.conventionalcommits.org) for the
  pull request title, for example `fix: keep archive state on edit`.

## Releases

The maintainer bumps `version` in `Cargo.toml`, renames the `Unreleased`
section of the changelog to the version, and pushes a `vX.Y.Z` tag. The
release workflow builds static binaries for x86-64 and arm64 Linux, publishes
the container image, and uses the changelog section as the release notes.

## License

By contributing, you agree that your contribution is licensed under the
[GPL-3.0-only](LICENSE), like the rest of the project.
