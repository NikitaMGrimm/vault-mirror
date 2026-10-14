# Commands and scheduling

[← Documentation](README.md)

## Commands

Use these commands to operate vault-mirror.

| Command | Effect |
| --- | --- |
| `vault-mirror init [--dir DIR]` | `init` asks for accounts and checks logins. It writes `vault-mirror.env` and `compose.yaml`. It shows the plan. |
| `vault-mirror` or `vault-mirror run` | The command completes one run. Flags: `--dry-run`, `--allow-initial-overwrite`, `--allow-mass-delete`. |
| `vault-mirror daemon` | `daemon` runs on `SCHEDULE` or every `INTERVAL`. It completes an active run before stopping on `SIGTERM`. |
| `vault-mirror check` | `check` checks logins, counts items, and prints the plan. It can write local login device state. It does not change vault data. |
| `vault-mirror snapshots [--from dest]` | `snapshots` lists snapshots. |
| `vault-mirror export --output DIR` | `export` decrypts a snapshot and its attachments into `DIR`. |
| `vault-mirror restore --to source\|dest` | `restore` writes a snapshot into the selected configured vault. |
| `vault-mirror healthcheck [--max-age-hours 26]` | `healthcheck` exits with a nonzero status unless the last run succeeded within the specified time. The image uses this command for health checks. |

A dry run makes enabled backups and prints the plan.
A dry run does not change the destination.

Every command accepts `--env-file PATH` to read the settings file.
See [Settings file](configuration.md#settings-file).

## Output

At a terminal, vault-mirror shows each stage on one line with a colored verb.
It shows a progress bar for long stages.

Outside a terminal, each line has a UTC timestamp and a level.
This includes container logs and systemd output:

```text
2026-10-14T03:00:12Z INFO Created 9 items
2026-10-14T03:00:13Z WARN The destination refused to change the archive state of 1 item: ...
```

Set `NO_COLOR` to disable colors.

## Scheduling

Use the container's `daemon` command, as shown in the [quick start](../README.md#quick-start).
Alternatively, configure your scheduler to start individual runs.

One run or restore can use a backup directory at a time.
The tool rejects another run or restore while the directory lock remains active.
The rejected operation does not change vault data or backups.

For systemd, use these service and timer settings:

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

The container runs as user 65532.
This user needs write access to a bind-mounted backup directory.
Give this user access with `chown 65532:65532 /var/lib/vault-mirror`.
