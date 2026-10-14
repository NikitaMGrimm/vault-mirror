<p align="center">
  <img src="assets/logo.svg" alt="vault-mirror" width="420">
</p>

<p align="center">
  Keeps a second Bitwarden or Vaultwarden vault in sync with your main one,<br>
  and keeps encrypted, restorable backups of both.
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
  <img src="assets/demo.gif" alt="vault-mirror checking, mirroring, and finding nothing to change on the second run" width="860">
</p>

vault-mirror copies one Bitwarden or Vaultwarden vault into another and keeps
the copy up to date. Run it on a schedule and the second vault follows the
first: new items are created, edited ones are updated in place, and deleted
ones are removed. It copies every item type, custom field, passkey, password
history entry, folder, and attachment, then reads the destination back to
check the result.

Before changing anything, each run backs up both vaults, encrypted with
[age](https://age-encryption.org) in Bitwarden's export format. You can
recover with the stock `age` tool and any Bitwarden client.

It is a single binary, or a 14 MB container image. It talks to the
Bitwarden API directly, without the `bw` CLI, and contacts nothing but your two
servers and, if you set one, a notification URL.

What people use it for:

- an off-site copy of a self-hosted Vaultwarden in Bitwarden's cloud, for when
  the server is down;
- moving between servers, mirroring until the switch;
- sharing one folder, or some collections, with another account;
- encrypted backups of a single vault, with no destination at all.

> [!IMPORTANT]
> vault-mirror logs in with your master password. Start with `check` and
> `--dry-run`, and read [SECURITY.md](SECURITY.md). It is tested against
> Vaultwarden and Bitwarden's own server, each as source and destination, and
> has been run against Bitwarden's cloud.

## Install

```sh
docker pull ghcr.io/nikitamgrimm/vault-mirror:0.2   # linux/amd64, linux/arm64
cargo binstall vault-mirror                          # or: cargo install vault-mirror --locked
```

Binaries for Linux (static, x86-64 and arm64) and macOS (Apple silicon and
Intel) are on the [releases page](https://github.com/NikitaMGrimm/vault-mirror/releases),
with checksums and [build provenance](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations):
`gh attestation verify FILE --repo NikitaMGrimm/vault-mirror`.

The macOS binaries are not notarized. If you download one with a browser,
clear the quarantine flag with `xattr -d com.apple.quarantine vault-mirror`.

## Quick start

Create a personal API key for each account: in the web vault, go to
**Settings → Security → Keys → View API key**. Use a dedicated, empty account
as the destination, because the tool deletes everything in it that does not
come from the source.

In an empty directory, run the setup. It asks for both accounts, checks that
the logins work, writes `vault-mirror.env` and `compose.yaml`, and shows what
the first run would do:

```sh
vault-mirror init
```

With only Docker installed, run the same command from the image:

```sh
docker run --rm -it --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work \
  ghcr.io/nikitamgrimm/vault-mirror:0.2 init
```

Then start it. It runs on the schedule you chose:

```sh
docker compose up -d
```

Without Docker, run `vault-mirror --env-file vault-mirror.env daemon`
instead. [Configuration](docs/configuration.md) describes every setting and
how to write the files by hand.

## Documentation

- [How it works](docs/how-it-works.md): what a run does, how copies are
  matched, and the [safety checks](docs/how-it-works.md#safety-checks) that
  stop a run before it does damage.
- [Configuration](docs/configuration.md): every setting,
  [organizations](docs/configuration.md#organizations), and
  [mirroring part of a vault](docs/configuration.md#mirroring-part-of-a-vault).
- [Backups and restoring](docs/backups.md)
- [Commands and scheduling](docs/running.md)
- [Limitations](docs/limitations.md): Sends, members, permissions, and
  account settings are not mirrored, and attachments need Premium on
  Bitwarden's servers.

## Contributing

Questions and ideas go to
[Discussions](https://github.com/NikitaMGrimm/vault-mirror/discussions),
bugs to [Issues](https://github.com/NikitaMGrimm/vault-mirror/issues), and
vulnerabilities to a [private report](SECURITY.md). Read
[CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## License

[GPL-3.0-only](LICENSE). vault-mirror uses Bitwarden's `bitwarden-crypto`
crate, which is published under the GPL.

vault-mirror is an independent project, not affiliated with or endorsed by
Bitwarden, Inc. or the Vaultwarden project.
