<p align="center">
  <img src="https://raw.githubusercontent.com/NikitaMGrimm/vault-mirror/main/assets/logo.svg" alt="vault-mirror" width="420">
</p>

<p align="center">
  Keeps a second Bitwarden or Vaultwarden vault in sync with your main one, with encrypted backups of both.
</p>

<p align="center">
  <a href="https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/getting-started.md">Getting started</a> |
  <a href="https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/README.md">Documentation</a> |
  <a href="https://github.com/NikitaMGrimm/vault-mirror/releases">Releases</a> |
  <a href="https://github.com/NikitaMGrimm/vault-mirror/discussions">Discussions</a>
</p>

<p align="center">
  <a href="https://github.com/NikitaMGrimm/vault-mirror/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/NikitaMGrimm/vault-mirror/ci.yml?branch=main&style=flat-square&label=CI"></a>
  <a href="https://github.com/NikitaMGrimm/vault-mirror/releases"><img alt="Release" src="https://img.shields.io/badge/dynamic/xml?url=https%3A%2F%2Fgithub.com%2FNikitaMGrimm%2Fvault-mirror%2Freleases.atom&amp;query=%28%2F%2F%2A%5Blocal-name%28%29%3D%22entry%22%5D%2F%2A%5Blocal-name%28%29%3D%22title%22%5D%29%5B1%5D&amp;label=release&amp;color=blue&amp;style=flat-square"></a>
  <a href="https://crates.io/crates/vault-mirror"><img alt="crates.io" src="https://img.shields.io/crates/v/vault-mirror?style=flat-square"></a>
  <a href="https://github.com/NikitaMGrimm/vault-mirror/blob/main/LICENSE"><img alt="License" src="https://img.shields.io/badge/license-GPL--3.0-blue?style=flat-square"></a>
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/NikitaMGrimm/vault-mirror/assets/demo.gif" alt="vault-mirror previews the changes, backs up both vaults, copies items, and verifies the destination" width="860">
</p>

vault-mirror copies your main vault, the **source**, into a second vault, the
**destination**, and keeps it up to date. Each run:

1. backs up both vaults, encrypted with [age](https://age-encryption.org),
2. creates, updates and deletes items until the destination matches the source,
3. reads the destination back to check the result.

Changes go one way. Items you add to the destination stay, and runs stop
rather than overwrite changes to copies found at the start of a run.
[Changes during a run](docs/limitations.md#changes-during-a-run) can still be lost.

For ongoing organization mirroring, use a Vaultwarden destination. Updates to
existing Bitwarden organization copies are [refused to protect members' private
data](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/limitations.md#server-restrictions).

Use it to keep a working copy on Bitwarden's cloud for when your own server is
down, to move between servers, or to share one folder or collection with
another account.

## Features

- Copies every item type, custom field, passkey, password history, folder,
  collection and attachment, within [server limits](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/limitations.md).
- Saves backups as encrypted Bitwarden exports, which you can decrypt with
  `age` and import into any Bitwarden app.
- Stops before runs that look like a mistake, such as swapped vaults or an empty
  source. See [safety checks](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/how-it-works.md#safety-checks).
- Runs as one binary or a container, without the `bw` CLI or Node.js.

## Quick start

You need Docker, your main account, and a second account for the destination.
vault-mirror stores both accounts' API keys and master passwords; see
[Security](https://github.com/NikitaMGrimm/vault-mirror/blob/main/SECURITY.md) for what that means.

1. In the web vault of each account, open **Settings → Security → Keys** and
   select **View API key**. Keep both pages open.

2. Create a directory and run the setup. It asks for both accounts and writes
   `vault-mirror.env` and `compose.yaml`.

   ```sh
   mkdir vault-mirror && cd vault-mirror
   docker run --rm -it --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work \
     ghcr.io/nikitamgrimm/vault-mirror:0.8 init
   ```

3. Start it:

   ```sh
   docker compose up -d
   ```

vault-mirror now runs on the schedule you chose. [Getting
started](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/getting-started.md) explains each step.

## Installation

The container image runs on linux/amd64 and linux/arm64:

```sh
docker pull ghcr.io/nikitamgrimm/vault-mirror:0.8
```

The [releases page](https://github.com/NikitaMGrimm/vault-mirror/releases) has
binaries for Linux (x86-64, arm64), macOS (Apple silicon, Intel) and Windows
(x86-64, arm64), with checksums and [build
provenance](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations).

With Rust installed, use [cargo-binstall](https://github.com/cargo-bins/cargo-binstall)
or cargo:

```sh
cargo binstall vault-mirror          # the release binary
cargo install vault-mirror --locked  # or build from source
```

The macOS binaries aren't notarized. If you downloaded one in a browser, allow
it to run with:

```sh
xattr -d com.apple.quarantine vault-mirror
```

## Documentation

- [Getting started](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/getting-started.md): a first setup, step by step.
- [How it works](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/how-it-works.md): what a run does, copies, and the safety checks.
- [Configuration](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/configuration.md): every setting, organizations, and
  mirroring part of a vault.
- [Backups and restoring](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/backups.md): the backup directory, and getting
  data back out.
- [Commands and scheduling](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/running.md): every command, scheduling, and systemd.
- [Limitations](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/limitations.md): what isn't copied, and server restrictions.
- [Questions](https://github.com/NikitaMGrimm/vault-mirror/blob/main/docs/questions.md): short answers to common questions.
- [Security](https://github.com/NikitaMGrimm/vault-mirror/blob/main/SECURITY.md): what vault-mirror can access and what it stores.

## Getting help

Ask questions in [Discussions](https://github.com/NikitaMGrimm/vault-mirror/discussions)
and report bugs in [Issues](https://github.com/NikitaMGrimm/vault-mirror/issues).
vault-mirror's logs contain no secrets or vault content, so you can paste them.
Report security problems privately, as [SECURITY.md](https://github.com/NikitaMGrimm/vault-mirror/blob/main/SECURITY.md) describes.

## Contributing

See [CONTRIBUTING.md](https://github.com/NikitaMGrimm/vault-mirror/blob/main/CONTRIBUTING.md).

## License

vault-mirror is licensed under the [GPL-3.0-only](https://github.com/NikitaMGrimm/vault-mirror/blob/main/LICENSE), like Bitwarden's
`bitwarden-crypto` crate that it uses.

This is an independent project. It isn't affiliated with or endorsed by
Bitwarden, Inc. or the Vaultwarden project.
