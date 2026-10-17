# Commands and scheduling

[← Documentation](README.md)

## Commands

| Command | What it does |
| --- | --- |
| `vault-mirror init [--dir DIR]` | Asks for the accounts, checks the logins, writes `vault-mirror.env` and `compose.yaml`, and shows what the first run would do. |
| `vault-mirror run` | Does one run. `--allow-overwrite`, `--allow-mass-delete` and `--reverse` get past a [safety check](how-it-works.md#safety-checks) for this run. |
| `vault-mirror daemon` | Runs once when it starts, then on `SCHEDULE`. On `SIGTERM` or Ctrl-C it lets a run in progress finish, then stops. |
| `vault-mirror check` | Logs in, counts what's in each vault, and shows what a run would do. It never changes either vault. It takes the same flags as `run`. |
| `vault-mirror snapshots` | Lists the backup snapshots. |
| `vault-mirror export --output DIR` | Decrypts a snapshot and its attachments into a new directory. |
| `vault-mirror healthcheck [--max-age-hours 26]` | Exits successfully only if the last run succeeded within that many hours. The container image uses it as its health check. |

Every command takes `--env-file PATH` to read a [settings
file](configuration.md#the-settings-file), and `--help` to describe its options.

Outside a terminal, such as in container logs or under systemd, each line
starts with a UTC timestamp and a level. Set `NO_COLOR` to turn colours off.

## Scheduling

The container's `daemon` command runs once when it starts, then at the times
`SCHEDULE` names. The image's health check gives the first run 26 hours. To
use your own scheduler instead, have it start `vault-mirror run`.

Only one `run`, `daemon` run, `export` or `snapshots` can use a
backup directory at a time. Another one stops straight away without changing
anything.

The generated `compose.yaml` gives a run five minutes to finish when the
container stops. Raise `stop_grace_period` for large vaults.

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
