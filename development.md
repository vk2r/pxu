English | [Español](development.es.md)

# pxu: development

Technical documentation for pxu: building, testing, installing, code structure, command and configuration reference. To install and use pxu, see the [README](README.md).

A lightweight LXC container updater for Proxmox VE, designed as an alternative to ProxMenux. It is a single Rust binary with a TUI (ratatui) and a CLI. It has no daemon and no dependencies on the host, and uses a few MB of RAM while it runs.

- **Lists** LXCs and VMs by reading `/etc/pve/{lxc,qemu-server}/*.conf` and the state from cgroup and pidfiles, without launching processes.
- **Checks** for updates with a single execution per CT. By default it is **read-only**: it doesn't refresh indexes unless you pass `--refresh`. It supports apt, apk, pacman, dnf and zypper.
- **Updates** CTs one at a time: temporary backup → simulation → upgrade → verification. If anything fails, it **rolls back automatically** to the backup and then deletes it.
- **Notifies** through [Apprise API](https://github.com/caronc/apprise-api) with `curl`. No need to install Python.

> Status: v0.1, in development. Tested on PVE 9.2 (real update and rollback on a Debian 13 CT). Apprise notifications and the rollback of a CT that was stopped are only tested with simulated data.

## Build

The toolchain is managed with [mise](https://mise.jdx.dev) (`mise.toml`: Rust 1.99.1):

```sh
mise install                   # installs the toolchain
mise run build                 # target/release/pxu (~1.3 MB, stripped)
mise run test                  # tests in dev and mock profile, against MockHost, no Proxmox needed
mise run check                 # tests + release + checks that the production binary rejects --mock
mise run lint                  # clippy warnings (not blocking yet)
mise tasks                     # lists all tasks
```

To develop without a server there is a test binary with an in-memory simulated Proxmox (`World` in `src/share/mock/scenario.rs`): there are no folders or data on disk, and the CTs are generic (`ct-101`, `ct-102`…):

```sh
mise run demo                             # TUI with 37 generic simulated guests (`demo` case)
mise run demo -- update-ok update --yes   # another case and command
```

Predefined cases: `demo`, `update-ok`, `update-fail`, `distros`, `cluster`, `failures`, `vms`. Each test builds its own with the builder (`World::new().ct(Ct::new(101).distro(Distro::Alpine).stopped())…`).

`mise run demo` first builds `target/mock/pxu` (`cargo build --profile mock --features mock`). The production binary rejects `--mock` (exits with code 2), and `--release --features mock` doesn't compile.

## Install

There are no published releases: the binary is built with `mise run build` and `install.sh` installs it on the host.

**By hand:** copy `target/release/pxu`, `install.sh` and `dist/` to the host and, as root:

```sh
sh install.sh ./pxu --timer   # --timer is optional; sh install.sh [path-to-binary]
```

`install.sh` installs `/usr/local/bin/pxu`, creates `/etc/pxu`, `/var/lib/pxu` and `/var/log/pxu` (0700), never overwrites `config.toml` and can be repeated. `--force` skips the Proxmox check; `--uninstall` removes the binary and the timer (with `--purge` it also deletes `/etc/pxu`, `/var/lib/pxu` and `/var/log/pxu`).

Without `/etc/pxu/config.toml`, `pxu` works in read-only mode (list and check) and updates nothing. In the TUI, `a` on a CT (table or detail) creates that file with only the CT override, after asking for confirmation, and `pxu` leaves read-only mode.

## Usage

```
pxu                                   TUI
pxu list   [--json] [--all-nodes]
pxu check  [--json] [--refresh] [--notify] [--ct 101,103] [vmid...]
pxu update [--json] [--yes] [--no-notify] [--ct 101,103] [vmid...]
pxu config check                      validates /etc/pxu/config.toml
pxu notify-test                       sends a test notification through Apprise
```

`update` is the only command that writes. It only touches CTs with a recent positive `pxu check` (`require_fresh_check_minutes`). Without `--ct`, it updates the CTs enabled with `auto_update` or with a tag from `auto_update_tags`.

**TUI keys:** `↑↓`/`jk` move · `/` search · `s` sort · `1-4` filter · `Enter` detail · `c` check · `C` refresh and check (asks for confirmation) · `u` update (asks for confirmation) · `x`/`Esc` cancel the run · `?` help · `q` quit.

### Exit codes

| Code | Command | Meaning |
|---|---|---|
| 0 | all | OK / no updates |
| 2 | all | incorrect usage, invalid config or insecure permissions |
| 3 | `update` | another run in progress |
| 5 | `notify-test` | sending failed |
| 10 | `check` | there are updates |
| 12 | `check` | some CT failed to be checked |
| 20 | `update` | some CT failed |
| 30 | `update` | run stopped (2 failures in a row or Ctrl-C) |

## Configuration

`/etc/pxu/config.toml` must be owned by root with mode `0600`; otherwise `pxu` refuses to use it. Minimal example:

```toml
[update]
auto_update_tags = ["autoupdate"]   # CTs with this PVE tag get updated
stopped          = "skip"           # "skip" | "start_and_restore"
reboot           = "never"          # "never" | "if-required"

[backup]
mode    = "auto"     # "auto" (snapshot if the CT supports it, otherwise vzdump) | "snapshot" | "vzdump" | "none"
storage = "local"    # vzdump destination
keep    = 0          # backups kept after a successful update (0 = temporary)

[apprise]
api_url = "http://apprise.local:8000"
api_key = "pxu"
on      = ["updates_found", "update_failed", "run_summary", "check_error"]

[[ct]]               # per-CT override
vmid        = 101
auto_update = true
```

The TUI also writes those overrides: `a` toggles the `auto_update` of the selected CT (modal with "today: … → new state"; `y`/Enter confirms, Esc cancels). It only changes the `auto_update` line of that vmid's `[[ct]]` block, or adds a block at the end; comments and order are preserved. The resulting TOML is validated and written atomically (root, `0600`). An override of `false` wins over the CT's tag.

`pxu update` and the TUI validate the config the same way as `pxu config check` before using it: unknown values (`on_failure = "Stop"`, `apt_mode = "Upgrade"`) or misspelled keys (`on_falure`) are errors. `pxu update` exits with code 2 and shows them; the TUI starts in read-only mode with the reason in the status bar. Warnings don't block.

### Notification and log of each run

Each update run (timer, `pxu update` or the TUI) ends with **a single notification**: a Markdown table (`Upgrades | Result | Packages | GPU driver | Time | Details | Manual`, the host first and then each CT, including failed and skipped ones) under the title `pxu: update run on <host> — N ok, N warn, N failed · <dur>` (+ `STOPPED` if it was stopped, + `· NVIDIA failed`). With more than 25 rows, the ones that are not ok come first, then up to 25 ok and `+N more ok (see log)`. The notification level is success, warning or failure according to the worst result.

- `run_summary` event: always sent when the run ends. `update_failed` event: if it is the only one (without `run_summary`), the same notification is sent only when some CT or the NVIDIA driver failed. Old configs remain valid.
- Each run writes **one log**: `/var/log/pxu/run-YYYYMMDD-HHMM.log` (UTC in the name; `-SS` if one from that minute already exists; `0600`). It is created at the start and grows while it runs: one section per host, NVIDIA driver and CT, with the time of each line, and at the end the summary identical to the notification; it is also written if the run is stopped or cancelled (with the reason). `/var/log/pxu/<vmid>/…`, `host-….log` and `nvidia-….log` are no longer created. An abrupt cut (SIGKILL, host crash) leaves the log partial, which the Logs page lists as `⛔ INCOMPLETE`.
- `[general] log_retention_days = 30` (default): when each run ends, pxu logs older than N days are deleted, including the old formats and empty `<vmid>` folders; `0` = never delete (max 3650). Only regular files with a pxu name inside `/var/log/pxu` are touched (symbolic links are not followed). `logs_per_ct` no longer exists: it is accepted and ignored (obsolete config warning).
- TUI: `l` opens the **Logs** page (date, duration, result and size, from newest to oldest; ↑↓/jk, click to choose, double click or Enter to open). In the viewer: arrows, PgUp/PgDn and wheel to scroll, `g`/`G` start/end, `n`/`p` next/previous log, `q`/Esc returns to the list with the same log selected. `L` in the Config view edits `log_retention_days`.

### Apprise: from the TUI

In the Config view (`g`) the "Notifications (Apprise)" section shows the status of `[apprise]` with secrets hidden: `api_url` with only scheme and host, `api_key` as `•••` and each destination URL as `tgram://…`.

- `N` opens the dialog: API address, mode (key stored in Apprise API **or** direct URLs, comma-separated) and the events that notify (`updates_found`, `update_failed`, `run_summary`, `check_error`). The stored secret is not shown: with the field empty it is kept, and what you type is drawn as dots.
- With Enter it validates the entire resulting config with the same code as `pxu config check`; if it doesn't validate (or no usable transport remains) it writes nothing and says why. Then it asks for confirmation with "today → new" (secrets hidden): `y`/Enter saves, `b` goes back to editing, Esc cancels.
- It saves only `[apprise]`: the rest of `config.toml` and its comments are preserved, the write is atomic (root, `0600`) and, if the file doesn't exist, it creates it. The key and the URLs are mutually exclusive: changing mode removes the other.
- `t` sends a test notification, the same as `pxu notify-test` (shows "sending…", then OK or the error with code 5, without dumping secrets).
- With `--read-only`, editing and testing are rejected: the test goes out over the network, so it counts as an action.

All keys and their default values are in [`docs/ARCHITECTURE.md` §5](docs/ARCHITECTURE.md).

## Backups and rollback

- In `auto` mode, `pxu` asks Proxmox whether the CT supports snapshots. If it doesn't, it makes a `vzdump` in `[backup] storage`.
- If the upgrade or the verification fails, it restores the CT with `pct rollback` or with `pct restore … --force --storage <rootfs storage>`, starts it and deletes the backup.
- If the rollback fails or the upgrade runs over its time limit, the backup **is kept**, the run stops and the command to restore by hand is shown.
- **The backup covers only the rootfs.** Bind mounts (`mpN` to `/mnt/pve/...`) are not included in vzdump or in the rollback.
- `pxu` only deletes the backups in its own registry (`/var/lib/pxu/backups.json`), and each write command is tied to the CT being updated.

## Scheduled run

`install.sh --timer` installs and enables `pxu-update.timer` (daily 04:30, with a random delay of 15 min). The units are in `dist/systemd/` and you can edit them before installing:

```sh
cat dist/systemd/pxu-update.service dist/systemd/pxu-update.timer
```

The `-` in front of `check` makes its code 10 ("there are updates") not stop the unit.

### From the TUI

Without touching `systemctl` by hand: in the Config view (`g`) you see the real state of the timer (installed units, enabled, effective schedule and next run), and `S` (also from the list or the detail) opens the schedule dialog:

- Enable or disable the timer, and choose daily at HH:MM, Monday to Friday at HH:MM, weekly (day + HH:MM) or a free-form `OnCalendar`. Times are the server's.
- Before confirming it validates the schedule with `systemd-analyze calendar` and shows the next run; if it doesn't validate, it writes nothing.
- After confirmation ("today: … → new"; `y`/Enter saves, Esc cancels) it writes the drop-in `/etc/systemd/system/pxu-update.timer.d/schedule.conf` (empty `OnCalendar=` + the new one, 0644, atomic), runs `systemctl daemon-reload` and `enable --now` or `disable --now`.
- If the units are not installed, the dialog installs them (they are the same as in `dist/systemd/`, embedded in the binary).
- It warns if there is no `config.toml` or no CT with auto-update (the update would do nothing) or if the config is invalid (it would fail with code 2). Rejected in forced read-only mode.

`install.sh --uninstall` also deletes that drop-in.

## Structure

```
src/main.rs           entry point: chooses CLI or TUI
src/cli/              commands (list, check, update, config) and argument parsing
src/tui/              terminal interface
src/share/            what both use:
  config.rs, model.rs, state.rs, notify.rs, fmt.rs, labels.rs   configuration, models, state, Apprise, texts
  proxmox/            reading of CT/VM and of Proxmox .conf files
  update/             check, backups and update with rollback
  host/               real and read-only Host
  mock/               in-memory simulated Proxmox (tests / --features mock only)
install.sh            installer for the Proxmox host
dist/systemd/         pxu-update.service and .timer
tests/snapshots/      TUI captures
docs/             ARCHITECTURE.md, DESIGN.md (TUI) and SECURITY-REVIEW.md
```

## Design documents

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): architecture and all configuration keys with their default values (§5).
- [`docs/DESIGN.md`](docs/DESIGN.md): TUI design.
- [`docs/SECURITY-REVIEW.md`](docs/SECURITY-REVIEW.md): security review.

## License

MIT
