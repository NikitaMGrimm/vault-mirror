<p align="center">
  <img src="assets/logo.svg" alt="vault-mirror" width="420">
</p>

<p align="center">
  Keeps a second Bitwarden or Vaultwarden vault in sync with your main one, with encrypted backups of both.
</p>

<p align="center">
  <a href="docs/getting-started.md">Getting started</a> |
  <a href="docs/README.md">Documentation</a> |
  <a href="https://github.com/NikitaMGrimm/vault-mirror/releases">Releases</a> |
  <a href="https://github.com/NikitaMGrimm/vault-mirror/discussions">Discussions</a>
</p>

<p align="center">
  <a href="https://github.com/NikitaMGrimm/vault-mirror/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/NikitaMGrimm/vault-mirror/ci.yml?branch=main&style=flat-square&label=CI"></a>
  <a href="https://github.com/NikitaMGrimm/vault-mirror/releases"><img alt="Release" src="https://img.shields.io/github/v/release/NikitaMGrimm/vault-mirror?sort=semver&style=flat-square"></a>
  <a href="https://crates.io/crates/vault-mirror"><img alt="crates.io" src="https://img.shields.io/crates/v/vault-mirror?style=flat-square"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-GPL--3.0-blue?style=flat-square"></a>
</p>

<p align="center">
  <img src="assets/demo.gif" alt="vault-mirror checks the vaults, copies items, and finds no changes on the second run" width="860">
</p>

vault-mirror copies one Bitwarden or Vaultwarden vault into another and keeps it
up to date. On every run it creates new items, updates the ones you edited, and
removes the ones you deleted, so the second vault is always ready to use. That's
useful when your self-hosted server is down, when you move between servers, or
when you want a separate copy of one folder or collection. Without a second
vault, it keeps encrypted backups of one vault on a schedule.

## Why vault-mirror?

- **A vault you can use, not just a file.** The destination is an ordinary
  account. Open it in any Bitwarden app when you need it.
- **Backups anyone can read.** Each run first backs up both vaults in Bitwarden's
  own export format, encrypted with [age](https://age-encryption.org). You can
  restore them with the standard `age` tool and any Bitwarden client, without
  vault-mirror.
- **Careful with your data.** It shows you the plan before the first run. It stops
  when the vaults look swapped, when the destination holds items it didn't make,
  and when a run would delete every copy at once. After each run it reads the
  destination back to check the result.
- **Small.** One static binary or a small container image. It talks to the
  Bitwarden API directly, without the `bw` CLI or Node.js, and contacts nothing
  but your two servers and, if you set one, a notification URL.

It copies every item type, custom field, passkey, password history, folder,
collection and attachment, within [what the servers allow](docs/limitations.md).

## Quick start

You need a second, empty account for the destination and Docker. In a new
directory, run:

```console
$ docker run --rm -it --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work \
    ghcr.io/nikitamgrimm/vault-mirror:0.2 init
$ docker compose up -d
```

`init` asks for both accounts, checks that it can log in, writes the settings
and a `compose.yaml`, and shows what the first run would do. The
[getting started guide](docs/getting-started.md) walks through every step.

> [!WARNING]
> Use a new, dedicated account as the destination. vault-mirror deletes
> everything in it that doesn't come from the source. Read
> [SECURITY.md](SECURITY.md) to see what it can access and what it stores.

## Installation

The container image runs on linux/amd64 and linux/arm64:

```console
$ docker pull ghcr.io/nikitamgrimm/vault-mirror:0.2
```

The [releases page](https://github.com/NikitaMGrimm/vault-mirror/releases) has
static Linux binaries (x86-64, arm64) and macOS binaries (Apple silicon, Intel),
with checksums and [build provenance](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations).
With Rust installed, you can also use Cargo:

```console
$ cargo binstall vault-mirror     # downloads the release binary
$ cargo install vault-mirror --locked
```

The macOS binaries aren't notarized. After downloading one in a browser, run
`xattr -d com.apple.quarantine vault-mirror`.

## A note on maturity

vault-mirror is new. Its test suite runs against Vaultwarden and Bitwarden's own
server, each as source and as destination, and it has been used with
Bitwarden's cloud. Still, it handles your passwords: keep backups enabled,
and look at `check` or `run --dry-run` before you let it run on a schedule.

## Documentation

- [Getting started](docs/getting-started.md): a first setup, step by step.
- [How it works](docs/how-it-works.md): what a run does, and the safety checks.
- [Configuration](docs/configuration.md): every setting, organizations, and
  mirroring part of a vault.
- [Backups and restoring](docs/backups.md)
- [Commands and scheduling](docs/running.md)
- [Limitations](docs/limitations.md)

## Getting help

Ask questions in [Discussions](https://github.com/NikitaMGrimm/vault-mirror/discussions)
and report bugs in [Issues](https://github.com/NikitaMGrimm/vault-mirror/issues).
vault-mirror's logs never contain secrets or vault content, so you can paste
them. Report security problems privately, as [SECURITY.md](SECURITY.md)
describes.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

vault-mirror is licensed under the [GPL-3.0-only](LICENSE), like Bitwarden's
`bitwarden-crypto` crate that it uses.

This is an independent project. It isn't affiliated with or endorsed by
Bitwarden, Inc. or the Vaultwarden project.
