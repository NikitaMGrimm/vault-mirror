# Agent instructions

Read [CONTRIBUTING.md](CONTRIBUTING.md) first. Its rules apply to agents too:
disclose AI assistance, and get approval in an issue or discussion before
opening a pull request for anything substantial.

## Never

- Run against a real vault or a real Bitwarden account. Use the sandbox.
- Put secrets, vault content, or request URLs in logs, errors, notifications,
  or test output. Attachment download URLs carry access tokens. `Reporter`
  messages hold counts and server-side ids only, and `api::error_message`
  reduces server error bodies to their message.
- Weaken a safety rule in `plan::check_safety` to make a test pass.

## Layout

| Path | Role |
| --- | --- |
| `src/api.rs` | HTTP client, login, retries |
| `src/crypto.rs` | The only module that uses `bitwarden-crypto` |
| `src/vault.rs` | Decrypted vault model and the server operations that change it |
| `src/plan.rs` | Pure diff of source and destination, and the safety rules |
| `src/mirror.rs` | Runs a plan, then re-reads and verifies the destination |
| `src/app.rs` | One full run: backups, mirror, status file, notification |
| `src/snapshot.rs`, `src/store.rs`, `src/backup.rs`, `src/restore.rs` | Encrypted backups |
| `src/config.rs` | Environment variables |
| `tests/` | End-to-end tests against the sandbox servers |
| `sandbox/` | Throwaway Vaultwarden and Bitwarden servers in Docker |

## Checks

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
sandbox/up.sh && cargo test
```

Changes to how vaults are read or written must also pass against Bitwarden's
own server, as source and as destination; see "Bitwarden's own server" in
CONTRIBUTING.md. Vaultwarden and Bitwarden differ in ways that matter here:
which item types and collections they show, and what they accept.

## Conventions

- Write code that reads like the code around it. Comments explain why, not
  what.
- Every bug fix comes with a test that fails without it.
- User-facing text is plain and specific: say what happened and what to do.
- Update the README or `docs/` when users would notice the change.
  CHANGELOG.md is generated from commit messages at release time; do not edit
  it.
- Conventional Commits, for example `fix(organization): ...`. Only `feat`,
  `fix` and `perf` reach the release notes, so word them for users.
