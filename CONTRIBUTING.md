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
fixes, and bug fixes with a test. Anything else, such as a feature, setting,
behaviour change, dependency or refactor, needs the maintainer's approval in an
issue or discussion **before** you open a pull request.

The project stays small. A feature that most users would not need, or that
other tools already do well, will probably be declined.

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

When the sandbox is not running, the end-to-end tests are skipped. CI requires
them with `VAULT_MIRROR_REQUIRE_SANDBOX=1`. One test checks that a snapshot
imports into the official Bitwarden CLI; set `VAULT_MIRROR_BW` to a `bw`
binary to run it.

### Bitwarden's own server

`sandbox/up.sh --official` also starts Bitwarden's self-hosted server, from a
2.7 GB image, with attachments in the Azurite blob storage emulator and every
account premium. Point the tests at it by URL:

```sh
VAULT_MIRROR_SANDBOX_SOURCE=http://127.0.0.1:18085 cargo test
VAULT_MIRROR_SANDBOX_DEST=http://127.0.0.1:18085 VAULT_MIRROR_SANDBOX_DEST_TLS=https://127.0.0.1:18445 cargo test
```

Changes to how vaults are read or written should pass with Vaultwarden and
with Bitwarden's server on each side. Re-run `sandbox/up.sh --official` after
restarting its containers. Stop everything with:

```sh
docker compose -f sandbox/compose.yaml --profile official down
```

### README demo

On Linux, with VHS v0.12.1 and FFmpeg installed, the sandbox running, and the
release binary and example built, run:

```sh
python3 .github/scripts/demo.py
```

The recorder captures `check` and `run` once against new sandbox accounts. VHS
measures their actual terminal output before rendering. Longer output gets more
rows, up to 24, then scrolls in readable pages. The font stays the same size.
A failed capture or render leaves the previous GIF in place. Releases publish a
fresh recording automatically.

### Performance

`cargo test --release --test scale -- --ignored --nocapture` mirrors a vault
of 2000 items. Set `VAULT_MIRROR_SCALE_ITEMS` to change the size.

## Design rules

Follow these in code, output and documentation. To change a rule, change it
here in the same pull request.

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
  helps.
- Say what a user does and what happens. Leave out the reasons behind design
  decisions, rare edge cases, and anything an error message already says.
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
- Fill in the template: the problem, the change, how you verified it, and the
  AI disclosure line. A check fails until it has them. Show changes to output
  or documentation as text, before and after.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` with the sandbox running.
- Update the README or `docs/` when behaviour or configuration changes.
- Title the pull request as a [Conventional Commit](https://www.conventionalcommits.org),
  for example `fix: keep archive state on edit`. Pull requests are squashed,
  and `feat`, `fix` and `perf` titles become the release notes.

CI runs the tests against Vaultwarden and against Bitwarden's own server, and
skips them when a change only touches Markdown, `docs/` or `assets/`.

## Releases

[Release-plz](https://release-plz.dev) keeps a release pull request open with
the next version and its changelog. The maintainer edits the changelog for
users and squash-merges it. Once CI passes on `main`, release-plz publishes the
crate and tags the version, and the release workflow builds the binaries and
container image with build provenance, then records the README demo.

## License

By contributing, you agree that your contribution is licensed under the
[GPL-3.0-only](LICENSE), like the rest of the project.
