# Contributing

Thanks for helping. vault-mirror handles password vaults, so changes are
reviewed for safety first and features second.

## Before you start

- Questions, ideas and feature requests go to
  [Discussions](https://github.com/NikitaMGrimm/vault-mirror/discussions).
- Bugs go to [Issues](https://github.com/NikitaMGrimm/vault-mirror/issues).
  Never paste secrets or vault content. vault-mirror's own logs contain
  neither, but check anything else you paste.
- Security problems are reported privately; see [SECURITY.md](SECURITY.md).

Small pull requests are welcome without asking first: typo and documentation
fixes, and bug fixes with a test. Anything substantial needs the maintainer's
approval in an issue or discussion **before** you open a pull request. That
includes new features, new settings, behaviour changes, new dependencies, and
refactors. Pull requests for substantial changes without prior approval are
closed.

The approval step, the one-change rule and the description check below are
for contributions from outside. The maintainer and collaborators skip them,
and the required CI checks still apply to everyone.

The project aims to stay small. A feature that most users would not need, or
that other tools already do well, will probably be declined. Remote storage
for backups is an example: point rclone or restic at the backup directory.

## AI-assisted contributions

AI-assisted contributions are welcome if they are disclosed. End the pull
request description with the model and tool that did the work, in the form the
template shows, or with "No AI assistance." Agent-written issues and comments
must say so at the top. You are responsible for every line you
submit: read it, run it, and be ready to explain it.

Agents should read [AGENTS.md](AGENTS.md).

## Development setup

You need Rust (stable, at least the `rust-version` in `Cargo.toml`) and
Docker. You don't need a Bitwarden account: the sandbox runs throwaway servers
locally.

```sh
sandbox/up.sh                           # two Vaultwarden servers, plus a TLS proxy
cargo test                              # unit and end-to-end tests
cargo run --example sandbox -- --seed   # prints a settings file for two new accounts with sample data
```

The sandbox uses localhost ports 18091 and 18092, leaving Monero's default
ports available.

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

## Design rules

These rules keep the tool predictable for people and for scripts. Follow them
in code, output and documentation. To change a rule, change it here in the
same pull request and give the reason.

### Terminal output

- Show each step as one line: a verb right-aligned in 12 columns, then the
  rest of the sentence. Use `Reporter::status`. Outside a terminal, the same
  line gets a UTC timestamp and a level.
- End every command with one `Finished` line. Give each kind of run one fixed
  shape that never gains or loses parts. Put changing details, such as a
  snapshot name or a warning, on the lines above it.
- Give each colour one meaning. Green: a step that happened. Cyan: progress
  and notes. Yellow: a warning, or a `Finished` line after warnings. Red: an
  error. Never let colour carry the only signal, because `NO_COLOR` removes it.
- Send progress bars to stderr, only at a terminal. Clear a bar before the
  next line.
- Write counts with `text::count`: "1 item", "2 items". Never write
  `item(s)`.
- Put counts, file names and server-side ids in messages. Never put item
  names, passwords or other vault content in them.
- In a warning, say what happened and what the user can do. In an error, say
  what failed and the next step, such as the flag that allows a refused run.

### Safety checks

- Change and delete only copies. An item without a marker belongs to the
  user, and so does the folder or collection it is in. Write to the source
  only in `restore --to source` or an explicitly requested `run --pull`.
- Refuse only a state that is almost never intended: swapped vaults, one
  vault twice, or a run that deletes every earlier copy.
- Name the flag that allows a refused run once.

### Documentation

- Write for someone setting vault-mirror up for the first time: plain
  English, addressed to "you", with short sentences and an example where one
  helps. Say why as well as what.
- Use one term for one meaning: source, destination, run, snapshot, copy (a
  destination item that vault-mirror made), marker (the `vault-mirror` field),
  settings file (`vault-mirror.env`).
- Do not use marketing words or filler, such as "simply", "seamless" or
  "robust".
- Keep the README to an overview, installation, a quick start and links. Put
  details and walkthroughs in `docs/`, starting with
  [Getting started](docs/getting-started.md).
- Keep sample output in the docs the same as what the command prints.

## Pull requests

- Keep each pull request to one change, with a test that fails without it.
- Fill in the template: the problem, the change, and how you verified it. A
  check fails until the description has these sections and the AI disclosure
  line. Show before and after for changes to output or documentation as
  text: a fenced block of terminal output, or a quoted excerpt. Use a
  screenshot or a short recording only when text cannot show the change,
  such as colour, layout or motion. Upload media to the pull request; do not
  commit it.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` with the sandbox running.
- CI skips the build and tests when a change only touches Markdown files,
  `docs/` or `assets/`. Otherwise it runs the tests against Vaultwarden and
  against Bitwarden's own server.
- Update the README or `docs/` when behaviour or configuration changes.
- The maintainer turns on auto-merge after review. A pull request merges as
  soon as the required checks pass. Fix a problem found later in a new pull
  request. Dependabot's patch and minor updates merge on their own the same
  way; major updates wait for the maintainer.
- Title the pull request as a [Conventional Commit](https://www.conventionalcommits.org),
  for example `fix: keep archive state on edit`. Pull requests are squashed,
  and the release notes are generated from these titles: `feat`, `fix` and
  `perf` appear in them, other types do not. A check holds every pull request
  to this, including the maintainer's.

## Releases

[Release-plz](https://release-plz.dev) keeps a release pull request open
with the next version and its changelog. The maintainer rewrites the
changelog there for users, and merges it with the ruleset bypass: pull
requests that release-plz opens don't start CI, and CI runs on `main` after
the merge. Release-plz waits for that commit's push CI run to succeed before
publishing. Manual CI runs and pull request runs do not satisfy the gate.
The gate waits up to two hours. If CI fails and then passes on a rerun,
rerun Release-plz's failed jobs too. Release pull requests must be squash-merged
so the commit being published is the one that passed CI.

Pushed tags and manual releases with a tag use the same push-to-main CI gate
for the commit the tag resolves to. All release builds use that resolved
commit, even when the manual workflow started from another commit. Manual
builds without a tag do not publish and do not wait for this gate.

The verified release publishes the crate to crates.io, tags the
version, and runs the release workflow, which builds static binaries for
x86-64 and arm64 Linux and native binaries for both macOS architectures,
publishes the container image, and attaches build provenance to all of them.
Last, it records the terminal demo in the README from `assets/demo.tape`
against the sandbox, and publishes it to the `assets` branch.

## License

By contributing, you agree that your contribution is licensed under the
[GPL-3.0-only](LICENSE), like the rest of the project.
