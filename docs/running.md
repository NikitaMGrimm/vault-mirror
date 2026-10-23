# Commands and scheduling

[← Documentation](README.md)

## Commands

| Command | What it does |
| --- | --- |
| `vault-mirror init [--dir DIR]` | Asks for the accounts, checks the logins, writes `vault-mirror.env` and `compose.yaml`, and shows what the first run would do. |
| `vault-mirror run` | Does one run. `--allow-overwrite`, `--allow-mass-delete` and `--reverse` get past a [safety check](how-it-works.md#safety-checks) for this run. |
| `vault-mirror daemon` | Runs once when it starts, then on `SCHEDULE`. On Ctrl-C, or `SIGTERM` on Unix, it lets a run in progress finish, then stops. |
| `vault-mirror check` | Logs in, counts what's in each vault, and shows what a run would do. It never changes either vault. It takes the same flags as `run`. |
| `vault-mirror snapshots` | Lists the backup snapshots. |
| `vault-mirror export --output DIR` | Decrypts a snapshot and its attachments into a new directory. |
| `vault-mirror healthcheck [--max-age-hours 26]` | Exits successfully only if the last run succeeded within that many hours. The container image uses it as its health check. |

Every command takes `--env-file PATH` to read a [settings
file](configuration.md#the-settings-file), and `--help` to describe its options.

`run` and `check` also accept `--allow-organization-metadata-loss` for
[Bitwarden organization updates](configuration.md#organizations). `daemon`
uses the persistent `ALLOW_ORGANIZATION_METADATA_LOSS=true` setting instead.

Outside a terminal, such as in container logs or under systemd, run progress
and run-failure messages start with a UTC timestamp and a level. Snapshot ids,
health-check output, and top-level command errors stay plain. Set `NO_COLOR`
to turn colours off.

## Scheduling

The container's `daemon` command runs once when it starts, then at the times
`SCHEDULE` names. The image's health check gives the first run 26 hours. To
use your own scheduler instead, have it start `vault-mirror run`.

A schedule with no future occurrence stops the daemon before its first run.

Only one `run` or `daemon` run can use a backup directory at a time. While a
run holds the directory's `run.lock`, `export` and `snapshots` stop straight
away. These recovery commands can share the lock with each other and block
runs until they finish. Keep `run.lock` when copying backups for
[read-only recovery](backups.md#restore-with-vault-mirror).

A run refused by the lock sends no notification. Other backup-directory
failures send the configured failure notification.

The generated `compose.yaml` gives a run five minutes to finish when the
container stops. Raise `stop_grace_period` for large vaults.

After editing `vault-mirror.env`, apply the settings with:

```sh
docker compose up -d --force-recreate vault-mirror
```

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
  -v /var/lib/vault-mirror:/data ghcr.io/nikitamgrimm/vault-mirror:0.9 run
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
user.

If `CA_CERT_FILE` is set, add a read-only mount before the image name in
`ExecStart`. Use the absolute path from `/etc/vault-mirror.env` for both paths:

```text
--mount type=bind,source=/absolute/path/ca.pem,target=/absolute/path/ca.pem,readonly
```

For each `*_FILE` setting, bind-mount its host file read-only at the configured
path inside the container. For example, with
`SOURCE_PASSWORD_FILE=/run/secrets/source_password`:

```text
--mount type=bind,source=/host/path/source_password,target=/run/secrets/source_password,readonly
```

The container user, UID `65532`, must be able to read every mounted file.

To run once now and see the result:

```sh
sudo systemctl daemon-reload
sudo systemctl start vault-mirror.service
journalctl -u vault-mirror.service -n 20
```

## Update the container

The generated `compose.yaml` pins the image to the minor version used by
`init`, such as `0.9`. To get patch updates within that version:

```sh
docker compose pull
docker compose up -d
```

To upgrade to a new minor version, read the
[release notes](https://github.com/NikitaMGrimm/vault-mirror/releases), change
the image tag in `compose.yaml`, then run the same commands.
