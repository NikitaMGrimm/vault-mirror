# Commands and scheduling

[← Documentation](README.md)

## Commands

| Command | Does |
| --- | --- |
| `vault-mirror` or `vault-mirror run` | One run. Flags: `--dry-run`, `--allow-initial-overwrite`, `--allow-mass-delete`. |
| `vault-mirror daemon` | Runs on `SCHEDULE` or every `INTERVAL`, and stops cleanly on `SIGTERM` between runs. |
| `vault-mirror check` | Logs in, counts items, and prints the plan. Writes nothing. |
| `vault-mirror snapshots [--from dest]` | Lists snapshots. |
| `vault-mirror export --output DIR` | Decrypts a snapshot and its attachments into `DIR`. |
| `vault-mirror restore --to source\|dest` | Writes a snapshot into a vault. |
| `vault-mirror healthcheck [--max-age-hours 26]` | Exits non-zero unless the last run succeeded within the given time. The image uses it as its health check. |

A dry run makes backups and prints the plan, but does not change the
destination.

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
  -v /var/lib/vault-mirror:/data ghcr.io/nikitamgrimm/vault-mirror:0.1

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
