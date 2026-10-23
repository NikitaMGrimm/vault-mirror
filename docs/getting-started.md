# Getting started

[← Documentation](README.md)

This guide sets up vault-mirror with Docker, from nothing to scheduled runs.
Your main vault is the **source**. The second vault, which vault-mirror keeps in
sync with it, is the **destination**.

## What you need

- **Docker.** Without Docker, use a binary from the
  [releases page](https://github.com/NikitaMGrimm/vault-mirror/releases): run
  `vault-mirror init`, then `vault-mirror --env-file vault-mirror.env daemon`.
- **Your source account**, on Bitwarden's cloud, a self-hosted Bitwarden server,
  or Vaultwarden.
- **A second account for the destination**, on the same server or another one.
  vault-mirror only changes the copies it makes there. Everything else in the
  account stays as it is.

A free Bitwarden cloud account can't hold attachments. A run that needs to
copy attachments there stops before changing the vault. To copy items without
their attachments, set [`SKIP_ATTACHMENTS=true`](configuration.md#what-to-mirror).
Backups still include the attachments.

Both servers must show every supported item type, even if your vault holds only
logins. Bitwarden servers can hide newer types and block runs. Check the
[server restrictions](limitations.md#server-restrictions) before setup.

## Step 1: Get the API keys

vault-mirror logs in with each account's personal API key and master password.
For each account:

1. Log in to the web vault.
2. Open **Settings → Security → Keys**.
3. Select **View API key**, and keep the page open for the next step.

## Step 2: Run the setup

Create a directory for vault-mirror's files, and run `init` in it:

```sh
mkdir vault-mirror && cd vault-mirror
docker run --rm -it --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work \
  ghcr.io/nikitamgrimm/vault-mirror:0.9 init
```

`init` asks for:

1. The source's server, `client_id`, `client_secret` and master password. It
   logs in right away to check them and make sure the server shows the whole vault.
2. The same for the destination. Answer no to "Mirror the source into a second
   account?" if you only want backups.
3. How often to run, and in which time zone.
4. A backup passphrase. Let `init` generate one.

It then writes two files and shows what the first run would do:

```text
     Created vault-mirror.env, readable only by you
     Created compose.yaml
note: keep BACKUP_PASSPHRASE outside the backups and vault. After the first run, copy backups/key.age there too. Recovery needs both.
       Would create 152 items, create 6 folders, copy 3 attachments; 0 items unchanged
    Finished check in 4.1s; neither vault was changed
```

Nothing has been changed yet. The "Would" line is the plan.

- `vault-mirror.env` holds every setting, secrets included.
  [Configuration](configuration.md) describes each one.
- `compose.yaml` runs vault-mirror on your schedule and keeps backups in
  `./backups`.

> [!IMPORTANT]
> Keep `BACKUP_PASSPHRASE` somewhere you can reach without either vault.
> Without it, the backups can't be read.

## Step 3: Start it

```sh
docker compose up -d
```

The daemon runs once right away, then at each scheduled time, such as 03:00.
To see what it does:

```sh
docker compose logs -f
```

A first run prints this, with a timestamp and a level before each line:

```text
  Backing up the source vault
   Backed up 3 attachments
       Saved snapshot 20261014T030000Z with 152 items
  Backing up the destination vault
       Saved snapshot 20261014T030000Z with 0 items
     Planned to create 152 items, create 6 folders, copy 3 attachments; 0 items unchanged
     Created 152 items
      Copied 3 attachments
    Verified the destination
    Finished 152 created, 0 updated, 0 deleted, 0 unchanged, 0 folders deleted in 41.3s
```

Your items are now in the destination. Each copy has a hidden field called
`vault-mirror`, its **marker**, which links it to its source item. Leave the
marker in place.

Copy `backups/key.age` to your recovery location now, and try an
[export](backups.md#restore-with-vault-mirror) once.

## Step 4: Check that it keeps working

The container reports itself unhealthy when the last run failed or is more than
26 hours old. To see its state:

```sh
docker compose ps
```

```text
NAME                          STATUS
vault-mirror-vault-mirror-1   Up 2 days (healthy)
```

To see what the last runs did:

```sh
docker compose logs --tail 20
```

To get a message when a run fails, set `NOTIFY_FAILURE_URL` in
`vault-mirror.env`, for example to an [ntfy](https://ntfy.sh) topic. Then apply
the change:

```sh
docker compose up -d --force-recreate vault-mirror
```

## If a run stops

vault-mirror stops before it changes anything when the plan looks like a
mistake, such as swapped vaults, an empty source or copies changed in the
destination. The error names the reason and the flag that allows the run. See
[safety checks](how-it-works.md#safety-checks).

## Next steps

- Mirror only some folders, or an organization: [Configuration](configuration.md)
- Get data back out of a backup: [Backups and restoring](backups.md)
- Run it with systemd instead of Docker Compose: [Commands and scheduling](running.md#systemd)
- What isn't copied: [Limitations](limitations.md)
