English | [Español](README.es.md)

# pxu

pxu keeps the containers (LXC) on your Proxmox server up to date. It shows which containers have updates, updates them one at a time and, if something goes wrong, **rolls back by itself** to the previous state. It can do this every day automatically and notify you through Fluxer or any other Apprise-compatible service.

You use it from a screen in the terminal (the "TUI") and it leaves nothing running in the background.

> Version 0.1, in development. Tested on Proxmox VE 9.2.

## Requirements

- A Proxmox VE server, and `root` access.
- For notifications (optional): an [Apprise API](https://github.com/caronc/apprise-api) server reachable from Proxmox.

## Installation

There are no published downloads yet: the program is built on another machine (with Rust, via [mise](https://mise.jdx.dev)) and copied to Proxmox.

1. On the machine that has the project folder:

```sh
mise install        # only once
mise run build      # leaves the program at target/release/pxu
```

2. Copy `target/release/pxu`, `install.sh` and the `dist/` folder (which `install.sh` looks for next to itself) to Proxmox, however you prefer. For example, with `scp`:

```sh
ssh root@your-proxmox mkdir -p /root/pxu-install
scp target/release/pxu install.sh root@your-proxmox:/root/pxu-install/
scp -r dist root@your-proxmox:/root/pxu-install/
```

3. On Proxmox, as root:

```sh
cd /root/pxu-install && sh install.sh ./pxu --timer
```

Without `--timer`, only the program is installed, without the daily run.

It can be repeated safely and never overwrites your configuration. If the machine doesn't look like Proxmox, the installer refuses; `--force` overrides that.

What gets installed:
- the program at `/usr/local/bin/pxu`
- your configuration at `/etc/pxu/config.toml` (you create it; see below)
- the logs in `/var/log/pxu/`

## First use

On the server, as root:

```sh
pxu
```

Without a configuration file, pxu works in **read-only** mode: you can see your containers and check for updates, but it doesn't update anything. To start updating, mark a container with `a` (see below); pxu creates the file for you, after asking for confirmation.

## The screen (TUI)

Press `?` on any screen to see the help with the keys. Everything is controlled with the keyboard.

### Container list

| Key | What it does |
|---|---|
| `↑` `↓` (or `j` `k`) | move around |
| `/` | search |
| `s` | sort |
| `1` `2` `3` `4` | filter: all / running / stopped / with updates |
| `Enter` | see a container's details |
| `c` | check for updates |
| `C` | refresh the container's indexes and check (asks for confirmation) |
| `u` | update (asks for confirmation) |
| `a` | turn automatic updates on or off for that container |
| `o` | choose columns |
| `r` | reload the list |
| `g` | Config screen |
| `l` | Logs screen |
| `S` | daily run schedule |
| `q` | quit |

### Update (`u`)

A list of the containers to update appears. Use `↑↓` to move, the space bar to remove or add one, and `Enter` to start. While it runs, `x` or `Esc` asks to cancel. Once inside, you can scroll through the progress with `↑↓`, `PgUp`/`PgDn`, and `f` jumps back to the end.

What it does with each container, one at a time: temporary backup → dry run → update → verification. If it fails, it restores the backup by itself. Only containers you checked (`c`) less than an hour ago are updated.

### Config (`g`)

Shows the status of the daily run, the notifications and the log retention. From here:

| Key | What it does |
|---|---|
| `S` | change the daily run schedule |
| `N` | set up notifications (Apprise) |
| `t` | send a test notification |
| `L` | change how many days logs are kept |
| `l` | open Logs |
| `Esc` | go back |

### Logs (`l`)

Lists each run with its date, duration, result and size, from newest to oldest. `↑↓` to choose, `Enter` to open it, `Esc` to go back. Inside a log: `↑↓`, `PgUp`/`PgDn` to scroll, `g`/`G` to jump to the start or the end, `n`/`p` for the next or previous log.

Results you will see: `✅ 5 ok`, `⚠️ 3 ok · 1 warn`, `❌` if something failed, `⛔ STOPPED` if the run was stopped, and `⛔ INCOMPLETE` if it was cut off abruptly (for example, the server shut down mid-run). The log of a run that is still in progress also appears as `⛔ INCOMPLETE` until it finishes.

At first the list says "no run logs yet" until there has been a run.

## Automatic daily run

With `--timer` at install time, pxu checks and updates every day at **04:30** (server time, with a random delay of up to 15 minutes). It updates the containers marked with `auto_update` (key `a`) or with the Proxmox `autoupdate` tag.

To change the time, or turn it off: key `S` in the list or in Config. You choose daily, Monday to Friday, or weekly, see the next run and confirm.

If the container is stopped, it is skipped by default.

## Notifications (Fluxer / Apprise)

Each run ends with **a single notification**: a table with the server first and then each container (result, packages, time and what to do by hand if needed). The title summarizes the run, for example `pxu: update run on server — 3 ok, 1 warn`.

To set it up: in Config (`g`) press `N`, type the address of your Apprise API and the key, and confirm. With `t` you send a test notification. Keys are never shown on screen.

## Log of each run and retention

Each run leaves **one log file** at `/var/log/pxu/run-YYYYMMDD-HHMM.log` (the time in the name is UTC), which is written as it runs. Open it from the Logs screen.

Logs are deleted automatically after **30 days**. To change this, press `L` in Config; `0` means never delete.

## Backups and rollback

- Before updating a container, pxu makes a temporary backup: a snapshot if the storage allows it, otherwise a vzdump copy.
- If the update or the verification fails, pxu restores the container, starts it and deletes the backup.
- If the restore fails or the update runs over its time limit, **the backup is kept**, the run stops, and the log and the notification show the command to restore by hand.
- The backup covers only the container's main disk. Disks mounted from outside (`/mnt/pve/...`) are not included.

## Updating pxu

Build again with `mise run build`, copy the files again and repeat `sh install.sh ./pxu --timer` on Proxmox. Your configuration is kept.

## Uninstall

On the server, as root:

```sh
./install.sh --uninstall           # removes the program and the daily run; keeps configuration and logs
./install.sh --uninstall --purge   # also deletes /etc/pxu, /var/lib/pxu and /var/log/pxu
```

## Common problems

- **"hay que ejecutarlo como root"** (must be run as root) during install: run `install.sh` as root on the server.
- **The TUI won't let you update and says "read-only"**: `/etc/pxu/config.toml` is missing or has errors. Mark a container with `a`, or check the file with `pxu config check`. The file must be owned by root with `0600` permissions.
- **"este equipo no parece Proxmox"** (this machine doesn't look like Proxmox) during install: run it on the Proxmox server, or use `--force`.
- **Notifications don't arrive**: test with `t` in Config. Check the address of your Apprise API.
- **A container doesn't update**: it must be marked with `a` (or with the `autoupdate` tag), and you must have checked it with `c` less than an hour ago.
- **The Logs list is empty**: there hasn't been a run with this version yet. Old logs are still in `/var/log/pxu` but don't appear in the list.
- **A log says `⛔ INCOMPLETE`**: the run was cut off (or is still in progress). The details are in the log itself.

## Development

To build, test or contribute: [development.md](development.md).

## License

MIT
