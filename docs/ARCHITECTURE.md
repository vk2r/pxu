# Architecture

English | [Español](ARCHITECTURE.es.md)

pxu is a lightweight LXC container updater for Proxmox VE. It is a single Rust binary with a TUI (ratatui) and a CLI. It has no daemon and no dependencies on the host, and uses a few MB of RAM while it runs.


Technical reference of how `pxu` works today. The TUI is described in [DESIGN](DESIGN.md); the security analysis, in [SECURITY-REVIEW](SECURITY-REVIEW.md). Installation and usage: [README](../README.md); building and testing: [DEVELOPMENT](../DEVELOPMENT.md).

Tested on: the automated tests run against a simulated server (`MockHost`, [§8](#8-code-structure-and-testing)), and pxu runs daily on a real server with Proxmox VE 9.2 and `dir`, `lvmthin` and `pbs` storages. Other storage types (ZFS, Ceph, NFS…) and other Proxmox versions have not been tried on a real server yet.

Status: v0.1, in development. Tested on PVE 9.2 (real update and rollback on a Debian 13 CT). Apprise notifications and the rollback of a CT that was stopped are only tested with simulated data.

---

## 0. Decisions

| Topic | Decision | Where to see it |
|---|---|---|
| Language / TUI | Rust + ratatui (crossterm); `libc` for locks, priorities and signals | `Cargo.toml` |
| Process | **No daemon.** It runs, does its work and exits. Automation is a systemd timer | `dist/systemd/` |
| Concurrency | **No async runtime.** The TUI spawns one thread per job (check, update); the CLI works on the main thread | `src/tui/mod.rs`, `src/cli/mod.rs` |
| Listing | Reads `/etc/pve/**/*.conf` and `/sys/fs/cgroup`; without launching processes | [§1](#1-listing-lxcs-and-vms) |
| Execution in CT | **A single execution per CT per check** (embedded `probe.sh` script) with `lxc-attach` | [§2.1](#21-one-execution-per-ct) |
| Check | Refreshes the indexes by default (`[check] refresh`); `--no-refresh` / `C` = read-only | [§2.3](#23-modes) |
| Update | Sequential; global lock + per-CT lock; prior temporary backup (snapshot or vzdump) with automatic rollback; post-update verification | [§3](#3-update-pxu-update) |
| Proxmox host and NVIDIA | Reviewed on every `check`; only updated on explicit order or with an opt-in in config | [§3.6](#36-proxmox-host-updatehostupdaters), [§3.7](#37-nvidia-driver-updatenvidiars) |
| Notifications | Apprise API via `curl` (fallback: `apprise` CLI); a single notification per run | [§4](#4-notifications-apprise) |
| Config | `/etc/pxu/config.toml`, 0600 root | [§5](#5-configuration-etcpxuconfigtoml) |
| Testing | `Host` trait with `RealHost` and `MockHost` (in-memory simulated server) | [§8](#8-code-structure-and-testing) |

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

`pxu list --all-nodes` includes the guests of other nodes; without the option only those of the local node appear. Besides the guests, the inventory includes a row for the **Proxmox host** (vmid `0`, [§3.6](#36-proxmox-host-updatehostupdaters)).

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
@@cache_age <seconds>                   # index age AFTER the refresh (0 if it succeeded); -1 if unknown; emitted once, after @@refresh
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

- **Refresh (default):** `[check] refresh = true`. `pxu check` and the TUI's `c` refresh the index inside each CT first. With `refresh_min_age_hours` (12) it only refreshes if the index is older (12 h, not 24: with a daily timer a 24 h limit would skip every other day). `--refresh` is still accepted.
- **Read-only:** `pxu check --no-refresh` or the TUI's `C` (also `c` in `[RO]`) never write in the guests, not even with `[[ct]] refresh = true`. `[check] refresh = false` and `[[ct]] refresh = false` turn the refresh off in the config. Shows the state of the index the CT already has, with its age.
- An index older than `[check] index_stale_days` (7) is marked `stale`.
- Stopped CTs, VMs and remote guests are not checked (`check::targets`).

### 2.4 Execution and persistence

- Sequential, one CT after another; the per-CT timeout is `[check] timeout_s` (60 s; triple with refresh).
- The result is saved in `/var/lib/pxu/state.json` (atomic write, and the whole read-modify-write cycle runs under `/run/pxu/pxu-state.lock` via `state::update_state`, 10 s wait, so check, update and notices never overwrite each other): date, manager, packages, number of security updates, index age and a **fingerprint** (FNV-1a of the sorted list `name=new_version`, `state::fingerprint`).
- The fingerprint avoids repeating notifications: [§4.2](#42-events-apprise-on). A check is "recent" for `update` if it is less than `[update] require_fresh_check_minutes` (60) old.
- `pxu check` also reviews the **Proxmox host** (`proxmox/hostcheck.rs`, read-only): pending packages, repositories, pending reboot and NVIDIA GPU/driver. The only commands it uses are `apt-get update` (only with refresh), `apt list --upgradable`, `apt-get -s … full-upgrade`, `lspci -nn` and `dkms status`.

---

## 3. Update (`pxu update`)

Code: `src/share/update/mod.rs`. It is the only command that writes.

### 3.1 Selection

`update::select` takes, without arguments, the LXCs with `auto_update` (by `[[ct]]` or by tag, [§5](#5-configuration-etcpxuconfigtoml)); with vmids or `--ct` it takes those. Each candidate can end up **skipped** (`skipped`) with a reason, without being touched:

- VM, CT of another node, template, with a PVE `lock:`, or stopped without `stopped = "start_and_restore"`.
- Never checked, or check older than `require_fresh_check_minutes`.
- No pending packages: it is skipped (in a run without explicit selection it isn't even listed).

### 3.2 Run flow

1. **Global lock** `/run/pxu/pxu-run.lock` (non-blocking): prevents two runs at the same time (timer and TUI). If it is taken, `RunError::Locked` → exit 3. The run log is created when it is taken ([§6](#6-cli-and-timer)).
2. If `[update] host_auto = true`, the "all" run starts with the **host** ([§3.6](#36-proxmox-host-updatehostupdaters)); then the NVIDIA driver if `host_nvidia_auto` ([§3.7](#37-nvidia-driver-updatenvidiars)). Only if the host finished well does it continue with the CTs.
3. Each CT, by ascending vmid, goes through the steps in [§3.3](#33-steps-per-ct).
4. **Circuit breaker:** `max_consecutive_failures` failures in a row (2) stop the run (with `on_failure = "stop"`, the first failure). The remaining CTs are left `skipped`.
5. At the end: report, final log and single notification ([§4.4](#44-notification-format)).

Cancellation (Ctrl-C/SIGINT/SIGTERM/SIGHUP in the CLI, dialog in the TUI): the run finishes the CT in progress and stops; the following ones are left `cancelled` and the exit is 30. `dpkg`/`apt` are never killed by a cancellation. Exception: before the install (pre-checks/backup) the cancel is immediate: only `vzdump` is signalled (SIGTERM; SIGKILL of its group after 120 s), the partial file/snapshot/lock are cleaned up by vzdump itself (pxu verifies it) and the CT ends `cancelled`, untouched.

### 3.3 Steps per CT

```
Lock → Preflight → Backup → Refresh+Simulate → Upgrade → Verify → (Rollback) → Restore
```

1. **Lock (`lock_ct`).** Per-CT lock (`/run/pxu` is created `0700` and validated before use; ProxMenux's locks stay in `/run/lock`, read-only) `/run/pxu/pxu-ct-<vmid>.lock`. Coexistence with ProxMenux: if `/run/lock/proxmenux-lxc-update-<vmid>.lock` exists it is opened **without `O_CREAT`** and `flock` is attempted; if busy, or with content (a reservation), or with `proxmenux-lxc-check-invalid-<vmid>`, the CT is skipped. Any error other than "doesn't exist" also skips it (fails closed). The locks last until the CT is returned to its initial state.
2. **Preflight.** If the CT is stopped (`stopped = "start_and_restore"`) it is started with `pct start`. It checks free space on `/` (`df -Pk /` ≥ `min_free_mb`, 512) and records the failed systemd units (`systemctl --failed`) to compare them later.
3. **Backup** (`backup.mode`, `auto` by default; `[[ct]] backup` changes it per CT):
   - `auto`: asks PVE (`pvesh get /nodes/<node>/lxc/<vmid>/feature --feature snapshot`, read-only); with `hasFeature = 1` it uses `pct snapshot <vmid> pxu-<YYYYMMDDHHMMSS>` (suffix `-2`… `-99` if the name exists); otherwise, or if the query fails, it falls back to vzdump.
   - `snapshot` / `vzdump`: only that route, no fallback to the other.
   - vzdump: `vzdump <vmid> --mode snapshot --storage <[backup] storage> --compress zstd --zstd 1 --ionice 7 --notes-template "pxu pre-update {{guestname}}"`. The storage of `rootfs:` is read from the `.conf` and recorded; without it no backup is made (the rollback would restore to `local`).
   - `none` (only if set by hand): no backup or rollback; the log and `config check` warn about it.
   - **Space check:** before vzdump pxu estimates the size (rootfs used from `df -Pk /` + non-bind `mpN` without `backup=0` at their allocated size, × `[backup] space_ratio`) and compares it with the free space of the storage (`pvesh get /nodes/<node>/storage/<storage>/status`, read-only). If less than max(`min_free_gb`, 10% of total) would remain, or the status is unreadable, vzdump is not launched: the CT fails with `backup skipped: needs ~X GB, storage S has Y GB free — CT untouched` and the run goes on with the next CT (also in `pxu update` and the auto-update).
   - **If the backup fails, the CT is not touched** (failure in the `backup` step).
   - **Scope:** the backup and the rollback cover only the rootfs; bind mounts and `mpN` with `backup=0` are neither saved nor reverted (`BACKUP_SCOPE_NOTE`, written in each CT's log).
4. **Refresh + Simulate.** Probe with refresh (always needed to update). With 0 packages the CT is left as `skipped: already up to date after refresh` and the backup is discarded.
5. **Upgrade.** The command from [§2.2](#22-by-package-manager), with `nice` and `ionice` from `[general]`. Timeout `[update] timeout_min` (30 min) **without killing the process** (`ExecOpts.no_kill_on_timeout`): when it expires, the CT is left failed with a `WARNING` in the log, the run stops and the backup is kept without rollback (the CT may be half-updated; the notification carries the manual command).
6. **Verify.** On apt, `dpkg --audit` must come out empty. The failed units before and after are compared: **new failed units ⇒ failure**. The probe is repeated without refresh, saved in `state.json` and it is noted whether there is `reboot-required`. With a pending reboot: `reboot = "if-required"` runs `pct reboot`; with `"never"` the CT is left as `warn: reboot required`.
7. **Settle** (`settle`), according to the result:
   - Success or `warn`: the backup becomes `kept` and is pruned to the newest `keep` for that CT (`keep = 0` ⇒ it is deleted).
   - `skipped`: the backup is deleted.
   - Failure: **automatic rollback**: `pct shutdown`, `pct stop` if it is still running, and `pct rollback <vmid> <snapshot>` or `pct restore <vmid> <volid> --force --storage <rootfs storage>`; it starts it again if it was running and checks that it responds (`df`). Only then does it delete the backup (`pct delsnapshot` / `pvesm free`).
   - Failed rollback or upgrade timeout: the backup is left `held`, never deleted automatically, the CT is marked failed, the run stops and the manual command is given (`pct rollback …` or `pct stop …; pct restore … --force --storage …`, which pxu never runs on its own).
8. **Restore.** If pxu started the CT, it shuts it down (`pct shutdown --timeout 60`); after an upgrade timeout it does **not** shut it down (dpkg may still be alive) and warns.

Results per CT: `ok`, `warn`, `failed`, `skipped`, `cancelled`.

### 3.4 Backup registry (`update/backup.rs`)

`/var/lib/pxu/backups.json` records each pxu backup with state `active` (in progress; if pxu dies, the next run turns it into `held`), `held` (never deleted) or `kept` (pruning candidate). Only what pxu created is deleted: `pxu-<14 digits>[-N]` snapshots and vzdump volumes recorded in the registry. Foreign snapshots (`manual-*`, `zfs_auto_*`…) are not touched. Each entry is validated on load; a corrupt registry is set aside as `backups.json.corrupt-<epoch>` and that run prunes nothing. pxu only deletes the backups in its own registry, and each write command is tied to the CT being updated.

### 3.5 Process priority and output

- Each child process carries `nice` and `ioprio` (`pre_exec`), `process_group(0)` and a single pipe for stdout+stderr (`host/real.rs::run`), so the order is preserved and a Ctrl-C from the terminal doesn't reach `apt`. `nice`/`ioprio` are inherited down to `apt`/`dpkg` inside the CT.
- Read timeouts (probes) are implemented with a temporary thread that kills the group with SIGKILL; the upgrade's doesn't kill ([§3.3](#33-steps-per-ct)).
- Output goes line by line to the run log and to the TUI, which keeps a buffer of `LOG_RING = 2000` lines. The engine cuts at 20,000 lines per CT (`LOG_CAP`).

### 3.6 Proxmox host (`update/hostupdate.rs`)

- **By hand:** `pxu update --host` (requires a terminal and confirmation; with `--yes` it only skips the question) or the `u` key on the host row in the TUI.
- **Scheduled:** with `[update] host_auto = true`, the host goes first in the "all" run; it requires a recent host check.
- Order: locks → `dpkg --audit` → space in `/var/cache/apt/archives` → fresh list (by hand it must match the confirmed one) → simulation → copy of `/etc` into `/var/backups/pxu` (5 are kept) → ZFS snapshot if the root is ZFS → `apt-get full-upgrade` (3 h cap, no kill) → `autoremove` if safe → new review. **It never reboots the host.**
- It uses its own opt-in and its own command allowlist (`is_write_argv`).

### 3.7 NVIDIA driver (`update/nvidia.rs`)

Only with `[update] host_nvidia_auto = true` (host) and `[[ct]] nvidia_auto = true` (CT), or from the «Include GPU driver update» box of the TUI window (`a`, next to «Automatic updates»; it saves only what changed; checking it on a CT also turns the host flag on, unchecking it on a CT only that CT, unchecking auto-update unchecks it; the GPU column shows `+drv` when it is on). It applies to a driver installed with the official `.run` (`/usr/bin/nvidia-uninstall` exists). It downloads the `.run` and its `.sha256sum` from `download.nvidia.com`, verifies, and only upgrades within the same major branch. CTs receive the libraries of the version the host has. It never reboots the host.

**By hand (`Mode::Reinstall` / `Mode::CtLibs`).** `pxu update --host --nvidia-reinstall --yes` reinstalls the host driver even if the version matches (it does not need `host_nvidia_auto`, and the timer never uses it); in the TUI, `u` on a GPU CT with nothing to update (or `n` in the update window) reinstalls only that CT's libraries with the host's version, without touching the host or the other CTs. The host result and each CT result are reported separately: a failed CT does not mark the host as failed. A reboot is requested only if `nvidia-smi` does not confirm the new version at the end of the host step; in that case the CTs get no libraries (Skipped) and the NVIDIA step is Warn.

---

## 4. Notifications (Apprise)

### 4.1 Transport (`share/notify.rs`)

| `[apprise] transport` | Behavior |
|---|---|
| `auto` (default) | `api` if there is an `api_url`; otherwise `cli` if `apprise` is in the `PATH`; otherwise `none` (notifications disabled, with a visible warning) |
| `api` | `curl` to the Apprise API: with `api_key` it is stateful mode (`POST {api_url}/notify/<key>`, destinations live in the API); without a key, stateless (`POST {api_url}/notify/` with `urls`) |
| `cli` | the `apprise` CLI with the URLs in the `APPRISE_URLS` variable |
| `none` | no notifications |

`pxu` never installs Python or pip. Secrets go **outside argv**: `curl` is given its configuration and the body through stdin (`curl --config -`). Timeout `timeout_s` (10 s) and retries with the waits from `retry_delays_ms` (default 5 s, 30 s, 2 min and 5 min, ~7.5 min), only for transient errors (curl failure, HTTP 408/424/429/5xx); any other 4xx (bad key, bad request) is not retried. `pxu check --notify` only uses the first two waits so it does not hold up the update that follows. Error messages are cleaned of keys and URLs. `api_key` and `urls` are stored encrypted and only decrypted right before sending ([§4.6](#46-secrets-encryption-with-systemd-creds)).

### 4.2 Events (`[apprise] on`)

| Event | When | Deduplication |
|---|---|---|
| `updates_found` | after `check --notify`: CTs (and the host) with new packages | only if the fingerprint changed since the last notification; the host uses the same fingerprint with key `0` |
| `check_error` | a CT couldn't be checked, or the host review failed | — |
| `run_summary` | end of `update`: **a single notification per run** with a table (host, NVIDIA and each CT: result, packages, time, detail and manual action) | — |
| `update_failed` | end of `update` with failures, only if `run_summary` is not enabled | — |

Two notifications are never sent for the same run. With all events in `on` (default value), a run produces one message (`update/report.rs`).

### 4.3 Queue

If sending fails, the message is saved in `/var/lib/pxu/outbox/` (one JSON per message) and retried at the end of the next run, and every 15 minutes by `pxu notify-flush` (units `pxu-notify-flush.service` + `.timer`, `OnCalendar=*:0/15`, `Persistent=true`, installed by the TUI's `S` key). `pxu notify-flush` only writes to the outbox: it prints `outbox: N sent, M still queued`, exits 0 if the queue is empty and 5 if something is left, and with an empty queue it does not touch the network. Maximum 50 messages and 7 days.

### 4.4 Notification format

Each update run (timer, `pxu update` or the TUI) ends with **a single notification**: a Markdown table (`Upgrades | Result | Packages | GPU driver | Time | Details | Manual`, the host first and then each CT, including failed and skipped ones) under the title `pxu: update run on <host> — N ok, N warn, N failed · <dur>` (+ `STOPPED` if it was stopped, + `· NVIDIA failed`). With more than 25 rows, the ones that are not ok come first, then up to 25 ok and `+N more ok (see log)`. The `Manual` column only appears if some row has something to do by hand. The notification level is success, warning or failure according to the worst result.

- `run_summary` event: always sent when the run ends. `update_failed` event: if it is the only one (without `run_summary`), the same notification is sent only when some CT or the NVIDIA driver failed. Old configs remain valid.

### 4.5 Setup from the TUI

In the Config view (`g`) the "Notifications (Apprise)" section shows the status of `[apprise]` with secrets hidden: `api_url` with only scheme and host, `api_key` as `•••` with `(set, encrypted)` or `(set, plain text)` (plus a note to save it in `n` to encrypt), and each destination URL as `tgram://…`; it also shows `tag` (or `—`).

- `n` opens the dialog: API address, mode (key stored in Apprise API **or** direct URLs, comma-separated), the `tag` (visible text, empty = the key is removed; letters, digits, `-_.,`; without a tag the Apprise API only notifies untagged targets) and the events that notify (`updates_found`, `update_failed`, `run_summary`, `check_error`). The stored secret is not shown: with the field empty it is kept, and what you type is drawn as dots. On saving, the secret is written **encrypted** ([§4.6](#46-secrets-encryption-with-systemd-creds)).
- With Enter it validates the entire resulting config with the same code as `pxu config check`; if it doesn't validate (or no usable transport remains) it writes nothing and says why. Then it asks for confirmation with "today → new" (secrets hidden): `y`/Enter saves, `b` goes back to editing, Esc cancels.
- It saves only `[apprise]`: the rest of `config.toml` and its comments are preserved, the write is atomic (root, `0600`) and, if the file doesn't exist, it creates it. The key and the URLs are mutually exclusive: changing mode removes the other.
- `t` sends a test notification, the same as `pxu notify-test` (shows "sending…", then OK or the error with code 5, without dumping secrets).
- With `--read-only`, editing and testing are rejected: the test goes out over the network, so it counts as an action.

### 4.6 Secrets: encryption with `systemd-creds`

`systemd-creds` is read from **stdout only** (stderr is kept apart: it always warns that the host key is not on encrypted media, and that text must never end up inside the secret). `seal` checks that the output is base64 and decrypts it back: if the value does not match, it fails with «round-trip check failed» and nothing is written. `unseal` only drops the final newline.

`[apprise] api_key` and `urls` are the only secrets pxu keeps. Saving them from the TUI (`g` → `n`) stores them **encrypted**:

- **How:** `systemd-creds encrypt --name=pxu-apprise --with-key=auto - -` (the secret goes in through stdin). `auto` uses the TPM2 chip if the machine has a usable one and otherwise the host key `/var/lib/systemd/credential.secret` (systemd creates it, root-only). pxu has no cryptography of its own and no key inside the binary.
- **In the file:** the result (base64, one line) is written as `api_key_enc` / `urls_enc` (the URLs are encrypted one per line) and the plain `api_key` / `urls` key is **deleted** in the same atomic write. Only the secret of the chosen mode is kept.
- **When it is decrypted:** only right before a send (`notify::send_once`, the one entry point of every notice, test, queue flush and retry): `systemd-creds decrypt --name=pxu-apprise - -`, input and output through stdin/stdout. The decrypted value lives in a copy of the config that is dropped after the send; it never goes to argv, logs, the screen, the outbox queue or the `ps` list, and error messages are scrubbed of it.
- **If it cannot be decrypted** (no `systemd-creds`, other machine, damaged value): nothing is sent, the error is `cannot decrypt apprise secret: …`, and the notice stays in the outbox queue like any failure (not retried in a loop: waiting does not fix it). `pxu notify-test` and the TUI show the error. pxu **never** falls back to plain text silently.
- **Old configs:** a plain `api_key` / `urls` keeps working. `pxu config check` and the Config view say `stored in plain text — save it in Config → n to encrypt`; saving in `n` (even with the secret field empty) or `pxu config encrypt-secrets` encrypts what is stored. If a plain and an encrypted value coexist, the plain one wins and `config check` warns.
- **Config view:** `api_key : ••• (set, encrypted)` or `(set, plain text)`; the "today → new" confirmation shows `•••` plus the same word.

What this protects, honestly: **backups, copies of `config.toml` and other machines** (the ciphertext is bound to this host's key or TPM and to the name `pxu-apprise`, so it is useless anywhere else). What it does **not** protect: **root on the same host** can run `systemd-creds decrypt --name=pxu-apprise` (or read pxu's memory) and get the secret; no program can prevent that, because the key lives on the same machine. The file stays `0600` root. A backup that also copies `/var/lib/systemd/credential.secret` carries the key with it. The traffic to the Apprise API is as private as its URL: with `http://` the key travels unencrypted on the network.

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
refresh = true                # by default it refreshes the indexes (false = read-only check)
refresh_min_age_hours = 12    # with refresh, only if the index is older
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
space_ratio = 1.0              # estimated backup size / used size (zstd 1:1)
min_free_gb = 5               # minimum spare GiB on the storage (or 10% of total, whichever is larger)

[apprise]
transport = "auto"            # "auto" | "api" | "cli" | "none"
api_url = ""
api_key = ""                 # plain text (old configs); the TUI saves api_key_enc instead (§4.6)
urls = []                     # stateless / CLI mode (plain text, same)
api_key_enc = ""              # written by the TUI: api_key encrypted with systemd-creds
urls_enc = ""
tag = ""
on = ["updates_found", "update_failed", "run_summary", "check_error"]
timeout_s = 10
retry_delays_ms = [5000, 30000, 120000, 300000]

[ui]
columns = ["type", "os", "cpu", "ram", "disk", "gpu", "upd", "auto", "backup"]   # optional: type os cpu ram disk tags gpu upd auto backup (tags hidden by default)

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

### 5.1 Validation and editing from the TUI

The TUI also writes those overrides: `a` toggles the `auto_update` of the selected CT (modal with "today: … → new state"; `y`/Enter confirms, Esc cancels). It only changes the `auto_update` line of that vmid's `[[ct]]` block, or adds a block at the end; comments and order are preserved. The resulting TOML is validated and written atomically (root, `0600`). An override of `false` wins over the CT's tag.

`b` does the same for the backup before the update: it writes `backup = "none"` in the CT's `[[ct]]` block (or adds the block), and re-enabling removes the `backup` key so the CT follows `[backup] mode` again (the block stays, with its other keys). The modal shows the current mode and where it comes from (`[[ct]]` override or `[backup] mode`) and, when disabling, warns that the update runs without backup and without automatic rollback. The host (vmid 0) is not handled here. The list can show the effective mode in the optional `BACKUP` column (`auto`/`snap`/`vzdump`/`none`) and the CT detail shows it too.

`pxu update` and the TUI validate the config the same way as `pxu config check` before using it: unknown values (`on_failure = "Stop"`, `apt_mode = "Upgrade"`) or misspelled keys (`on_falure`) are errors. `pxu update` exits with code 2 and shows them; the TUI starts in read-only mode with the reason in the status bar. Warnings don't block.

---

## 6. CLI and timer

```
pxu                                   # TUI
pxu list   [--json] [--all-nodes]
pxu check  [--json] [--no-refresh] [--notify] [--ct 101,103] [vmid...]
pxu update [--json] [--yes] [--no-notify] [--ct 101,103] [vmid...]
pxu update --host [--yes] [--nvidia-reinstall]   # reinstall the NVIDIA driver even if the version matches (needs --yes; never from the timer)
pxu config check
pxu config encrypt-secrets
pxu notify-test
pxu notify-flush
pxu --version | -v
pxu --help | -h
```

- `update` without arguments updates the CTs with `auto_update` that have a recent positive check. With `[update] host_auto`, it starts with the host. `--yes` / `-y` skips the confirmations (the timer uses it); `--no-notify` suppresses the notification.
- `--json`: one document per execution with `"schema": 1`.
- `pxu config check` validates TOML, values, unknown keys, mode `0600` and owner `root`; it doesn't print secrets, and notes an `[apprise]` secret stored in plain text.
- `pxu config encrypt-secrets` encrypts a plain-text `[apprise]` `api_key` / `urls` with `systemd-creds` (same result as saving in `n`; needs root; nothing to do → exit 0).
- `--mock CASE` or `PXU_MOCK=CASE` only exists in builds with the `mock` feature ([§8](#8-code-structure-and-testing)); the production binary rejects it with exit 2.

**Exit codes**

| Code | Command | Meaning |
|---|---|---|
| `0` | all | OK / no updates / notification sent |
| `2` | all | incorrect usage, invalid config or permissions |
| `3` | `update` | another run in progress |
| `5` | `notify-test`, `notify-flush` | sending failed / notices still queued |
| `10` | `check` | there are updates |
| `12` | `check` | at least one CT failed to be checked (takes precedence over 10) |
| `20` | `update` | run with partial failures |
| `30` | `update` | run stopped: circuit breaker or cancelled |

(`check.rs` and `update/mod.rs`, `EXIT_*` constants.)

### Installer (`install.sh`)

`install.sh [--force] [--release vX.Y.Z] [--uninstall [--purge]] [path-to-binary]`. It must run as root; without `--force` it requires the machine to look like Proxmox (`pct` and `/etc/pve`). It is safe to rerun.

- **Installs** the binary at `/usr/local/bin/pxu` (atomic replacement) and creates `/etc/pxu`, `/var/lib/pxu` and `/var/log/pxu` with mode `0700`. It **never touches** an existing `/etc/pxu/config.toml`; if there is none, it says pxu stays read-only.
- **Download verification.** Every download requires HTTPS (also on redirects). Before `tar` it fetches `provenance.sigstore.json` and runs `cosign verify-blob-attestation --new-bundle-format --type slsaprovenance1` pinned to the GitHub Actions issuer and the `release.yml` workflow of `<repo>` on a `v*` tag; a missing or pre-2.4.0 cosign (and a missing curl/tar/sha256sum) is installed through apt, and if that is not possible (not root, no apt-get) it aborts with the command to run by hand; with an invalid attestation it aborts with "nothing was installed". `PXU_SKIP_ATTESTATION=1` is the only degraded mode and prints a WARNING; SHA-256 stays as a second check. Installing from a local binary is unaffected.
- **`--uninstall`** disables the timers and removes the units, the `pxu-update.timer.d` drop-in (the schedule the TUI saves) and the binary; it keeps `/etc/pxu`, `/var/lib/pxu` and `/var/log/pxu`. With `--purge` it also deletes those three directories.
- Variables for testing: `DESTDIR` (install under another root, no root, no systemctl) and `PXU_UNITS` (units folder).
- To **update** pxu, repeat the install: the binary is replaced and the configuration is kept.
- Build, copy to the server and first steps: [README](../README.md).


### systemd timer

The units are in `dist/systemd/` (the same source that `share/schedule.rs` embeds and that `install.sh` copies on an update):

```ini
# pxu-update.service
[Unit]
OnFailure=pxu-update-failed.service pxu-update-notify-failed.service

[Service]
Type=oneshot
ExecStart=-/usr/local/bin/pxu check --notify
ExecStart=/usr/local/bin/pxu update --yes
Nice=10
IOSchedulingClass=idle
TimeoutStartSec=20h
TimeoutStopSec=4h
KillMode=mixed

# pxu-update.timer
[Timer]
OnCalendar=*-*-* 04:30
RandomizedDelaySec=15m
Persistent=true
```

Time limits. Every command of `update` has its own `timeout_s` (queries 2 min, `pct` start/stop 5 min, `pct snapshot` 30 min, `vzdump` and rollback/restore 2 h; `apt`/`dpkg` get their timeout plus a 30-minute grace, then SIGTERM, then SIGKILL to the group as the last resort). A command with no limit is a bug: debug builds panic, release builds apply 10 min, and a test fails if `timeout_s: None` appears. The run has a 16 h budget (`RUN_DEADLINE_S`) that every step checks BEFORE it starts: the host, the NVIDIA step and each CT only start if their worst case (the sum of their internal limits: `host_worst_s`, `nvidia::worst_s`, `ct_worst_s`) fits in the time left; otherwise they are reported as "not started: run time limit" and the run is marked aborted and sent through Apprise. `pxu check` has its own 1 h budget (`CHECK_DEADLINE_S`: the CTs left are reported as timeout and the host check is skipped). Both `ExecStart` share `TimeoutStartSec=20h`; a test requires check + `RUN_DEADLINE_S` + `RUN_MARGIN_S` (2 h: notices with retries, report) < 20 h < 24 h, so a stuck run never blocks the next day's. `TimeoutStartSec` is only the last resort: if systemd reaches it, the final SIGKILL can still hit `apt`/`dpkg`. On `systemctl stop`, `pxu` gets SIGTERM, finishes the step in progress and starts no more CTs; `TimeoutStopSec=4h` covers that step (a stop in the middle of a long full-upgrade followed by an autoremove can still go past it). Config maxima (rejected by `config check`, clamped on load): `[check] timeout_s` 120 s, `[update] timeout_min` 120, `[apprise] timeout_s` 30 s and at most 6 `retry_delays_ms` adding up to 10 min, so that no worst case exceeds 16 h and the notices (`notify_worst_s`: the summary with all its retries, the check's events, and at most 10 queued notices per flush pass; the encrypted secrets are decrypted once per send and once per pass, one `systemd-creds` call per encrypted secret: `reveal_s(n)`, 100 s with two) fit in `RUN_MARGIN_S`. `check` starts each CT probe (up to `3 × timeout_s` with refresh) and the host check only if its worst case fits in `CHECK_DEADLINE_S`. `nvidia::worst_s` sums every long command the step runs (host: download, extract, test build, sha256, `--check`, install; per GPU CT: copy, installer, `pct` stop/start, queries), and a test walks the mock and checks the commands never add up to more (4 GPU CTs). Pruning old backups deletes at most 4 per CT and run (the oldest; the rest wait for the next run).

Failure notice that does not depend on `pxu`. `OnFailure=` starts two units when the service fails or systemd kills it (it does not fire while the run is merely hung, before the 20 h limit): `pxu-update-failed.service` writes a critical journal entry (always, as the fallback) and `pxu-update-notify-failed.service` sends the Apprise notice. When the Apprise settings are saved from the TUI, `pxu` also writes `/etc/pxu/failure-notice.cred` (root, 0600): a second credential encrypted with `systemd-creds` under the same name (`pxu-apprise`) with the same URLs/key, holding a ready-to-send `curl --config -` file (Apprise API) or the URL list (CLI). The unit loads it with `LoadCredentialEncrypted=` and `/usr/local/bin/pxu-failure-notice` (a small script installed by `install.sh` and by the TUI schedule installer, not pxu) feeds it to `curl --config -` or `apprise --config /dev/stdin` by stdin: no secret in argv, in the unit or in the journal. Without that file (Apprise never saved from the TUI) `ConditionPathExists=` skips the unit and only the journal entry remains; a config saved before this change needs one save from the TUI to create the credential. When Apprise is turned off (`pxu config disable-apprise`, or saving without a transport) `pxu` deletes that credential first (`Host::remove_failure_notice`, root only) and only then writes the config, so no old secret stays on disk and no unit references it. Queued notices use a second short lock, `pxu-outbox-files.lock`, separate from the flush lock held during network sends; a notice that could not be queued is reported as lost and retried at the next check. Not tested on a real server: delivery to a real Apprise.

`dist/systemd/pxu-notify-flush.service` (`pxu notify-flush`, `Nice=10`, `IOSchedulingClass=idle`, `TimeoutStartSec=5min`) and `pxu-notify-flush.timer` (`OnCalendar=*:0/15`, `Persistent=true`) resend the queued notices; they are installed and enabled together with the update timer.

The TUI's `S` key saves another schedule as a `pxu-update.timer.d` drop-in that replaces `OnCalendar`; `install.sh --uninstall` deletes it. The log of each scheduled run and its retention: [§7](#7-logs).

You can edit the units before installing them:

```sh
cat dist/systemd/pxu-update.service dist/systemd/pxu-update.timer
```

The `-` in front of `check` makes its code 10 ("there are updates") not stop the unit.

### Schedule from the TUI

Without touching `systemctl` by hand: in the Config view (`g`) you see the real state of the timer (installed units, enabled, effective schedule and next run), and `S` (also from the list or the detail) opens the schedule dialog:

- Enable or disable the timer, and choose daily at HH:MM, Monday to Friday at HH:MM, weekly (day + HH:MM) or a free-form `OnCalendar`. Times are the server's.
- Before confirming it validates the schedule with `systemd-analyze calendar` and shows the next run; if it doesn't validate, it writes nothing.
- After confirmation ("today: … → new"; `y`/Enter saves, Esc cancels) it writes the drop-in `/etc/systemd/system/pxu-update.timer.d/schedule.conf` (empty `OnCalendar=` + the new one, 0644, atomic), runs `systemctl daemon-reload` and `enable --now` or `disable --now`.
- If the units are not installed, the dialog installs them (they are the same as in `dist/systemd/`, embedded in the binary).
- It warns if there is no `config.toml` or no CT with auto-update (the update would do nothing) or if the config is invalid (it would fail with code 2). Rejected in forced read-only mode.

`install.sh --uninstall` also deletes that drop-in.

---

## 7. Logs

One log per run: `/var/log/pxu/run-YYYYMMDD-HHMM[SS][-N].log` (UTC time in the name; `update/runlog.rs`).

- It is created when the lock is taken with a minimal header (`pxu-run-log 1`, `started`, `node`, `---` line) and grows while it runs, with one section per host, NVIDIA and CT and the local time of each line. It is also written if the run is stopped or cancelled (with the reason); mode `0600`.
- When it ends it is rewritten entirely: full header (`duration`, `result`), the sections and the same summary as the notification (a single source, `update/report.rs`). A log without `result` (abrupt cut) is shown as `⛔ INCOMPLETE`.
- Retention by days: `[general] log_retention_days` (30; `0` = don't delete). When each run ends, the oldest `run-*.log` are deleted; `prune_in` only touches names in pxu's format. Logs in the previous format (`host-*.log`, `nvidia-*.log`, per-CT folders) also expire by days, but the Logs page only lists `run-*.log`. Empty `<vmid>` folders of the old format are also removed; maximum 3650 days; only regular files with a pxu name are touched (symbolic links are not followed). `logs_per_ct` no longer exists: it is accepted and ignored (obsolete config warning).
- The log masks URLs with user and password (`fmt::mask_url_creds`) and replaces control characters (`fmt::clean`).
- The TUI lists and opens them with the `l` key (up to `LIST_MAX = 1000` logs and `VIEW_MAX_LINES = 100 000` lines); see [DESIGN §5](DESIGN.md#5-screens).
- TUI: `l` opens the **Logs** page (date, duration, result and size, from newest to oldest; ↑↓/jk, click to choose, double click or Enter to open). In the viewer: arrows, PgUp/PgDn and wheel to scroll, `g`/`G` start/end, `n`/`p` next/previous log, `q`/Esc returns to the list with the same log selected. `d` in the Config view edits `log_retention_days`.
- Results shown in the Logs page: `✅ 5 ok`, `⚠️ 3 ok · 1 warn`, `❌` if something failed, `⛔ STOPPED` if the run was stopped, and `⛔ INCOMPLETE` if it was cut off abruptly (for example, the server shut down mid-run) or is still in progress. The list says "no run logs yet" until there has been a run.

---

## 8. Code structure and testing

### 8.1 Tree

The code tree, the dependencies and the build profile live in [DEVELOPMENT](../DEVELOPMENT.md) ("Code map"). The sections below describe the `Host` trait, the test builder and the tests.

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
- `ReadOnlyHost` wraps any host and only lets through commands from an **allowlist**; writes require explicit, scoped opt-ins (CT in progress, host, NVIDIA). Security detail: [SECURITY-REVIEW](SECURITY-REVIEW.md).
- `MockHost` serves a virtual in-memory filesystem and answers commands according to a `Scenario`; it is built by the `World` builder (`share/mock/scenario.rs`). There is no versioned data on disk.

### 8.3 `World` builder and cases

A case declares only what changes (distro, running/stopped, storage with or without snapshot, injected failures, ProxMenux locks, VMs and guests of another node, backup policy). Predefined cases: `demo`, `update-ok`, `update-fail`, `distros`, `cluster`, `failures`, `vms`, `host`. The values are made up, not captures of a server. Each test builds its own with the builder (`World::new().ct(Ct::new(101).distro(Distro::Alpine).stopped())…`).

### 8.4 Tests

- Unit: `.conf` parser, `@@` protocol, parsers of each manager, config, notifications (simulated curl), timer schedule.
- Integration: full `check` and `update` against `MockHost`; the exact sequence of commands is checked, which also proves that read-only mode never emits `apt-get update`/`apk update`/etc. (`check_path_is_read_only`).
- TUI: `TestBackend` captures against `tests/snapshots/`.
- `mise run test` runs the tests in the dev and `mock` profiles; `mise run check` additionally checks that the production binary rejects `--mock`. See [DEVELOPMENT](../DEVELOPMENT.md).

## 9. Storage page (`share/storage.rs`, `tui/storage.rs`)

- `share/storage.rs`: parsers (`parse_storage_list`, `parse_content`, `parse_api_guests`, `parse_lvs`, `parse_storage_cfg`), `read_base` + `read_content` (I/O only; content read in parallel), `build` (pure: raw data + guests → `StorageData`, alerts, sort, filter) and `load` / `load_progressive` (sends a partial `StorageData` first, then the full one). Thresholds are constants: `OLD_BACKUP_DAYS=30`, `FULL_DISK_PCT=98`, `FULL_STORAGE_PCT=90`.
- `tui/storage.rs`: `StorageState` (selection, sort, alert filter, detail) and key handling; drawing lives in `tui/views.rs` (`draw_storage`, `storage_detail_lines`).
- Non-blocking: `tui/mod.rs::spawn_storage` runs `load` in a `std::thread::scope` worker and returns the result over an `mpsc` channel; `Action::OpenStorage` / `RefreshStorage`.
- New reads, all in the `ReadOnlyHost` allowlist (`share/host/readonly.rs`) with argument validation (node name and storage ids are checked, vmids via `is_pve_id`; anything else is rejected, see `storage_page_reads_are_exact_and_validated`):
  - `pvesh get /nodes/<node>/storage --output-format json`
  - `pvesh get /nodes/<node>/lxc --output-format json` and `…/qemu` (`disk`/`maxdisk` per guest: the source of USED/% and of the full-disk alert, same as the Proxmox GUI)
  - `pvesh get /nodes/<node>/storage/<storage>/status --output-format json`
  - `pvesh get /nodes/<node>/storage/<storage>/content --output-format json`
  - `lvs --reportformat json --units b --nosuffix -o vg_name,lv_name,lv_size,data_percent,pool_lv,lv_attr`
  - plus reading `/etc/pve/storage.cfg` and the guests' `.conf` (already readable files).
- No `pct exec`/`df` inside CTs: disk usage comes from `pvesh` only (`disk`/`maxdisk`). `lvs` `Data%` is thin-pool blocks (it includes deleted, untrimmed files), so it is kept apart (`DiskUse.thin_pct`) and only feeds the detail's trim notice, never USED/%, sort or alerts. Stopped guests report `disk = 0`: `—`. A failed read yields `—`, not an error screen.
- Mock: `share/mock/storage.rs` builds a fixture (dir `local`, `local-lvm` lvmthin, `local-zfs`, `remote-backup-3` with dedup) from the scenario via `World::storage_demo()`; `demo` includes a CT without backup, one with an old backup, one at 99% (CT 133), one with thin blocks at 99.8% but real use 66% (CT 131, the trim notice), a stopped one and one with a `pxu-*` snapshot.

