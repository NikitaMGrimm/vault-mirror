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
  <a href="https://github.com/NikitaMGrimm/vault-mirror/releases"><img alt="Release" src="https://img.shields.io/badge/dynamic/xml?url=https%3A%2F%2Fgithub.com%2FNikitaMGrimm%2Fvault-mirror%2Freleases.atom&amp;query=%28%2F%2F%2A%5Blocal-name%28%29%3D%22entry%22%5D%2F%2A%5Blocal-name%28%29%3D%22title%22%5D%29%5B1%5D&amp;label=release&amp;color=blue&amp;style=flat-square"></a>
  <a href="https://crates.io/crates/vault-mirror"><img alt="crates.io" src="https://img.shields.io/crates/v/vault-mirror?style=flat-square"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-GPL--3.0-blue?style=flat-square"></a>
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/NikitaMGrimm/vault-mirror/assets/demo.gif" alt="vault-mirror previews the changes, backs up both vaults, copies items, and verifies the destination" width="860">
</p>

vault-mirror copies your main vault, the **source**, into a second vault, the
**destination**, and keeps it up to date. Each run:

1. backs up both vaults, encrypted with [age](https://age-encryption.org),
2. creates, updates and deletes items until the destination matches the source,
3. reads the destination back to check the result.

Use it to keep a working copy on Bitwarden's cloud for when your own server is
down, to move between servers, or to share one folder or collection with
another account.

## Features

- Copies every item type, custom field, passkey, password history, folder,
  collection and attachment, within [server limits](docs/limitations.md).
- Saves backups in Bitwarden's export format, readable with `age` and any
  Bitwarden app.
- Stops before runs that look like a mistake, such as swapped vaults or an empty
  source. See [safety checks](docs/how-it-works.md#safety-checks).
- Runs as one binary or a container, without the `bw` CLI or Node.js.

## Quick start

You need Docker, your main account, and a second account for the destination.
vault-mirror only changes the copies it makes there.

1. In the web vault of each account, open **Settings → Security → Keys** and
   select **View API key**. Keep both pages open.

2. Create a directory and run the setup. It asks for both accounts and writes
   `vault-mirror.env` and `compose.yaml`.

   ```sh
   mkdir vault-mirror && cd vault-mirror
   docker run --rm -it --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work \
     ghcr.io/nikitamgrimm/vault-mirror init
   ```

3. Start it:

   ```sh
   docker compose up -d
   ```

vault-mirror now runs on the schedule you chose. [Getting
started](docs/getting-started.md) explains each step.

## Installation

The container image runs on linux/amd64 and linux/arm64:

```sh
docker pull ghcr.io/nikitamgrimm/vault-mirror
```

The [releases page](https://github.com/NikitaMGrimm/vault-mirror/releases) has
binaries for Linux (x86-64, arm64), macOS (Apple silicon, Intel) and Windows
(x86-64, arm64), with checksums and [build
provenance](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations).

With Rust installed, download the release binary with
[cargo-binstall](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall vault-mirror
```

Or build it from source:

```sh
cargo install vault-mirror --locked
```

The macOS binaries aren't notarized. If you downloaded one in a browser, allow
it to run with:

```sh
xattr -d com.apple.quarantine vault-mirror
```

## Documentation

- [Getting started](docs/getting-started.md): a first setup, step by step.
- [How it works](docs/how-it-works.md): what a run does, and the safety checks.
- [Configuration](docs/configuration.md): every setting.
- [Backups and restoring](docs/backups.md)
- [Commands and scheduling](docs/running.md)
- [Limitations](docs/limitations.md)
- [Questions](docs/questions.md)
- [Security](SECURITY.md): what vault-mirror can access and what it stores.

## Getting help

Ask questions in [Discussions](https://github.com/NikitaMGrimm/vault-mirror/discussions)
and report bugs in [Issues](https://github.com/NikitaMGrimm/vault-mirror/issues).
vault-mirror's logs contain no secrets or vault content, so you can paste them.
Report security problems privately, as [SECURITY.md](SECURITY.md) describes.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

vault-mirror is licensed under the [GPL-3.0-only](LICENSE), like Bitwarden's
`bitwarden-crypto` crate that it uses.

This is an independent project. It isn't affiliated with or endorsed by
Bitwarden, Inc. or the Vaultwarden project.
