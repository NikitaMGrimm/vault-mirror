<p align="center">
  <img src="assets/logo.svg" alt="vault-mirror" width="420">
</p>

<p align="center">
  Keeps a second Bitwarden or Vaultwarden vault in sync with your main one, with encrypted backups of both.
</p>

<p align="center">
  <a href="https://github.com/NikitaMGrimm/vault-mirror/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/NikitaMGrimm/vault-mirror/ci.yml?branch=main&style=flat-square&label=CI"></a>
  <a href="https://github.com/NikitaMGrimm/vault-mirror/releases"><img alt="Release" src="https://img.shields.io/github/v/release/NikitaMGrimm/vault-mirror?sort=semver&style=flat-square"></a>
  <a href="https://crates.io/crates/vault-mirror"><img alt="crates.io" src="https://img.shields.io/crates/v/vault-mirror?style=flat-square"></a>
  <a href="https://github.com/NikitaMGrimm/vault-mirror/pkgs/container/vault-mirror"><img alt="Container image" src="https://img.shields.io/badge/image-ghcr.io-blue?style=flat-square"></a>
  <a href="Cargo.toml"><img alt="Minimum Rust version" src="https://img.shields.io/crates/msrv/vault-mirror?style=flat-square"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-GPL--3.0-blue?style=flat-square"></a>
</p>

<p align="center">
  <img src="assets/demo.gif" alt="vault-mirror checks the vaults, copies items, and finds no changes on the second run" width="860">
</p>

vault-mirror copies one Bitwarden or Vaultwarden vault into another and keeps the destination up to date.
On a schedule, it creates new items, updates edited items in place, and removes deleted items.

It copies every item type, custom field, passkey, password history entry, folder, and attachment, subject to [server limitations](docs/limitations.md).
It reads the destination again to check the result.

With backups enabled, each run backs up both vaults before changing the destination.
It encrypts backups with [age](https://age-encryption.org) in Bitwarden's export format.
You can recover data with the standard `age` tool and any Bitwarden client.

The tool runs as one binary or a 14 MB container image.
It uses the Bitwarden API directly, without the `bw` CLI.
It contacts only the two configured servers and, if you set one, a notification URL.

Common uses include:

- Keep a destination in Bitwarden's cloud when your self-hosted Vaultwarden server is unavailable.
- Copy items between servers before changing servers.
- Copy one folder or selected collections to another account.
- Keep encrypted backups of one source without a destination.

Tests cover Vaultwarden and Bitwarden's own server, each as source and destination.
The tool has also run against Bitwarden's cloud.
Start with `check` and `--dry-run` before enabling scheduled runs.

## Install

```sh
docker pull ghcr.io/nikitamgrimm/vault-mirror:0.2   # linux/amd64, linux/arm64
cargo binstall vault-mirror                          # or: cargo install vault-mirror --locked
```

The [releases page](https://github.com/NikitaMGrimm/vault-mirror/releases) has binaries for these systems:

| System | Architectures |
| --- | --- |
| Linux, static binaries | x86-64, arm64 |
| macOS | Apple silicon, Intel |

Releases include checksums and [build provenance](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations).
Verify a downloaded file with `gh attestation verify FILE --repo NikitaMGrimm/vault-mirror`.

The macOS binaries have no notarization.
After a browser download, remove the quarantine flag with `xattr -d com.apple.quarantine vault-mirror`.

## Quick start

> [!WARNING]
> vault-mirror logs in with your master password.
> Use a dedicated, empty destination account.
> It permanently deletes destination items that do not come from the source.

Read [SECURITY.md](SECURITY.md) before setup.

1. Open **Settings → Security → Keys → View API key** in each account's web vault.
2. Create a personal API key for each account.

In an empty directory, run the setup command:

```sh
vault-mirror init
```

`init` asks for both accounts and checks their logins.
It writes `vault-mirror.env` and `compose.yaml`.
It also shows the first run's plan.

With only Docker installed, run this command instead:

```sh
docker run --rm -it --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work \
  ghcr.io/nikitamgrimm/vault-mirror:0.2 init
```

Start the container with this command:

```sh
docker compose up -d
```

The container runs on the schedule you chose during setup.
Without Docker, start scheduled runs with `vault-mirror --env-file vault-mirror.env daemon`.
[Configuration](docs/configuration.md) describes every setting and manual setup.

## Documentation

- [How it works](docs/how-it-works.md): run stages, matching copies, and [safety checks](docs/how-it-works.md#safety-checks) that prevent damage.
- [Configuration](docs/configuration.md): every setting, [organizations](docs/configuration.md#organizations), and [mirroring part of a vault](docs/configuration.md#mirroring-part-of-a-vault).
- [Backups and restoring](docs/backups.md)
- [Commands and scheduling](docs/running.md)
- [Limitations](docs/limitations.md): vault-mirror does not copy Sends, members, permissions, or account settings. Bitwarden attachments require Premium.

## Contributing

Post questions and ideas in [Discussions](https://github.com/NikitaMGrimm/vault-mirror/discussions).
Report bugs in [Issues](https://github.com/NikitaMGrimm/vault-mirror/issues).
Report vulnerabilities through a [private report](SECURITY.md).

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## License

vault-mirror uses the [GPL-3.0-only](LICENSE) license.
It uses Bitwarden's `bitwarden-crypto` crate, which also uses the GPL.

This is an independent project.
Bitwarden, Inc. and the Vaultwarden project do not affiliate with or endorse it.
