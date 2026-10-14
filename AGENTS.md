# Agent instructions

vault-mirror copies one Bitwarden or Vaultwarden vault into another and keeps
encrypted backups of both. It holds people's master passwords and decrypts their
vaults, so its priorities are fixed, in this order:

1. **Never lose vault data without a way back.** Every run backs up before it
   changes anything, deletes last, and stops on the states in
   `plan::check_safety` that are almost never intended.
2. **Never leak a secret.** Not in logs, errors, notifications, the status
   file or test output.
3. **Stay predictable.** The same input gives the same plan, and output follows
   the design rules in [CONTRIBUTING.md](CONTRIBUTING.md).
4. Features come after all three.

Keep the tool small. The smallest change that does the job is the right one,
and deleting code is welcome. A new dependency, setting or command needs a reason
a user would recognize.

## Words

Use these terms the way the docs do:

- **source**: the vault that is copied. **destination**: the vault that is made
  to match it.
- **run**: one backup-compare-change-verify cycle (`app::run_once`).
- **copy**: a destination item vault-mirror made. **marker**: the hidden
  `__source` field on every copy that names its source item.
- **snapshot**: one encrypted backup of one vault.
- **scope**: a personal vault or one organization (`vault::Scope`).
- **settings file**: `vault-mirror.env`.

## Ways to hurt yourself

1. **Touching a real vault.** Never run against a real vault or a real Bitwarden
   account. Use the sandbox (`sandbox/up.sh`); it holds throwaway servers.
2. **Leaking through messages.** `Reporter` messages hold counts, file names
   and server-side ids only. Attachment download URLs carry access tokens, so
   request URLs never appear in errors; `api::error_message` reduces server
   error bodies to their message. Item names count as vault content.
3. **Reading "missing" as "empty".** A server response without a list, or an
   organization the account can only partly see, must be an error. Read as an
   empty vault, it makes the next run delete every copy.
4. **Trusting one server.** Vaultwarden and Bitwarden differ in which item types
   and collections they show, what they accept, and how they handle archiving.
   A change to how vaults are read or written must also pass against
   Bitwarden's own server, as source and as destination; see "Bitwarden's own
   server" in CONTRIBUTING.md.
5. **Weakening a safety check to pass a test.** If a test hits
   `plan::check_safety`, the test or the change is wrong, not the check.

## Layout

| Path | Role |
| --- | --- |
| `src/main.rs`, `src/console.rs`, `src/wizard.rs` | The command line: commands, terminal output, `init` |
| `src/app.rs` | One full run: backups, mirror, status file, notification |
| `src/config.rs` | Environment variables |
| `src/api.rs` | HTTP client, login, retries |
| `src/crypto.rs` | The only module that uses `bitwarden-crypto` |
| `src/node.rs` | A decrypted item as a JSON tree that remembers which strings were encrypted |
| `src/vault.rs` | Decrypted vault model and the server operations that change it |
| `src/plan.rs` | Pure diff of source and destination, and the safety rules |
| `src/mirror.rs` | Runs a plan, then re-reads and verifies the destination |
| `src/snapshot.rs`, `src/store.rs`, `src/backup.rs`, `src/restore.rs` | Encrypted backups |
| `src/provision.rs`, `src/fixtures.rs` | Test accounts and sample items for the sandbox |
| `tests/` | End-to-end tests against the sandbox servers, and the command line |
| `sandbox/` | Throwaway Vaultwarden and Bitwarden servers in Docker |

## Checks

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
sandbox/up.sh && VAULT_MIRROR_REQUIRE_SANDBOX=1 cargo test
```

Without the sandbox, the end-to-end tests skip themselves and still pass.
`VAULT_MIRROR_REQUIRE_SANDBOX=1` makes them fail instead, so you know they ran.
If you can't run them, say so in your report.

## Conventions

- Write code that reads like the code around it. Comments explain why, not
  what.
- Every bug fix comes with a test that fails without it.
- Update the README or `docs/` when users would notice the change, and keep
  sample output in the docs the same as what the command prints.
  CHANGELOG.md is generated from commit messages at release time; don't edit it.
- Conventional Commits, for example `fix(organization): ...`. Only `feat`,
  `fix` and `perf` reach the release notes, so word those for users.
- Pull requests follow the template, end with the agent disclosure line it
  shows, and need the maintainer's agreement first if they come from outside;
  see CONTRIBUTING.md. When you work for the maintainer, their instructions
  decide the scope.
