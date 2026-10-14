# Getting started

[← Documentation](README.md)

This guide takes you from nothing to a second vault that stays in sync with your
main one, with encrypted backups of both. It takes about ten minutes. Along the
way you'll see what vault-mirror does before it does it, so nothing about the
first real run should surprise you.

We'll call your main vault the **source** and the second vault the
**destination**. vault-mirror reads the source and makes the destination match
it.

## What you need

- **Your source account.** Bitwarden's cloud (US or EU), a self-hosted Bitwarden
  server, or Vaultwarden all work.
- **A second, empty account for the destination.** This can be on the same
  server or a different one. Create a new account for this. vault-mirror deletes
  anything in the destination that didn't come from the source, so don't point it
  at an account you already use.
- **Docker**, or the `vault-mirror` binary from the
  [releases page](https://github.com/NikitaMGrimm/vault-mirror/releases). This
  guide uses Docker for the long-running part, because then restarts and
  scheduling are handled for you.

> [!NOTE]
> If the destination is a free Bitwarden cloud account, everything except
> attachments gets copied. Bitwarden only accepts attachments from Premium
> accounts. vault-mirror tells you how many it skipped and copies them once the
> account has Premium.

## Step 1: Get an API key for each account

vault-mirror logs in the way the official command-line client does: with a
personal API key plus the master password, which it needs to decrypt the vault.

For each of the two accounts:

1. Log in to the web vault.
2. Open **Settings → Security → Keys**.
3. Select **View API key** and keep the page open. You'll need the `client_id`
   and `client_secret` in the next step.

## Step 2: Run `init`

Make a directory for vault-mirror's files and run the setup there:

```console
$ mkdir vault-mirror && cd vault-mirror
$ docker run --rm -it --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work \
    ghcr.io/nikitamgrimm/vault-mirror:0.2 init
```

If you installed the binary instead, `vault-mirror init` does the same thing.

`init` asks for:

1. **The source account**: which server it's on, its `client_id` and
   `client_secret`, and its master password. It logs in right away, so a typo
   shows up now rather than at 3 a.m.
2. **Whether to mirror into a second account**, and the same details for the
   destination. Answer no if you only want backups of the source.
3. **How often to run**: every day at 03:00, every 6 hours, or every hour. You can
   change this later.
4. **A backup passphrase**. Let `init` generate one unless you have a reason not
   to.

Then it writes two files and shows you what the first run would do:

```text
     Created vault-mirror.env, readable only by you
     Created compose.yaml
note: keep a copy of BACKUP_PASSPHRASE outside the backups and the vault; without it, the backups cannot be read
       Would create 152 items, create 6 folders, copy 3 attachments; 0 items unchanged
    Finished check in 4.1s; neither vault was changed
```

The "Would" line is the plan: nothing has been changed yet.

> [!IMPORTANT]
> Copy `BACKUP_PASSPHRASE` from `vault-mirror.env` into your password manager now.
> The backups are encrypted with it, and without it they can't be read by anyone,
> including you.

### What did `init` write?

`vault-mirror.env` holds every setting as `NAME=value` lines, including your
secrets, so only your user can read it. The
[configuration reference](configuration.md) describes every setting in it.

`compose.yaml` runs the container as a daemon on the schedule you picked, keeps
backups in `./backups`, and restarts the container if it stops.

## Step 3: Try a dry run (optional)

`init` has already shown you the plan. If you want to see the full run, backups
included, without touching the destination, do a dry run:

```console
$ docker compose run --rm vault-mirror run --dry-run
```

A dry run writes the first backups to `./backups` and prints the plan. It doesn't
change the destination.

## Step 4: Start it

```console
$ docker compose up -d
```

If you chose a daily schedule, the first run happens at the next 03:00. With an
interval, it starts right away. You can also run it once now:

```console
$ docker compose run --rm vault-mirror run
```

A first run looks like this:

```text
  Backing up the source vault
       Saved snapshot vault-20261014T030000Z.json.age with 152 items
  Backing up the destination vault
       Saved snapshot vault-20261014T030004Z.json.age with 0 items
     Planned to create 152 items, create 6 folders, copy 3 attachments; 0 items unchanged
     Created 152 items
      Copied 3 attachments
    Verified the destination
    Finished 152 created, 0 updated, 0 deleted, 0 unchanged in 41.3s
```

Every run backs up both vaults first, then compares them, then makes the
changes. Last, it reads the destination back and checks that it now matches the
source. [How it works](how-it-works.md) goes through this in detail.

Open the destination in any Bitwarden app and you'll find your items. Each copy
has one extra hidden field, `__source`, which is how vault-mirror recognizes its
copies on later runs. Leave it there.

## Step 5: Check that it keeps working

The container reports itself unhealthy if the last run failed or is more than 26
hours old:

```console
$ docker compose ps
NAME                          STATUS
vault-mirror-vault-mirror-1   Up 2 days (healthy)
```

For details, read the logs, or `backups/last-run.json`, which records the result
and counts of the last run:

```console
$ docker compose logs --tail 20
```

To get a message after each run, set `NOTIFY_FAILURE_URL` (and
`NOTIFY_SUCCESS_URL`, if you like) in `vault-mirror.env` to an
[ntfy](https://ntfy.sh) topic, a [healthchecks.io](https://healthchecks.io) check,
or any other service that accepts a plain `POST`. Then restart with
`docker compose up -d`.

## If a run stops

vault-mirror stops before it changes anything when something looks wrong. For
example:

- **The destination already has items that vault-mirror didn't create.** You
  probably picked an account you already use. If you really want those items
  replaced, run once with `run --allow-initial-overwrite`.
- **The run would delete every copy it made before.** This is what an empty or
  wrong source looks like. If you did empty the source on purpose, run once with
  `run --allow-mass-delete`.
- **The source has `__source` fields.** The source and destination are probably
  swapped in the settings.

The error message names the flag to use. Flags apply to that one run; the next
scheduled run is checked as usual. [Safety checks](how-it-works.md#safety-checks)
lists them all.

## Where to go next

- Mirror only some folders or collections, or an organization:
  [Configuration](configuration.md).
- Get data back out of a backup, with or without vault-mirror:
  [Backups and restoring](backups.md).
- Run it with systemd instead of Docker:
  [Commands and scheduling](running.md#scheduling).
- What isn't copied, and why: [Limitations](limitations.md).
