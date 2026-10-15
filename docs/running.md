# Commands and scheduling

[← Documentation](README.md)

## Commands

| Command | What it does |
| --- | --- |
| `vault-mirror init [--dir DIR]` | Asks for the accounts, checks the logins, writes `vault-mirror.env` and `compose.yaml`, and shows what the first run would do. |
| `vault-mirror run` | Does one run. Add `--dry-run` to back up and show the plan without changing the destination. `--allow-mass-delete` gets past a [safety check](how-it-works.md#safety-checks) for this run. |
| `vault-mirror daemon` | Keeps running on `SCHEDULE` or every `INTERVAL`. On `SIGTERM` it finishes a run in progress, then stops. |
| `vault-mirror check` | Logs in, counts what's in each vault, and shows what a run would do. It never changes either vault. |
| `vault-mirror snapshots` | Lists the backup snapshots. |
| `vault-mirror export --output DIR` | Decrypts a snapshot and its attachments into a new directory. |
| `vault-mirror restore --to source\|dest` | Writes a snapshot into one of the configured vaults. See [Restoring with vault-mirror](backups.md#restoring-with-vault-mirror). |
| `vault-mirror healthcheck [--max-age-hours 26]` | Exits successfully only if the last run succeeded within that many hours. The container image uses it as its health check. |

Every command takes `--env-file PATH` to read a [settings
file](configuration.md#the-settings-file), and `--help` to describe its options.

## Output

At a terminal, each step is one line: a coloured verb, then what happened. Long
steps show a progress bar. The last line always starts with `Finished` and sums
up the run:

```text
  Backing up the source vault
       Saved snapshot vault-20261014T030000Z.json.age with 151 items
  Backing up the destination vault
     Skipped a new snapshot; the vault is unchanged since the newest one
     Planned to update 2 items, delete 1 item; 149 items unchanged
     Updated 2 items
     Deleted 1 item
    Verified the destination
    Finished 0 created, 2 updated, 1 deleted, 149 unchanged in 6.8s
```

Elsewhere, such as in container logs or under systemd, each line starts with a
UTC timestamp and a level instead:

```text
2026-10-14T03:00:12Z INFO Created 9 items
2026-10-14T03:00:13Z WARN The destination refused to change the archive state of 1 item: ...
```

Set `NO_COLOR` to turn colours off. Messages contain counts, file names and
server-side ids, but never item names, passwords or other vault content.

## Scheduling

The simplest way is the container's `daemon` command, as in
[Getting started](getting-started.md). If you'd rather use your own scheduler,
have it start `vault-mirror run`.

Only one run or restore can use a backup directory at a time. If another one is
already running, the new one stops straight away without changing anything.

### systemd

To run vault-mirror nightly with a systemd timer instead of the daemon, run
these commands in the directory with `vault-mirror.env`:

```sh
sudo install -m 600 vault-mirror.env /etc/vault-mirror.env
sudo install -d -o 65532 -g 65532 /var/lib/vault-mirror

sudo tee /etc/systemd/system/vault-mirror.service > /dev/null <<'EOF'
[Unit]
Description=vault-mirror run
After=docker.service
Requires=docker.service

[Service]
Type=oneshot
ExecStart=/usr/bin/docker run --rm --env-file /etc/vault-mirror.env \
  -v /var/lib/vault-mirror:/data ghcr.io/nikitamgrimm/vault-mirror run
EOF

sudo tee /etc/systemd/system/vault-mirror.timer > /dev/null <<'EOF'
[Timer]
OnCalendar=*-*-* 03:00:00
RandomizedDelaySec=15m
Persistent=true

[Install]
WantedBy=timers.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now vault-mirror.timer
```

The container runs as user 65532, so `/var/lib/vault-mirror` belongs to that
user. To run once now and see the result:

```sh
sudo systemctl start vault-mirror.service
journalctl -u vault-mirror.service -n 20
```
