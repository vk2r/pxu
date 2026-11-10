English | [Español](ARCHITECTURE.es.md)

# ARCHITECTURE — `pxu`

Technical reference of how `pxu` works today. The TUI is described in [DESIGN.md](DESIGN.md); the security analysis, in [SECURITY-REVIEW.md](SECURITY-REVIEW.md). Installation and usage: [README.md](../README.md); building and testing: [development.md](../development.md).

Known limitation: the logic is tested against a simulated server (`MockHost`, §8). Behavior with every storage type and Proxmox version has not been verified on a real server.

---

## 0. Decisions

| Topic | Decision | Where to see it |
|---|---|---|
| Language / TUI | Rust + ratatui (crossterm); `libc` for locks, priorities and signals | `Cargo.toml` |
| Process | **No daemon.** It runs, does its work and exits. Automation is a systemd timer | `dist/systemd/` |
| Concurrency | **No async runtime.** The TUI spawns one thread per job (check, update); the CLI works on the main thread | `src/tui/mod.rs`, `src/cli/mod.rs` |
| Listing | Reads `/etc/pve/**/*.conf` and `/sys/fs/cgroup`; without launching processes | §1 |
| Execution in CT | **A single execution per CT per check** (embedded `probe.sh` script) with `lxc-attach` | §2.1 |
| Check | Read-only by default: it doesn't refresh the indexes | §2.3 |
| Update | Sequential; global lock + per-CT lock; prior temporary backup (snapshot or vzdump) with automatic rollback; post-update verification | §3 |
| Proxmox host and NVIDIA | Reviewed on every `check`; only updated on explicit order or with an opt-in in config | §3.6, §3.7 |
| Notifications | Apprise API via `curl` (fallback: `apprise` CLI); a single notification per run | §4 |
| Config | `/etc/pxu/config.toml`, 0600 root | §5 |
| Testing | `Host` trait with `RealHost` and `MockHost` (in-memory simulated server) | §8 |

Nothing `pxu` runs goes through a host shell: every `Command` carries a fixed argv. The only `sh -c` runs **inside** the CT with the binary's constant script.

---

## 1. Listing LXCs and VMs

`pxu list` and the TUI read files; `src/share/proxmox/guests.rs` doesn't launch processes.

| Data | Source |
|---|---|
| Config of each guest | `/etc/pve/nodes/<node>/{lxc,qemu-server}/<vmid>.conf` |
| Local or remote guest | the node's directory; those from other nodes are listed as `remote` and not touched |
| LXC running | `/sys/fs/cgroup/lxc/<vmid>/cgroup.events` with `populated 1` (on PVE 9 `cgroup.procs` comes empty) |
| VM running | `/var/run/qemu-server/<vmid>.pid` + existing `/proc/<pid>` |
| Live RAM and CPU (LXC) | `memory.current` and `cpu.stat` of the cgroup; the TUI samples CPU every 3 s (the first one, at 1 s) |

`pxu list --all-nodes` includes the guests of other nodes; without the option only those of the local node appear. Besides the guests, the inventory includes a row for the **Proxmox host** (vmid `0`, §3.6).

### `.conf` parser (`proxmox/conf.rs`)

- The leading `#…` lines are the description.
- `key: value` up to the first `[name]` section; the following sections are snapshots or `[pending]`.
- `lxc.*` keys can repeat and are stored in order.
- A `lock:` key in the config leaves the guest locked: it is never updated. `template: 1` marks it as a template: it is listed, never updated.

---

## 2. Update check

### 2.1 One execution per CT

`pxu` runs **one** POSIX `sh` script (busybox-compatible) embedded with `include_str!` from `src/share/proxmox/probe.sh`, passed through argv; nothing is written into the CT. Inside the CT it runs like this:

```
lxc-attach -n <vmid> --clear-env --set-var PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin -- sh -c "$PROBE" pxu-probe <refresh:0|1> <min_age_s>
```

(`host/real.rs::lxc_attach_argv`; `pct exec` is not used.) The script prints a line protocol with the `@@` prefix:

```
@@v 1
@@os <ID> <VERSION_ID> "<ID_LIKE>"
@@pm apt|apk|pacman|dnf|zypper|none     # detected by binary present
@@cache_age <seconds>                   # -1 if it can't be known
@@refresh skipped|fresh|ok|failed:<rc>|unavailable|unavailable-checkupdates-used
@@pkg <name> <current> <new> <security 0|1>
@@held <name>
@@reboot_required 0|1
@@end <rc>
```

Always with `LC_ALL=C`. Without `@@end` the result is an error (timeout, stopped CT, missing `sh`). The CT output is sanitized with `fmt::clean` before use (SECURITY-REVIEW, H1).

### 2.2 By package manager

| Manager | Check (read-only) | Refresh (opt-in) | Upgrade (`update_argv`) |
|---|---|---|---|
| **apt** | `apt-get -s -o Debug::NoLocking=1 dist-upgrade` (`Inst` lines; the `*Security*` origin marks security) + `apt-mark showhold` | `apt-get update -qq` | `env DEBIAN_FRONTEND=noninteractive apt-get -y -o Dpkg::Options::=--force-confold -o Dpkg::Options::=--force-confdef dist-upgrade` (`upgrade` with `[update] apt_mode = "upgrade"`) |
| **apk** | `apk --no-network list -u` | `apk update` | `apk upgrade --no-cache` |
| **pacman** | `pacman -Qu` (local database) | not refreshed: reports `unavailable` and no `pacman -Sy` is done in the check | `pacman -Syu --noconfirm` |
| **dnf** (or `yum`) | `dnf -q -C check-update` (code 100 = there are updates) | `dnf -q makecache` | `dnf -y upgrade` |
| **zypper** | `zypper --no-refresh -n -q list-updates` | `zypper -n -q refresh` | `zypper -n update` |
| other | `@@pm none` → `unsupported` state: listed, not touched | — | — |

The upgrades are in `update/mod.rs::upgrade_argv`, which is the single source for the engine, the mock and the allowlist.

### 2.3 Modes

- **Read-only (default):** `[check] refresh = false`. Shows the state of the index the CT already has, with its age.
- **Refresh:** `[check] refresh = true`, `[[ct]] refresh = true`, `pxu check --refresh` or the TUI's refresh key. With `refresh_min_age_hours` (24) it only refreshes if the index is older.
- An index older than `[check] index_stale_days` (7) is marked `stale`.
- Stopped CTs, VMs and remote guests are not checked (`check::targets`).

### 2.4 Execution and persistence

- Sequential, one CT after another; the per-CT timeout is `[check] timeout_s` (60 s; triple with refresh).
- The result is saved in `/var/lib/pxu/state.json` (atomic write): date, manager, packages, number of security updates, index age and a **fingerprint** (FNV-1a of the sorted list `name=new_version`, `state::fingerprint`).
- The fingerprint avoids repeating notifications: §4.3. A check is "recent" for `update` if it is less than `[update] require_fresh_check_minutes` (60) old.
- `pxu check` also reviews the **Proxmox host** (`proxmox/hostcheck.rs`, read-only): pending packages, repositories, pending reboot and NVIDIA GPU/driver. The only commands it uses are `apt-get update` (only with refresh), `apt list --upgradable`, `apt-get -s … full-upgrade`, `lspci -nn` and `dkms status`.

---

## 3. Update (`pxu update`)

Code: `src/share/update/mod.rs`. It is the only command that writes.

### 3.1 Selection

`update::select` takes, without arguments, the LXCs with `auto_update` (by `[[ct]]` or by tag, §5); with vmids or `--ct` it takes those. Each candidate can end up **skipped** (`skipped`) with a reason, without being touched:

- VM, CT of another node, template, with a PVE `lock:`, or stopped without `stopped = "start_and_restore"`.
- Never checked, or check older than `require_fresh_check_minutes`.
- No pending packages: it is skipped (in a run without explicit selection it isn't even listed).

### 3.2 Run flow

1. **Global lock** `/run/lock/pxu-run.lock` (non-blocking): prevents two runs at the same time (timer and TUI). If it is taken, `RunError::Locked` → exit 3. The run log is created when it is taken (§6).
2. If `[update] host_auto = true`, the "all" run starts with the **host** (§3.6); then the NVIDIA driver if `host_nvidia_auto` (§3.7). Only if the host finished well does it continue with the CTs.
3. Each CT, by ascending vmid, goes through the steps in §3.3.
4. **Circuit breaker:** `max_consecutive_failures` failures in a row (2) stop the run (with `on_failure = "stop"`, the first failure). The remaining CTs are left `skipped`.
5. At the end: report, final log and single notification (§4.3).

Cancellation (Ctrl-C/SIGINT/SIGTERM/SIGHUP in the CLI, dialog in the TUI): the run finishes the CT in progress and stops; the following ones are left `cancelled` and the exit is 30. `dpkg`/`apt` are never killed by a cancellation.

### 3.3 Steps per CT

```
Lock → Preflight → Backup → Refresh+Simulate → Upgrade → Verify → (Rollback) → Restore
```

1. **Lock (`lock_ct`).** Per-CT lock `/run/lock/pxu-ct-<vmid>.lock`. Coexistence with ProxMenux: if `/run/lock/proxmenux-lxc-update-<vmid>.lock` exists it is opened **without `O_CREAT`** and `flock` is attempted; if busy, or with content (a reservation), or with `proxmenux-lxc-check-invalid-<vmid>`, the CT is skipped. Any error other than "doesn't exist" also skips it (fails closed). The locks last until the CT is returned to its initial state.
2. **Preflight.** If the CT is stopped (`stopped = "start_and_restore"`) it is started with `pct start`. It checks free space on `/` (`df -Pk /` ≥ `min_free_mb`, 512) and records the failed systemd units (`systemctl --failed`) to compare them later.
3. **Backup** (`backup.mode`, `auto` by default; `[[ct]] backup` changes it per CT):
   - `auto`: asks PVE (`pvesh get /nodes/<node>/lxc/<vmid>/feature --feature snapshot`, read-only); with `hasFeature = 1` it uses `pct snapshot <vmid> pxu-<YYYYMMDDHHMMSS>` (suffix `-2`… `-99` if the name exists); otherwise, or if the query fails, it falls back to vzdump.
   - `snapshot` / `vzdump`: only that route, no fallback to the other.
   - vzdump: `vzdump <vmid> --mode snapshot --storage <[backup] storage> --compress zstd --zstd 1 --ionice 7 --notes-template "pxu pre-update {{guestname}}"`. The storage of `rootfs:` is read from the `.conf` and recorded; without it no backup is made (the rollback would restore to `local`).
   - `none` (only if set by hand): no backup or rollback; the log and `config check` warn about it.
   - **If the backup fails, the CT is not touched** (failure in the `backup` step).
   - **Scope:** the backup and the rollback cover only the rootfs; bind mounts and `mpN` with `backup=0` are neither saved nor reverted (`BACKUP_SCOPE_NOTE`, written in each CT's log).
4. **Refresh + Simulate.** Probe with refresh (always needed to update). With 0 packages the CT is left as `skipped: already up to date after refresh` and the backup is discarded.
5. **Upgrade.** The command from §2.2, with `nice` and `ionice` from `[general]`. Timeout `[update] timeout_min` (30 min) **without killing the process** (`ExecOpts.no_kill_on_timeout`): when it expires, the CT is left failed with a `WARNING` in the log, the run stops and the backup is kept without rollback (the CT may be half-updated; the notification carries the manual command).
6. **Verify.** On apt, `dpkg --audit` must come out empty. The failed units before and after are compared: **new failed units ⇒ failure**. The probe is repeated without refresh, saved in `state.json` and it is noted whether there is `reboot-required`. With a pending reboot: `reboot = "if-required"` runs `pct reboot`; with `"never"` the CT is left as `warn: reboot required`.
7. **Settle** (`settle`), according to the result:
   - Success or `warn`: the backup becomes `kept` and is pruned to the newest `keep` for that CT (`keep = 0` ⇒ it is deleted).
   - `skipped`: the backup is deleted.
   - Failure: **automatic rollback**: `pct shutdown`, `pct stop` if it is still running, and `pct rollback <vmid> <snapshot>` or `pct restore <vmid> <volid> --force --storage <rootfs storage>`; it starts it again if it was running and checks that it responds (`df`). Only then does it delete the backup (`pct delsnapshot` / `pvesm free`).
   - Failed rollback or upgrade timeout: the backup is left `held`, never deleted automatically, the CT is marked failed, the run stops and the manual command is given (`pct rollback …` or `pct stop …; pct restore … --force --storage …`, which pxu never runs on its own).
8. **Restore.** If pxu started the CT, it shuts it down (`pct shutdown --timeout 60`); after an upgrade timeout it does **not** shut it down (dpkg may still be alive) and warns.

Results per CT: `ok`, `warn`, `failed`, `skipped`, `cancelled`.

### 3.4 Backup registry (`update/backup.rs`)

`/var/lib/pxu/backups.json` records each pxu backup with state `active` (in progress; if pxu dies, the next run turns it into `held`), `held` (never deleted) or `kept` (pruning candidate). Only what pxu created is deleted: `pxu-<14 digits>[-N]` snapshots and vzdump volumes recorded in the registry. Foreign snapshots (`manual-*`, `zfs_auto_*`…) are not touched. Each entry is validated on load; a corrupt registry is set aside as `backups.json.corrupt-<epoch>` and that run prunes nothing.

### 3.5 Process priority and output

- Each child process carries `nice` and `ioprio` (`pre_exec`), `process_group(0)` and a single pipe for stdout+stderr (`host/real.rs::run`), so the order is preserved and a Ctrl-C from the terminal doesn't reach `apt`. `nice`/`ioprio` are inherited down to `apt`/`dpkg` inside the CT.
- Read timeouts (probes) are implemented with a temporary thread that kills the group with SIGKILL; the upgrade's doesn't kill (§3.3).
- Output goes line by line to the run log and to the TUI, which keeps a buffer of `LOG_RING = 2000` lines. The engine cuts at 20,000 lines per CT (`LOG_CAP`).

### 3.6 Proxmox host (`update/hostupdate.rs`)

- **By hand:** `pxu update --host` (requires a terminal and confirmation; with `--yes` it only skips the question) or the `u` key on the host row in the TUI.
- **Scheduled:** with `[update] host_auto = true`, the host goes first in the "all" run; it requires a recent host check.
- Order: locks → `dpkg --audit` → space in `/var/cache/apt/archives` → fresh list (by hand it must match the confirmed one) → simulation → copy of `/etc` into `/var/backups/pxu` (5 are kept) → ZFS snapshot if the root is ZFS → `apt-get full-upgrade` (3 h cap, no kill) → `autoremove` if safe → new review. **It never reboots the host.**
- It uses its own opt-in and its own command allowlist (`is_write_argv`).

### 3.7 NVIDIA driver (`update/nvidia.rs`)

Only with `[update] host_nvidia_auto = true` (host) and `[[ct]] nvidia_auto = true` (CT), or from the TUI dialog. It applies to a driver installed with the official `.run` (`/usr/bin/nvidia-uninstall` exists). It downloads the `.run` and its `.sha256sum` from `download.nvidia.com`, verifies, and only upgrades within the same major branch. CTs receive the libraries of the version the host has. It never reboots the host.

---

## 4. Notifications (Apprise)

### 4.1 Transport (`share/notify.rs`)

| `[apprise] transport` | Behavior |
|---|---|
| `auto` (default) | `api` if there is an `api_url`; otherwise `cli` if `apprise` is in the `PATH`; otherwise `none` (notifications disabled, with a visible warning) |
| `api` | `curl` to the Apprise API: with `api_key` it is stateful mode (`POST {api_url}/notify/<key>`, destinations live in the API); without a key, stateless (`POST {api_url}/notify/` with `urls`) |
| `cli` | the `apprise` CLI with the URLs in the `APPRISE_URLS` variable |
| `none` | no notifications |

`pxu` never installs Python or pip. Secrets go **outside argv**: `curl` is given its configuration and the body through stdin (`curl --config -`). Timeout `timeout_s` (10 s) and retries with the waits from `retry_delays_ms` (default 2 s and 8 s). Error messages are cleaned of keys and URLs.

### 4.2 Events (`[apprise] on`)

| Event | When | Deduplication |
|---|---|---|
| `updates_found` | after `check --notify`: CTs (and the host) with new packages | only if the fingerprint changed since the last notification; the host uses the same fingerprint with key `0` |
| `check_error` | a CT couldn't be checked, or the host review failed | — |
| `run_summary` | end of `update`: **a single notification per run** with a table (host, NVIDIA and each CT: result, packages, time, detail and manual action) | — |
| `update_failed` | end of `update` with failures, only if `run_summary` is not enabled | — |

Two notifications are never sent for the same run. With all events in `on` (default value), a run produces one message (`update/report.rs`).

### 4.3 Queue

If sending fails, the message is saved in `/var/lib/pxu/outbox/` (one JSON per message) and retried at the end of the next run. Maximum 50 messages and 7 days.

---

## 5. Configuration (`/etc/pxu/config.toml`)

- Owner root, mode `0600`; `pxu` **refuses to use** a file that another user can write or that isn't owned by root (`config::perm_problem`; fails closed, also if `stat` can't be done). Without a file, `pxu` works in read-only mode and nothing is updated.
- There are no custom commands: the commands are constants of the binary.
- Unknown keys are warned about in `config check` and **block** `update` and the TUI when writing (`load_checked`); `logs_per_ct` and `keep_snapshots` are marked as obsolete.

```toml
[general]
nice = 10                     # 0..19 for child processes
io_class = "idle"             # "idle" | "best-effort"
log_retention_days = 30       # days of logs in /var/log/pxu; 0 = don't delete; cap 3650

[check]
refresh = false               # by default it does NOT refresh indexes
refresh_min_age_hours = 24    # with refresh, only if the index is older
timeout_s = 60
index_stale_days = 7          # older index ⇒ stale

[update]
auto_update_tags = []         # CTs with any of these PVE tags auto-update
stopped = "skip"              # "skip" | "start_and_restore"
require_fresh_check_minutes = 60
min_free_mb = 512
reboot = "never"              # "never" | "if-required"
apt_mode = "dist-upgrade"     # "dist-upgrade" | "upgrade"
on_failure = "continue"       # "continue" | "stop"
max_consecutive_failures = 2  # >= 1
timeout_min = 30
host_auto = false             # the scheduled run also updates the host
host_nvidia_auto = false      # …and the host's NVIDIA driver

[backup]
mode = "auto"                 # "auto" | "snapshot" | "vzdump" | "none"
storage = "local"             # PVE storage for vzdumps
keep = 0                      # pxu backups kept per CT after a successful update (0 = temporary)

[apprise]
transport = "auto"            # "auto" | "api" | "cli" | "none"
api_url = ""
api_key = ""
urls = []                     # stateless / CLI mode
tag = ""
on = ["updates_found", "update_failed", "run_summary", "check_error"]
timeout_s = 10
retry_delays_ms = [2000, 8000]

[ui]
columns = ["type", "os", "cpu", "ram", "disk", "gpu", "upd", "auto"]   # optional: type os cpu ram disk tags gpu upd auto

# Per-CT overrides (optional)
[[ct]]
vmid = 101
auto_update = true
nvidia_auto = false
refresh = false
backup = "snapshot"
keep = 2
```

Priority of `auto_update`: `[[ct]] auto_update` > `auto_update_tags` tag > no. The host (vmid 0) doesn't go through here: `[[ct]] vmid = 0` with `auto_update` or `nvidia_auto` is a config error; `host_auto` / `host_nvidia_auto` are used.

---

## 6. CLI and timer

```
pxu                                   # TUI
pxu list   [--json] [--all-nodes]
pxu check  [--json] [--refresh] [--notify] [--ct 101,103] [vmid...]
pxu update [--json] [--yes] [--no-notify] [--ct 101,103] [vmid...]
pxu update --host [--yes]
pxu config check
pxu notify-test
```

- `update` without arguments updates the CTs with `auto_update` that have a recent positive check. With `[update] host_auto`, it starts with the host. `--yes` / `-y` skips the confirmations (the timer uses it); `--no-notify` suppresses the notification.
- `--json`: one document per execution with `"schema": 1`.
- `pxu config check` validates TOML, values, unknown keys, mode `0600` and owner `root`; it doesn't print secrets.
- `--mock CASE` or `PXU_MOCK=CASE` only exists in builds with the `mock` feature (§8); the production binary rejects it with exit 2.

**Exit codes**

| Code | Command | Meaning |
|---|---|---|
| `0` | all | OK / no updates / notification sent |
| `2` | all | incorrect usage, invalid config or permissions |
| `3` | `update` | another run in progress |
| `5` | `notify-test` | sending failed |
| `10` | `check` | there are updates |
| `12` | `check` | at least one CT failed to be checked (takes precedence over 10) |
| `20` | `update` | run with partial failures |
| `30` | `update` | run stopped: circuit breaker or cancelled |

(`check.rs` and `update/mod.rs`, `EXIT_*` constants.)

### systemd timer

The units are in `dist/systemd/` (the same source that `share/schedule.rs` embeds and that `install.sh --timer` copies):

```ini
# pxu-update.service
[Service]
Type=oneshot
ExecStart=-/usr/local/bin/pxu check --notify
ExecStart=/usr/local/bin/pxu update --yes
Nice=10
IOSchedulingClass=idle
TimeoutStartSec=4h
KillMode=mixed

# pxu-update.timer
[Timer]
OnCalendar=*-*-* 04:30
RandomizedDelaySec=15m
Persistent=true
```

The TUI's `S` key saves another schedule as a `pxu-update.timer.d` drop-in that replaces `OnCalendar`; `install.sh --uninstall` deletes it. The log of each scheduled run and its retention: §7.

---

## 7. Logs

One log per run: `/var/log/pxu/run-YYYYMMDD-HHMM[SS][-N].log` (UTC time in the name; `update/runlog.rs`).

- It is created when the lock is taken with a minimal header (`pxu-run-log 1`, `started`, `node`, `---` line) and grows while it runs, with one section per host, NVIDIA and CT and the local time of each line.
- When it ends it is rewritten entirely: full header (`duration`, `result`), the sections and the same summary as the notification (a single source, `update/report.rs`). A log without `result` (abrupt cut) is shown as `⛔ INCOMPLETE`.
- Retention by days: `[general] log_retention_days` (30; `0` = don't delete). When each run ends, the oldest `run-*.log` are deleted; `prune_in` only touches names in pxu's format. Logs in the previous format (`host-*.log`, `nvidia-*.log`, per-CT folders) also expire by days, but the Logs page only lists `run-*.log`.
- The log masks URLs with user and password (`fmt::mask_url_creds`) and replaces control characters (`fmt::clean`).
- The TUI lists and opens them with the `l` key (up to `LIST_MAX = 1000` logs and `VIEW_MAX_LINES = 100 000` lines); see DESIGN §5.

---

## 8. Code structure and testing

### 8.1 Tree

A single `pxu` package: a library (`src/lib.rs` → `share/`, no terminal) and the binary (`main.rs`, `cli/`, `tui/`) on top. `cli/` and `tui/` only depend on `share/`, not on each other.

```
src/
├─ main.rs                 # chooses CLI or TUI; wraps the host in ReadOnlyHost
├─ lib.rs                  # pub mod share
├─ cli/{mod,args}.rs       # commands; lexopt
├─ tui/{mod,app,views,logs,sched,apprise,tests}.rs
└─ share/
   ├─ config.rs  model.rs  fmt.rs  labels.rs  state.rs  schedule.rs
   ├─ notify.rs                         # Apprise
   ├─ proxmox/{conf,guests,hostcheck}.rs + probe.sh
   ├─ update/{mod,check,backup,hostupdate,nvidia,report,runlog}.rs
   ├─ host/{mod,real,readonly}.rs       # Host trait, RealHost, ReadOnlyHost
   └─ mock/{mod,scenario}.rs            # MockHost + World builder
tests/snapshots/           # TUI captures (80×24 and 120×40)
dist/systemd/              # pxu-update.service and pxu-update.timer
install.sh   mise.toml
```

Dependencies (`Cargo.toml`, all without default features except `lexopt` and `libc`): `ratatui`, `crossterm`, `serde`, `toml`, `toml_edit` (to edit the config preserving comments), `serde_json`, `lexopt`, `libc`. There is no async runtime, `clap`, `regex` or `chrono`. The `release` profile uses `opt-level = "s"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` and `strip`.

### 8.2 The `Host` trait

`share/host/mod.rs`. All of the library's code talks to `&dyn Host`:

```rust
fn read_file / read_dir / exists / read_head / file_meta
fn guest_exec(vmid, argv, opts, sink)      // inside the CT
fn host_cmd(argv, opts, sink)              // on the host
fn write_file / write_config / append_file / remove_file / remove_empty_dir
fn try_lock(path, create) / lock_dir
fn timer_status / validate_calendar / apply_schedule
fn set_update_enabled / set_update_target / now
```

- `RealHost` uses `/` and `std::process::Command`.
- `ReadOnlyHost` wraps any host and only lets through commands from an **allowlist**; writes require explicit, scoped opt-ins (CT in progress, host, NVIDIA). Security detail: [SECURITY-REVIEW.md](SECURITY-REVIEW.md).
- `MockHost` serves a virtual in-memory filesystem and answers commands according to a `Scenario`; it is built by the `World` builder (`share/mock/scenario.rs`). There is no versioned data on disk.

### 8.3 `World` builder and cases

A case declares only what changes (distro, running/stopped, storage with or without snapshot, injected failures, ProxMenux locks, VMs and guests of another node, backup policy). Predefined cases: `demo`, `update-ok`, `update-fail`, `distros`, `cluster`, `failures`, `vms`, `host`. The values are made up, not captures of a server.

### 8.4 Tests

- Unit: `.conf` parser, `@@` protocol, parsers of each manager, config, notifications (simulated curl), timer schedule.
- Integration: full `check` and `update` against `MockHost`; the exact sequence of commands is checked, which also proves that read-only mode never emits `apt-get update`/`apk update`/etc. (`check_path_is_read_only`).
- TUI: `TestBackend` captures against `tests/snapshots/`.
- `mise run test` runs the tests in the dev and `mock` profiles; `mise run check` additionally checks that the production binary rejects `--mock`. See [development.md](../development.md).
