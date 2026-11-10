English | [Español](README.es.md)

# pxu

pxu keeps the containers (LXC) on your Proxmox server up to date. It shows which containers have updates, updates them one at a time and, if something goes wrong, **rolls back by itself** to the previous state. It can run every day and notify you through Fluxer or any Apprise-compatible service.

You use it from a terminal screen (the "TUI"). It leaves nothing running in the background.

> Version 0.1, in development. Tested on Proxmox VE 9.2.

## Requirements

- A Proxmox VE server, and `root` access.
- For notifications (optional): an [Apprise API](https://github.com/caronc/apprise-api) server reachable from Proxmox.

## Install pxu

Build pxu on another machine (with Rust, via [mise](https://mise.jdx.dev)), then install it on Proxmox. There are no prebuilt releases yet.

1. Build the binary:

   ```sh
   mise install        # only once
   mise run build      # leaves the program at target/release/pxu
   ```

2. Copy `target/release/pxu`, `install.sh` and `dist/` to Proxmox. For example, with `scp`:

   ```sh
   ssh root@your-proxmox mkdir -p /root/pxu-install
   scp target/release/pxu install.sh root@your-proxmox:/root/pxu-install/
   scp -r dist root@your-proxmox:/root/pxu-install/
   ```

3. On Proxmox, as root:

   ```sh
   cd /root/pxu-install && sh install.sh ./pxu --timer
   ```

`--timer` enables the daily run. Without it, only the program is installed.
The installer is safe to rerun and never overwrites your configuration.

Paths: program `/usr/local/bin/pxu`; configuration `/etc/pxu/config.toml` (you create it); logs `/var/log/pxu/`. The installer creates `/etc/pxu`, `/var/lib/pxu` and `/var/log/pxu` with mode `0700`.

## Try pxu in read-only mode

On the server, as root:

```sh
pxu
```

Without a configuration file, pxu is **read-only**: you can see your containers and check for updates, but nothing is updated.
Use `↑↓` to move, `c` to check, `a` to mark a container, `u` to update and `q` to quit. Press `?` for all keys.

## Choose which containers to update

**pxu only updates containers marked with `a` or with a tag listed in `auto_update_tags` (for example `autoupdate`). All other containers are left untouched.**

1. Press `a` on a container. pxu asks for confirmation and creates the configuration file.
2. Press `c` to check for updates.
3. Press `u` to update. pxu asks for confirmation first.

Only containers you checked less than an hour ago are updated.

## Safety and rollback

For each container, one at a time: temporary backup → dry run → update → verification.

- The backup is a snapshot if the storage allows it, otherwise a vzdump copy.
- If the update or the verification fails, pxu restores the container, starts it and deletes the backup.
- If the restore fails or the update runs over its time limit, **the backup is kept**, the run stops, and the log and the notification show the command to restore by hand.
- **The backup covers only the container's main disk.** Disks mounted from outside (`/mnt/pve/...`) are not included.

## Enable automatic updates

With `--timer` at install time, pxu checks and updates every day at **04:30** (server time, with a random delay of up to 15 minutes). It updates the containers marked as above. Stopped containers are skipped by default.

To change the time or turn it off, press `S` in the list or in Config (`g`).

Each run writes one log in `/var/log/pxu/`; open it with `l`. Logs are deleted after 30 days (change with `L` in Config; `0` keeps them forever).

## Set up notifications

Each run ends with **a single notification**: a table with the server first and then each container.

1. In Config (`g`), press `N`.
2. Type the address of your Apprise API and the key, then confirm.
3. Press `t` to send a test notification.

Keys are never shown on screen.

## Update pxu

Build again with `mise run build`, copy the files again and repeat `sh install.sh ./pxu --timer` on Proxmox. Your configuration is kept.

## Uninstall

On the server, as root:

```sh
./install.sh --uninstall           # removes the program and the daily run; keeps configuration and logs
./install.sh --uninstall --purge   # also deletes /etc/pxu, /var/lib/pxu and /var/log/pxu
```

## Common problems

- **"hay que ejecutarlo como root"** (must be run as root) during install: run `install.sh` as root on the server.
- **"este equipo no parece Proxmox"** (this machine doesn't look like Proxmox) during install: run it on the Proxmox server, or use `--force`, which skips that check.
- **The TUI won't let you update and says "read-only"**: `/etc/pxu/config.toml` is missing or has errors. Mark a container with `a`, or check the file with `pxu config check`. The file must be owned by root with `0600` permissions.
- **A container doesn't update**: it must be marked with `a` (or have a tag listed in `auto_update_tags`), and you must have checked it with `c` less than an hour ago.
- **Notifications don't arrive**: test with `t` in Config. Check the address of your Apprise API.
- **The Logs list is empty**: there hasn't been a run with this version yet. Old logs are still in `/var/log/pxu` but don't appear in the list.
- **A log says `⛔ INCOMPLETE`**: the run was cut off (or is still in progress). The details are in the log itself.

## More documentation

- [development.md](development.md): build, test and contribute.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how pxu works, configuration, command line, logs.
- [docs/DESIGN.md](docs/DESIGN.md): the TUI, screen by screen, with all keys.
- [docs/SECURITY-REVIEW.md](docs/SECURITY-REVIEW.md): security review.

## License

MIT
