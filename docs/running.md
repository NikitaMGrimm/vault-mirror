# Commands and scheduling

[← Documentation](README.md)

## Commands

| Command | Does |
| --- | --- |
| `vault-mirror init [--dir DIR]` | Asks for the accounts, checks the logins, writes `vault-mirror.env` and `compose.yaml`, and shows the plan. |
| `vault-mirror` or `vault-mirror run` | One run. Flags: `--dry-run`, `--allow-initial-overwrite`, `--allow-mass-delete`. |
| `vault-mirror daemon` | Runs on `SCHEDULE` or every `INTERVAL`, and stops cleanly on `SIGTERM` between runs. |
| `vault-mirror check` | Logs in, counts items, and prints the plan. Writes nothing. |
| `vault-mirror snapshots [--from dest]` | Lists snapshots. |
| `vault-mirror export --output DIR` | Decrypts a snapshot and its attachments into `DIR`. |
| `vault-mirror restore --to source\|dest` | Writes a snapshot into a vault. |
| `vault-mirror healthcheck [--max-age-hours 26]` | Exits non-zero unless the last run succeeded within the given time. The image uses it as its health check. |

A dry run makes backups and prints the plan, but does not change the
destination.

Every command takes `--env-file PATH` to read settings from a file; see
[Settings file](configuration.md#settings-file).

## Output

At a terminal, each step is one line with a coloured verb, and long steps
show a progress bar. Elsewhere, such as in container logs or under systemd,
each line has a UTC timestamp and a level instead:

```text
2026-10-14T03:00:12Z INFO Created 9 items
2026-10-14T03:00:13Z WARN The destination refused to change the archive state of 1 item: ...
```

Set `NO_COLOR` to turn colours off.

## Scheduling

Either let the container run `daemon`, as in the [quick start](../README.md#quick-start), or have your
scheduler start a single run. Only one run or restore at a time can use a
backup directory: one that starts while another is still going fails without
touching the vaults or the backups. A systemd example:

```ini
# /etc/systemd/system/vault-mirror.service
[Service]
Type=oneshot
ExecStart=/usr/bin/docker run --rm --env-file /etc/vault-mirror.env \
  -v /var/lib/vault-mirror:/data ghcr.io/nikitamgrimm/vault-mirror:0.2

# /etc/systemd/system/vault-mirror.timer
[Timer]
OnCalendar=*-*-* 03:00:00
RandomizedDelaySec=15m
Persistent=true

[Install]
WantedBy=timers.target
```

The container runs as user 65532. A bind-mounted backup directory must be
writable by that user, for example after `chown 65532:65532
/var/lib/vault-mirror`.
