English | [Español](DESIGN.es.md)

# DESIGN — `pxu` TUI

Reference for the current terminal interface (`pxu` with no arguments). It describes what the code does; for the architecture (modules, state, check modes, update state machine, configuration, systemd) see [ARCHITECTURE.md](ARCHITECTURE.md). The screen blocks are literal copies of the snapshots in `tests/snapshots` (80×24); each view also has a 120×40 version. The data in those snapshots comes from the demo (`[MOCK]`), not from a real server.

Code: `src/tui/` — `app.rs` (state and keys), `views.rs` (drawing), `logs.rs` (logs view), `sched.rs` / `apprise.rs` (Config dialogs), `mod.rs` (loop, mouse, saving).

---

## 1. Principles

1. **Dense table, not cards.** One row per guest, `htop`/`k9s` style: state → resources → updates.
2. **Pure reducer.** `App::on_key` (`app.rs:486`) does no IO: it returns an `Action` that the loop executes (`mod.rs`); `views::draw` only draws. That is why every screen is tested with `TestBackend` against the snapshots (`PXU_UPDATE_SNAPSHOTS=1` regenerates them).
3. **Color is never the only signal.** States in words (`run`, `stopped`), step marks (`●` in progress, `✓` ok, `✗` fail, `»` skip, `·` pending), cursor with `>` in addition to reverse video. Works with `NO_COLOR`.
4. **Keyboard first, mouse optional.** Everything is done with the keyboard; each screen declares its keys in the footer. The mouse only acts in the logs view (§4.7).
5. **Expensive things on demand.** The default view lives off files in `/etc/pve` and cgroups; the package check only runs with `c`/`C`.
6. **Every change asks for confirmation.** `u`, `a`, `b`, `S`, `N`, `L` and the host row open a dialog before writing or running anything.

Comfortable minimum: 80×24; the footer (`keybar`) and the columns adapt to the width (§3, §5.1).

## 2. Redraw and sampling

The loop (`mod.rs`, ~380-470) draws on every iteration and then waits for events with `poll`:

- **No active work:** it waits until the next CPU sample. The CPU column is sampled 1 s after opening and then every 3 s (`CPU_FIRST` / `CPU_EVERY`, `mod.rs:369-370`); it reads `cpu.stat` of the running LXCs, without reloading the inventory. It shows `—` when there is no data and for stopped CTs.
- **With a check, a run or a command in progress:** `poll` of at most 150 ms to handle keys and messages from the workers.
- The header clock (`HH:MM`) is updated on every draw.
- **Update run:** the time of the step in progress is updated when a log line arrives or the state changes (footer: `step times update on log lines or state changes`); there is no spinner. Each line of the live log carries its time.

## 3. Palette, symbols and key footer

16 ANSI colors over the terminal background (no background is painted). With `NO_COLOR` there are no colors and the selection stays in reverse video with `>` in the first column.

| Role | Use |
|---|---|
| text | default |
| dimmed | `stopped`, notes, secondary rows |
| green | `run`, `ok`, `✓` |
| cyan | accent: cursor, active filter, step in progress `●`, updates > 0 |
| yellow | warning: security > 0, `warn`, `stale` index |
| red + bold | `FAIL`, `✗`, errors |
| magenta | `VM` type |
| bold | titles and headings |
| reverse video | selected row |

**Key footer (`keybar`, `views.rs:61-81`).** Format `[key] action` separated by two spaces. Keys are left-aligned with 2-space gaps, never justified. All views (`keybar_wrap`) treat their keys as one sequence: on one row if they fit the width, otherwise on the next row, filling each row with every key that fits and never cutting one, with 2 rows at most (if they still do not fit, whole actions are dropped from the end, always keeping the last `keep` ones, those of `?` and `q`). When the list needs only one row, the spare row goes to the table. All views with a footer compose it the same way, from top to bottom: a full-width `─` rule, the status line, another `─` rule and the key rows (one or two key rows, plus the note row in Config and detail). Help takes whatever it needs. The two rules take 2 rows from the table; the visible rows are computed from the remaining height. Header: title + badges (`[RO]`, `[MOCK]`) on the left and clock on the right (`badges`, `views.rs:134-143`). After the header, a context line (filters, summary) and a `─` rule.

Data format: updates `n/s` (total/security), sizes `x/y GB`, index age `5h`, `32d*` (with `*` = stale), durations `0m00s` / `45m 11s`. UI text in English, states in lowercase (`run`, `stopped`, `ok`) and short results (`ok`, `warn`, `FAIL`, `skip`, `canceled`).

## 4. Key map

Priority (`on_key`, `app.rs:486-562`): `Ctrl-C` → modals (update, columns, host, NVIDIA, auto-update, backup, schedule, Apprise, retention) → help `?` → search → view key. A modal captures all keys.

`Ctrl-C` with an active update run opens the cancel dialog (§7); without a run it exits.

### 4.1 List (`list_key`, `app.rs:1015-1064`)

| Key | Action |
|---|---|
| `↑↓` / `j k` | move cursor |
| `PgUp PgDn` | 10 rows · `Home End` first/last |
| `1 2 3 4` | filter All / Running / Stopped / Updates |
| `/` | search by name, tag or id (`Enter` sets, `Esc` clears) |
| `s` | cycles the order: vmid · name · updates · ram |
| `r` | reloads the `.conf` files from `/etc/pve` (files only) |
| `Enter` | guest detail |
| `c` | check with index refresh (config `[check] refresh`; in `[RO]` without refresh) |
| `C` | read-only check (no index refresh), no confirmation |
| `v` | view the last check result (if there is one or one is in progress) |
| `o` | choose visible columns |
| `u` | update the visible CTs with updates (asks for confirmation); on the host row, updates the host |
| `a` | auto-update options of the CT / the host (asks for confirmation): the «Automatic updates» box and, on the host or on a CT with an NVIDIA GPU and a `.run` driver, «Include GPU driver update»; one window, also in the detail |
| `b` | backup before the CT's update: on (follows `[backup] mode`) / `none` (asks for confirmation; not for the host) |
| `d` | Storage page: space per storage, guest, snapshot and backup (read-only) |
| `g` | Config view |
| `S` | timer schedule dialog |
| `l` | run logs |
| `?` / `q` | help / quit · `Esc` clears the search |

### 4.2 Detail (`detail_key`, `app.rs:1196-1223`)

`↑↓/jk` scroll · `PgUp PgDn` 10 · `Home` top · `u` update this guest · `r` reload · `c` check · `a` auto-update · `b` backup · `g` Config · `S` schedule · `?` help · `Esc`/`Backspace` back · `q` quit.

### 4.3 Check result (`check_key`, `app.rs:1139-1172`)

`↑↓/jk` · `PgUp PgDn` · `Home End` · `Enter` CT detail · `c` re-check with index refresh · `C` (or `R`) read-only check · `u` update the CTs with updates · `Esc`/`Backspace` back (with a check in progress, it cancels it) · `?` · `q`.

### 4.4 Update run (`update_key`, `app.rs:932-`)

`x`, `q` or `Esc` with the run active open the cancel dialog · `↑↓/jk` and `PgUp PgDn` scroll the log · `f` or `End` follow the end · `Home` start · `?` help · `u` is disabled during the run. When the run is finished: `Esc`/`Backspace` goes back to the list and `q` quits. There is no alternate focus or `Tab`.

### 4.5 Dialogs

| Dialog | Keys |
|---|---|
| Confirm update (`modal_key`) | `↑↓/jk` move · `Space` include/exclude · `Enter` start · `Esc`/`q` cancel |
| Confirm host update | `↑↓/jk` scroll · `Enter` install · `Esc`/`q`/`n` cancel |
| Auto-update (`a`) | `↑↓/jk` choose box · `Space` check · `Enter` save · `Esc`/`q` cancel |
| Backup (`b`) | `y`/`Enter` save · `Esc`/`n`/`q` cancel |
| Columns (`o`) | `Space` show/hide · `Enter` close |
| Cancel run | `Enter` finish the CT in progress and stop · `Esc` continue |
| Schedule (`S`) | `Tab`/`↓` and `BackTab`/`↑` field · `←/→/Space` change · `Enter` validate · `Esc` cancel; on the confirmation `y`/`Enter` saves, `b`/`Backspace` goes back to editing |
| Apprise (`N`) | same as Schedule |
| Retention (`L`) | digits (up to 5) · `Backspace` · `Enter` save · `Esc` cancel |
| Refresh (`C`) | `y` confirms; any other key cancels |
| Help (`?`) | any key closes; `q` quits |

### 4.6 Config (`config_key`, `app.rs:682-696`)

`s` schedule · `n` notifications (Apprise) · `d` log retention (`S`/`N`/`L` also work, hidden) · `t` test notification · `l` logs · `r` re-read · `?` · `Esc`/`Backspace` back · `q` quit.

### 4.7 Logs (`logs.rs:80-140`)

List: `↑↓/jk` · `PgUp PgDn` (10) · `Home`/`g` first · `End`/`G` last · `Enter` open · `Esc`/`Backspace`/`l` back · `q` quit.
Viewer: `↑↓/jk` · `PgUp PgDn`/`Space` page · `g`/`G` start/end · `n`/`p` next/previous log · `q`/`Esc`/`Backspace` back to the list.
**Mouse** (only in Logs; the loop enables capture on entering and disables it on leaving, `sync_mouse`, `mod.rs:317`): the wheel scrolls 3 lines, a click selects a row and a double click (≤ 500 ms) opens it.

## 5. Screens

### 5.1 Main list

Every column is as wide as its content (at least its header), with 2-space gaps, left-aligned; spare width is never spread. `ID`, `TYPE`, `STATE`, `OS`, `RAM`, `DISK`, `GPU`, `UPD` and `AUTO` measure max(header, longest value among the current guests); `CPU` is fixed (6, or 5 when narrow) because its value changes every second and the table must not jump. `NAME` comes first at the width of the longest name and is only cut with `…` if it does not fit even without `TAGS`; `TAGS` takes what is left up to its longest text, and when less than 6 columns are left it is hidden (header included) instead of cutting `NAME`. If everything fits the rest of the line stays empty. The selection bar spans the whole row. Columns: `ID NAME STATE OS CPU RAM UPD BACKUP AUTO`, plus `TYPE`, `DISK`, `TAGS` and `GPU` (`TAGS` hidden by default) depending on the width and the choice made with `o` (`ID`, `NAME` and `STATE` always; the selection is saved in `config.toml`). `BACKUP` is `yes` if pxu makes a backup before updating that CT (mode auto/snap/vzdump) and `-` if not (mode `none`, VMs and the host); the exact mode is in the detail and in `b`. The list footer (`views.rs:446-451`) and the line under the table (`▼ 21 below …`, summary of the selected guest) change with the selection.

```
pxu [RO] [MOCK]                               server · 34/37 up · 13 upd   00:16
[All 37]  Run 34   Stop 3   Upd 13  / search…
────────────────────────────────────────────────────────────────────────────────
    ID  NAME             STATE    OS              CPU  RAM           UPD    AUTO
   100  ct-100           run      debian 12         —  0.2/0.5 GB    0      —
   101  ct-101           run      debian 12         —  0.3/1.0 GB    4/1    —
   102  ct-102           run      debian 12         —  0.2/0.5 GB    2/1    —
>  103  ct-103           run      debian 12         —  0.3/1.0 GB    7/2    yes
   104  ct-104           run      alpine 3.20       —  0.2/0.5 GB    4/1    —
   105  ct-105           run      fedora 41         —  0.3/1.0 GB    0      yes
   106  ct-106           run      debian 12         —  0.2/0.5 GB    3/1    yes
   107  ct-107           run      debian 12         —  0.3/1.0 GB    4/1    —
   108  ct-108           run      debian 12         —  0.2/0.5 GB    2/1    yes
   109  ct-109           run      alpine 3.20       —  0.3/1.0 GB    0      —
   110  ct-110           run      debian 12         —  0.2/0.5 GB    4/1    yes
   111  ct-111           run      alpine 3.20       —  0.3/1.0 GB    0      —
   112  ct-112           run      alpine 3.20       —  0.2/0.5 GB    3/1    —
   113  ct-113           run      debian 12         —  0.3/1.0 GB    0      —
▼ 23 below (next 114 ct-114)   — filter: all · 37 guests
────────────────────────────────────────────────────────────────────────────────
> 103 ct-103 · debian 12 · 7 updates (2 security) · auto: yes · [Enter] detail
────────────────────────────────────────────────────────────────────────────────
[↑↓] move  [1-4] filter  [/] search  [s] sort  [c] check  [u] update (asks)
[r] reload  [a] auto  [b] bkp  [o] cols  [g] cfg  [l] logs  [?] help  [q] quit
```

**The host row.** Proxmox itself appears as a `vmid 0` row (`GuestKind::Host`) with its package summary in the header (`host: 7 upd`). `u` on it opens its own confirmation (package list grouped by kernel/firmware/pve/security/other, reboot notice, prior copy of `/etc`, no automatic rollback) and it never enters a CT run. `a` on it toggles `[update] host_auto`, the timer's opt-in. Before `c` the host appears as "not reviewed yet: press c".

```
pxu [RO] [MOCK]                    server · 5/6 up · 1 upd · host: 7 upd   00:16
[All 6]  Run 5   Stop 1   Upd 1  / search…
────────────────────────────────────────────────────────────────────────────────
    ID  NAME                   STATE    GPU       CPU  RAM           UPD    AUTO
>    0  server                 run      NV ✓        —  24.4/62.5 GB  7/2    —
   131  ct-131                 run      AMD ✓       —  0.2/0.5 GB    0      —
   134  ct-134                 run      NV ✓        —  0.2/0.5 GB    1/0    —
   135  ct-135                 stopped  NV ?        —  —/0.5 GB      0      —
   140  ct-140                 run      —           —  0.2/0.5 GB    0      —
   150  ct-150                 run      —           —  0.2/0.5 GB    0      —
   200  vm-200                 run      pass        —  —/2.0 GB      —      —








────────────────────────────────────────────────────────────────────────────────
> 0 server · proxmox ve 9.0.11 · 7 updates (2 security) · auto: no · [Enter] de…
────────────────────────────────────────────────────────────────────────────────
[↑↓] move  [1-4] filter  [/] search  [s] sort  [c] check  [u] update (asks)
[r] reload  [a] auto  [b] bkp  [o] cols  [g] cfg  [l] logs  [?] help  [q] quit
```

```
pxu [MOCK]                         server · 5/6 up · 1 upd · host: 7 upd   00:16
[┌ confirm host update ───────────────────────────────────────────────────────┐
─│ Install 7 packages on the Proxmox host (2 security)?                       │─
 │                                                                            │O
>│ kernel (2)                                                                 │
 │   proxmox-kernel-6.17 6.17.9-1 → 6.17.13-2                                 │
 │   proxmox-default-kernel 2.0.1 → 2.0.2                                     │
 │ firmware (1)                                                               │
 │   pve-firmware 3.16-1 → 3.17-2                                             │
 │ pve (1)                                                                    │
 │   pve-manager 9.1.1 → 9.1.4                                                │
 │ security (2)                                                               │
 │   libc6 2.36-9+deb12u9 → 2.36-9+deb12u10                                   │
 │   openssl 3.0.16-1 → 3.0.17-1                                              │
 │ other (1)                                                                  │
 │   htop 3.4.0-1 → 3.4.1-1                                                   │
 │                                                                            │
 │ ! a new kernel: a reboot will be needed (pxu never reboots)                │
 │ /etc is backed up first · no automatic rollback · log in /var/log/pxu      │
─│ [Enter] install  [↑↓/jk] scroll  [Esc] cancel                              │─
>│                                                                            │…
─└────────────────────────────────────────────────────────────────────────────┘─
[↑↓] move  [1-4] filter  [/] search  [s] sort  [c] check  [u] update (asks)
[r] reload  [a] auto  [b] bkp  [o] cols  [g] cfg  [l] logs  [?] help  [q] quit
```

**GPU / NVIDIA.** The `GPU` column (`NV ✓`, `AMD ✓`, `pass`, `—`) appears when there are GPUs; `+drv` after an NVIDIA label (`NV ✓+drv`, `NVIDIA ✓+drv` when wide) means the scheduled updates include the driver (`host_nvidia_auto` / `[[ct]] nvidia_auto`); the column measures its content as any other. The guest detail has a `gpu driver:` line (the host detail, `driver update:` under NVIDIA). There is no separate key or window: the option lives in the `a` window, with two boxes at their saved values: `[ ] Automatic updates` and, only on the host or on a CT with an NVIDIA GPU and a driver installed with the `.run`, `[ ] Include GPU driver update` (with installed/latest version, what the run does and the CT restart). `↑↓/jk` move the focus, `Space` toggles the box, `Enter` saves only what changed, and the `Saves:` line says what will be written (or `nothing changed`, and Enter writes nothing). The driver depends on auto-update: with «Automatic updates» unchecked the driver box is dimmed and cannot be checked, and unchecking auto-update also unchecks the driver (turns that machine's driver off, as before). Checking the driver on a CT also turns `host_nvidia_auto` on (the CTs get the libraries of the host's version); unchecking it on a CT only turns off that CT; unchecking it on the host turns off `host_nvidia_auto` and the window warns that the CTs with `nvidia_auto` get nothing while it is off. If the machine does not apply there is no box and one line gives the reason (`no NVIDIA GPU` / `driver not installed with the .run`); if the NVIDIA index cannot be read the box is shown anyway (`unknown`). The timer installs the `.run` driver after updating the host, always without rebooting.

```
Automatic update options of CT 134 ct-134

today: yes by [[ct]] override

> [x] Automatic updates
  [x] Include GPU driver update (NVIDIA)
      Installed: 595.112.07 · Latest: 595.120.01 (newer, same 595 branch)
      the run installs the newest 595.x with DKMS after the host update; GPU CTs get its libs
      this CT restarts after getting the libs
      skipped if a host reboot is pending; pxu never reboots the host

nothing changed: Enter writes nothing          (driver unchecked: "Saves: [[ct]] 134 nvidia_auto = false (host flag untouched)")

[↑↓] move  [Space] check  [Enter] save  [Esc] cancel
```

**Columns (`o`):**

```
pxu [RO] [MOCK]                               server · 34/37 up · 13 upd   00:16
[All 37]  Run 34   Stop 3   Upd 13  / search…
────────────────────────────────────────────────────────────────────────────────
    ID  NAME             STATE    OS              CPU  RAM           UPD    AUTO
>  100  ct-100   ┌ columns ───────────────────────────────────┐GB    0      —
   101  ct-101   │ ID, NAME and STATE are always shown        │GB    4/1    —
   102  ct-102   │                                            │GB    2/1    —
   103  ct-103   │> [x] TYPE                                  │GB    7/2    yes
   104  ct-104   │  [x] OS                                    │GB    4/1    —
   105  ct-105   │  [x] CPU                                   │GB    0      yes
   106  ct-106   │  [x] RAM                                   │GB    3/1    yes
   107  ct-107   │  [x] DISK                                  │GB    4/1    —
   108  ct-108   │  [ ] TAGS                                  │GB    2/1    yes
   109  ct-109   │  [x] GPU                                   │GB    0      —
   110  ct-110   │  [x] UPD                                   │GB    4/1    yes
   111  ct-111   │  [x] AUTO                                  │GB    0      —
   112  ct-112   │                                            │GB    3/1    —
   113  ct-113   │ [Space] show/hide  [Enter] close           │GB    0      —
▼ 23 below (next └────────────────────────────────────────────┘
────────────────────────────────────────────────────────────────────────────────
> 100 ct-100 · debian 12 · 0 updates (0 security) · auto: no · [Enter] detail
────────────────────────────────────────────────────────────────────────────────
[↑↓] move  [1-4] filter  [/] search  [s] sort  [c] check  [u] update (asks)
[r] reload  [a] auto  [b] bkp  [o] cols  [g] cfg  [l] logs  [?] help  [q] quit
```

**Help (`?`).** Text of `draw_help` (`views.rs:1040-1085`), specific to each view:

```
px┌ help · any key closes ───────────────────────────────────────────────────┐16
[A│↑↓ / j k     move cursor                                                  │
──│PgUp PgDn    move 10 · Home End first/last                                │──
  │1 2 3 4      filter All / Running / Stopped / Updates                     │TO
> │/            search name, tag or id (Esc clears)                          │
  │s            cycle sort: vmid · name · updates · ram                      │
  │r            reload /etc/pve confs (files only)                           │
  │Enter        guest detail                                                 │s
  │c            check LXC updates (visible CTs, or all): refreshes indexes   │
  │C            read-only check: does not refresh the indexes                │s
  │v            show the last check result                                   │s
  │o            choose visible columns (Space toggles; saved in config.toml) │
  │u            update the visible CTs with updates (asks first)             │s
  │             temporary backup, verify, auto-rollback on failure           │
  │a            toggle scheduled auto-update of the CT (asks first)          │s
  │             host row: host_auto · host/GPU CT: also the GPU driver box   │
  │b            toggle the backup before updating the CT (asks first)        │
  │g / S        config view + timer state · S change schedule (asks first)   │
  │l            run logs (one file per update run)                           │
▼ │?            this help · q quit                                           │
──│Columns: UPD pending updates (all/security) · — no data yet (press c)     │──
> │AUTO yes = in the scheduled run · host row: u asks before updating        │
──│                                                                          │──
[↑│read-only: no command that writes can run                                 │
[r└──────────────────────────────────────────────────────────────────────────┘
```

### 5.2 Detail

Two panels: configuration read from the `.conf` (left) and state/updates (right, from 120 columns). At 80 columns only the configuration panel remains. For a VM the detail is informational only: pxu does not update VMs.

```
pxu · guest 103 · ct-103 [RO] [MOCK]              LXC · server · running   00:16
detail · config (left) · state & updates (right)
────────────────────────────────────────────────────────────────────────────────
┌ config · /etc/pve/nodes/server/lxc/103.conf ─────────────────────────────────┐
│vmid 103 · hostname ct-103 · type LXC · node server                           │
│state running · ostype debian · arch amd64                                    │
│unprivileged 1 · onboot 1                                                     │
│tags apps,auto · template no                                                  │
│                                                                              │
│resources                                                                     │
│  cores 4                                                                     │
│  memory 1024 MB · swap 512 MB                                                │
│  rootfs local-zfs:subvol-103-disk-0 · 5.0 GB                                 │
│  mp0 local-zfs:subvol-103-mp0 → /mnt/data · 20.0 GB · backup=1               │
│                                                                              │
│network                                                                       │
│  net0 name=eth0 bridge=vmbr0 ip=192.168.1.29/24 firewall=1                   │
│       hwaddr BC:24:11:2A:67:D1 · gw 192.168.1.1                              │
└──────────────────────────────────────────────────────────────────────────────┘
────────────────────────────────────────────────────────────────────────────────
ct-103 · read from /etc/pve/nodes/server/lxc/103.conf
────────────────────────────────────────────────────────────────────────────────
[↑↓/jk] scroll  [u] update (asks)  [r] reload  [a] auto  [b] backup  [?] help  [q] quit
config is read from the .conf file (no processes); runtime metrics come from cg…
```

### 5.3 Check result

Table `ID NAME PM INDEX UPD SEC REBOOT RESULT` and, below it, the package panel of the selected CT. The footer shows `a check is already running` while it runs.

```
pxu · check [RO] [MOCK]            read-only · finished 00:16 · took 0 s   00:16
34 checked · 3 skipped · 13 with updates · 45 packages (14 security) · 1 stale …
     ID  NAME            PM      INDEX  UPD  SEC  REBOOT  RESULT
>   100  ct-100          apt        5h    0    0  no      ok
    101  ct-101          apt        5h    4    1  no      ok
    102  ct-102          apt        5h    2    1  no      ok
    103  ct-103          apt        5h    7    2  no      ok
    104  ct-104          apk        5h    4    1  no      ok
    105  ct-105          dnf        5h    0    0  no      ok
    106  ct-106          apt        5h    3    1  no      ok
    107  ct-107          apt        5h    4    1  no      ok
    108  ct-108          apt        5h    2    1  no      ok
    109  ct-109          apk        5h    0    0  no      ok
 packages · 100 ct-100 · apt ───────────────────────────────────────────────────
up to date





────────────────────────────────────────────────────────────────────────────────
a check is already running
────────────────────────────────────────────────────────────────────────────────
[↑↓] select  [Enter] detail  [c] refresh+check  [C] read-only  [?] help  [q] quit
```

### 5.4 Update run

Queue with a mark per CT (`✓ ● · ✗ »`), counters in the header and live log. If there is a notice, it is **only one per run**: the log line `NOTE: the backup and the rollback cover only the CT's rootfs` and the `WARNING` of the confirmation modal (§5.5).

Every non-interactive `pxu update` run that reaches the pre-check (the scheduled one included) leaves its `/var/log/pxu/run-….log`, even with nothing to update: it takes the global lock like any run, lists each skipped CT and the host with its reason (`no updates pending`, old check…) and its header says `result: ✅ nothing to do`. The Logs page shows it with that result (not INCOMPLETE or failed), retention prunes it like the rest and it sends no notification. With `host_auto` on and no host updates, the log and the terminal say `host: skipped — no updates pending`; with it off the host is not mentioned. The summary has no empty table, only `Nothing to do: no CT or host had pending updates.`, and the terminal counts as skipped the auto-update CTs with nothing pending (one line, not one per CT). `--json` carries `log_path`.

```
pxu · update run [MOCK]                       started 00:16 · sequential   00:16
ok 1 · warn 0 · fail 0 · skip 0 · canceled 0 · CT 1/3
── queue ───────────────────────────────────────────────────────────────────────
✓ 401   ct-401           ok                0m00s  3 → 0 package(s) pending
● 402   ct-402           upgrade           00:00
· 403   ct-403           pending
── live log · 402 ct-402 ───────────────────────────────────────────────────────
 00:16:00  402 $ vzdump 402 --mode snapshot --storage local --compress zstd --z…
 00:16:00  402 INFO: creating vzdump archive '/var/lib/vz/dump/vzdump-lxc-402-2…
 00:16:00  402 INFO: backup finished
 00:16:00  402 backup ready: vzdump local:backup/vzdump-lxc-402-2026_11_08-03_1…
 00:16:00  402 NOTE: the backup and the rollback cover only the CT's rootfs (sy…
 00:16:00  402 == refresh+simulate
 00:16:00  402 $ (refresh index + simulate)
 00:16:00  402 3 package(s) to upgrade
 00:16:00  402 == upgrade
 00:16:00  402 $ [CT 402] apk upgrade --no-cache
 00:16:00  402 Reading package lists...
 00:16:00  402 Calculating upgrade... Done
────────────────────────────────────────────────────────────────────────────────
cancel: [x] → finish current guest (402), then stop · killing dpkg mid-install …
step times update on log lines or state changes · log: /var/log/pxu/run-*.log
────────────────────────────────────────────────────────────────────────────────
[x] cancel  [↑↓] scroll log  [f] follow  [?] help
```

Run finished with failures (the circuit breaker stopped the queue after 2 consecutive failures):

```
pxu · update run [MOCK]                            started  · sequential   00:16
ok 0 · warn 0 · fail 2 · skip 2 · canceled 0 · CT 4/4 · finished
── queue ───────────────────────────────────────────────────────────────────────
✗ 421   ct-421           FAIL              0m00s  apt-get exited with 100; roll…
✗ 422   ct-422           FAIL              0m00s  backup failed, CT untouched: …
» 423   ct-423           skip                  —  not started: stopped after 2 …
» 424   ct-424           skip                  —  not started: stopped after 2 …
── live log ────────────────────────────────────────────────────────────────────
 00:16:00  422 $ [CT 422] df -Pk /
 00:16:00  422 Filesystem 1024-blocks Used Available Capacity Mounted on
 00:16:00  422 /dev/loop0 4000000 1000000 3000000 25% /
 00:16:00  422 $ [CT 422] systemctl --failed --no-legend --plain
 00:16:00  422 == backup
 00:16:00  422 $ pvesh get /nodes/server/lxc/422/feature --feature snapshot --o…
 00:16:00  422 {"hasFeature":0}
 00:16:00  422 snapshot not used (PVE reports no snapshot support for this CT);…
 00:16:00  422 $ vzdump 422 --mode snapshot --storage local --compress zstd --z…
 00:16:00  422 ERROR: vzdump failed
 00:16:00  422 == result: Failed: backup failed, CT untouched: vzdump exit 255
────────────────────────────────────────────────────────────────────────────────
run stopped: stopped after 2 consecutive failures · notify: run_summary: sent v…
step times update on log lines or state changes · log: /var/log/pxu/run-*.log
────────────────────────────────────────────────────────────────────────────────
[Esc] back  [↑↓] scroll log  [f] follow  [?] help  [q] quit
```

### 5.5 Confirmation before updating

Lists the CTs with updates included by default (`Space` excludes them), shows the plan per guest and the backup scope: backup and rollback cover only the rootfs; bind mounts / `mp` with `backup=0` are neither saved nor reverted (ARCHITECTURE §3).

```
pxu [MOCK]                                       server · 2/3 up · 3 upd   00:16
[All 3]  Run 2   Stop 1   Upd 3  / search…
────────────────────────────────────────────────────────────────────────────────
    ID  NAME             STATE    OS              CPU  RAM           UPD    AUTO
>┌ confirm ───────────────────────────────────────────────────────────────────┐
 │ Update 3 guests?                                                           │
 │                                                                            │
 │ order vmid ↑ · backup: auto (keep 0) · stopped: start + restore            │
 │ on failure: continue (circuit-breaker after 2 in a row) · reboot: never    │
 │ WARNING: backup + rollback cover only each CT's rootfs; bind mounts / mp w…│
 │                                                                            │
 │>✓   401 ct-401             3 pkgs (1 sec)                                  │
 │ ✓   402 ct-402             3 pkgs (1 sec)                                  │
 │                                                                            │
 │ per guest: pre-checks (check ≤ 60 m, free ≥ 512 MB) → lock → backup → refr…│
 │ → upgrade (nice 10, io idle) → verify → delete backup; on failure: automat…│
 │ [Space] include/exclude  [Enter] start  [Esc] cancel                       │
 │                                                                            │
 └────────────────────────────────────────────────────────────────────────────┘
────────────────────────────────────────────────────────────────────────────────
> 401 ct-401 · debian 12 · 3 updates (1 security) · auto: yes · [Enter] detail
────────────────────────────────────────────────────────────────────────────────
[↑↓] move  [1-4] filter  [/] search  [s] sort  [c] check  [u] update (asks)
[r] reload  [a] auto  [b] bkp  [o] cols  [g] cfg  [l] logs  [?] help  [q] quit
```

### 5.6 Config, schedule and Apprise

Shows the state of the `pxu-update` timer (units, active, schedule, next run), the config (`/etc/pxu/config.toml`), CTs with auto-update, `host_auto`, log retention and the Apprise state. `s` (or `S`) changes the schedule (writes a systemd drop-in), `n` edits Apprise (secrets are not shown and are saved encrypted with systemd-creds; a plain-text secret from an old config is flagged `plain text` with a note to save it in `n`), `d` the retention days (0 = never delete), `t` sends a test notification. The timer runs `pxu check --notify` and then `pxu update --yes`.

```
pxu · config [MOCK]                                                        00:16
config · automatic update schedule (systemd timer pxu-update) and notifications…
────────────────────────────────────────────────────────────────────────────────
┌ Schedule ────────────────────────────────────────────────────────────────────┐
│ units installed : yes                                                        │
│ enabled         : yes (state: active)                                        │
│ schedule        : Mon..Fri *-*-* 03:00                                       │
│ next run        : Tue 2026-11-10 04:35:12 -03                                │
│ times are the server's                                                       │
│                                                                              │
│ config          : /etc/pxu/config.toml                                       │
│ CTs with auto-update: 8                                                      │
│ host_auto       : no — the scheduled run never touches the host              │
│ log retention   : 30 days (run logs in /var/log/pxu older than this are dele…│
│                                                                              │
│ Notifications (Apprise)                                                      │
│ status          : configured, sends via apprise-api (curl)                   │
│ api_url         : http://apprise.local:8000                                  │
│ api_key         : ••• (set, encrypted)                                       │
│ tag             : system                                                     │
└──────────────────────────────────────────────────────────────────────────────┘
────────────────────────────────────────────────────────────────────────────────
[s] schedule  [n] notifications  [d] log days  [t] test notice
────────────────────────────────────────────────────────────────────────────────
[s] schedule  [n] notifications  [d] log days  [l] logs  [t] test  [r] re-read  [Esc] back  [?] help  [q] quit
the timer runs `pxu check` then `pxu update --yes` (host first if host_auto; th…
```

Apprise editing:

```
pxu · config [MOCK]                                                        00:16
config · automatic update schedule (systemd timer pxu-update) and notifications…
────────────────────────────────────────────────────────────────────────────────
┌ Schedule ────────────────────────────────────────────────────────────────────┐
│ units installed : yes                                                        │
│┌ Notifications (Apprise) ───────────────────────────────────────────────────┐│
││ > API address              http://apprise.local:8000_                      ││
││   Sends with               (•) saved key in Apprise API  ( ) urls          ││
││   API key                  (stored — leave empty to keep)                  ││
││   Tag                      system_                                         ││
││   Notify on                [x] updates_found                               ││
││                            [x] update_failed                               ││
││                            [x] run_summary                                 ││
││                            [x] check_error                                 ││
││                                                                            ││
││ secrets are never shown (dots) and are saved encrypted (systemd-creds)     ││
││ tag empty: only untagged targets of the key are notified                   ││
││                                                                            ││
││ [Tab] field  [←/→/Space] change  [Enter] validate  [Esc] cancel            ││
│└────────────────────────────────────────────────────────────────────────────┘│
└──────────────────────────────────────────────────────────────────────────────┘
────────────────────────────────────────────────────────────────────────────────
[s] schedule  [n] notifications  [d] log days  [t] test notice
────────────────────────────────────────────────────────────────────────────────
[s] schedule  [n] notifications  [d] log days  [l] logs  [t] test  [r] re-read  [Esc] back  [?] help  [q] quit
the timer runs `pxu check` then `pxu update --yes` (host first if host_auto; th…
```

The confirmation (`Enter` in the window above) shows one block per state, one field per line with aligned labels and a blank line between sections; in `new` the fields that change are highlighted (bold). It always stays inside the content area of Config (it never covers the footer): when it is taller than the space, it drops first the blank lines (the top one, then the ones between blocks), then the «the rest of…» note, and finally cuts the end with «…». Notes that do not fit in one line continue with a hanging indent. At 80x24 the real case (urls → api_key, with tag) fits with no blank lines (drawing at 120x40):

```
┌ Save the notifications? ────────────────────────────────────────────────────┐
│                                                                             │
│ today                                                                       │
│   api_url   http://192.168.1.28:8000                                        │
│   urls      ••• (encrypted)                                                 │
│   tag       system                                                          │
│   on        update_failed, run_summary                                      │
│                                                                             │
│ new                                                                         │
│   api_url   http://192.168.1.28:8000                                        │
│   api_key   ••• (encrypted)                                                 │
│   tag       system                                                          │
│   on        update_failed, run_summary                                      │
│                                                                             │
│ · the stored urls are removed (the key and the urls exclude each other)     │
│ · stored encrypted (systemd-creds); no plain-text copy stays in the file    │
│ · the rest of /etc/pxu/config.toml is kept as it is                         │
│                                                                             │
│ [y/Enter] save  [b] back  [Esc] cancel                                      │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 5.7 Logs

`l` from List or Config opens the list of run logs (newest first, one row per file in `/var/log/pxu`: date, duration, result, size); `Enter` opens the viewer.

```
pxu · log [RO] [MOCK]                                                      00:16
run-20261104-0300.log · ❌  3 ok · 1 warn · 2 failed · 45m 11s
────────────────────────────────────────────────────────────────────────────────
line 1
line 2
line 3
line 4
line 5
line 6
line 7
line 8
line 9
line 10
line 11
line 12
line 13
line 14
line 15
line 16
line 17
────────────────────────────────────────────────────────────────────────────────
lines 1-17 of 100
────────────────────────────────────────────────────────────────────────────────
[↑↓/PgUp/PgDn] scroll  [g/G] top/end  [n/p] next/prev  [q/Esc] back  [?] help
```

### 5.8 Storage

Opened with `d` from the list (the host row is not required). Read-only: it only runs allowlisted reads and never enters a CT.

```
pxu · storage [RO]                                                    00:16
node server · 4 storages · 37 guests
── Storages ──────────────────────────────────────────────────────────────
 local-lvm  lvmthin [█████░░░░░░░] 16.4G/40.0G  41%  alloc 25.0G (63%)
 remote-backup-3   pbs     [█░░░░░░░░░░░] 26.6G/2.0T   1%  dedup 4.2× · 47 bk · 112G logical
── Guests · sort: used ↓ ─────────────────────────────────────────────────
   ID NAME     STATE    USED/ALLOC    % SNAPS LAST BACKUP BK SIZE BACKUP ALERT
! 133 ct-133   running   4.9G/5.0G  99%     0 2026-09-24     3.0G yes    last backup 44 d ago, disk 99% full
```

- **Storages**: bar, used/total, %, plus info: templates and backups on `dir`, `alloc` (sum of the thin volumes, only for `lvmthin`), `N backups · X logical · dedup N.N×` on PBS (logical = sum of the backup contents; dedup = logical / used) and `pxu pre-update target` on the storage that receives pre-update backups. What cannot be read shows `—`.
- **Guests**: one row per CT/VM: disk used/allocated and % (the numbers Proxmox shows: `disk`/`maxdisk` of `pvesh get /nodes/<n>/lxc|qemu`; a stopped guest, or a VM without an agent, reports `disk = 0`, so it shows `—`, never 0% and never an alert), snapshots (`N*` = one is a `pxu-*` snapshot), last backup, its size, BACKUP (same as the main list: `yes` if pxu backs up before updating, `-` if not; the backup's storage is only in the detail) and an alert tag. Last backup ignores pxu pre-update leftovers.
- **Alerts** (fixed thresholds, `share/storage.rs`): no backup · last backup older than 30 days · guest disk ≥ 98% (only from Proxmox's `disk`/`maxdisk`) · thin pool overcommitted or > 90% · the pxu target storage lacks room for the largest CT (same rule as the update space check). A summary line counts them.
- **Why not `lvs`**: the `Data%` of a thin LV counts every block ever written to the pool, deleted files included, until a trim returns them; it can read 99.8% on a disk Proxmox shows at 66%. So it is never used as the guest's used/%, in sort or in alerts. It only appears in the detail as `thin blocks 99.8% (20.0G) — reclaim with \`pct fstrim <id>\``, when it exceeds the real use by more than 15 points (`UNTRIMMED_GAP_PCT`) or the real use is unknown. pxu only reads: it never runs `fstrim`.
- **Detail** (`Enter`): disks and mount points (volid, used/alloc; the rootfs usage comes from Proxmox), snapshots (`pxu-*` marked), backups across every backup storage (date, size, storage, volid; `(pxu pre-update)` marks pre-update backups), and the effective pre-update mode with its origin (CT override / global / default).
- **Load**: a worker thread reads in the background; the screen shows "loading…" and never blocks. Two phases: storages, `lvs` and guests first (backups show `…`), then each storage's `content`, read in parallel. `r` reloads. Works with 0 backups, inactive storages and VMs.

| Key | Action |
|---|---|
| `↑↓` / `j k`, `PgUp PgDn`, `Home End` | move |
| `Enter` | guest space detail |
| `S` | rotate sort: used · alloc · % · last backup · id |
| `f` | only guests with alerts |
| `r` | reload |
| `?` / `q` / `Esc` | help / back |

## 6. Errors

A single place: the **status line** (between the two footer rules), which is cleared by the next key. Config errors are persistent and force read-only mode (§8). Real messages (`app.rs`): `updates disabled in read-only mode (no valid config)`, `a run is already in progress`, `no CT with a positive check here: run [c] first`, `host not reviewed yet: press c`, `host update blocked: …`, `host: nothing to update`, `auto-update applies to local LXC and the host only`. Errors do not open dialogs; only confirmations are modal.

## 7. Cancellation

`x` (also `q`, `Esc` or `Ctrl-C`) during a run opens **Cancel run?**:

```
pxu · update run [MOCK]                       started 00:16 · sequential   00:16
ok 1 · warn 0 · fail 0 · skip 0 · canceled 0 · CT 1/3
── queue ───────────────────────────────────────────────────────────────────────
✓ 401   ct-401           ok                0m00s  3 → 0 package(s) pending
● 402   ct-402           upgrade           00:00
· 403   ct-403           pending
── live log · 402 ct-402 ───────────────────────────────────────────────────────
 00:16:00  402 $ vzdump 402 --mode snapshot --storage local --compress zstd --z…
 00:16:┌ Cancel run? ───────────────────────────────────────────────────┐-402-2…
 00:16:│ [Enter] finish CT 402 (upgrade → verify → restore), then stop  │
 00:16:│ [Esc] keep running                                             │8-03_1…
 00:16:│                                                                │fs (sy…
 00:16:│ dpkg/apt is never killed mid-install · pending CTs become cance│
 00:16:│                                                                │
 00:16:└────────────────────────────────────────────────────────────────┘
 00:16:00  402 == upgrade
 00:16:00  402 $ [CT 402] apk upgrade --no-cache
 00:16:00  402 Reading package lists...
 00:16:00  402 Calculating upgrade... Done
────────────────────────────────────────────────────────────────────────────────
cancel: [x] → finish current guest (402), then stop · killing dpkg mid-install …
step times update on log lines or state changes · log: /var/log/pxu/run-*.log
────────────────────────────────────────────────────────────────────────────────
[x] cancel  [↑↓] scroll log  [f] follow  [?] help
```

`Enter` sets stop-after: the CT in progress finishes its cycle (upgrade → verify → restore) and the pending ones become `canceled`. **`dpkg`/`apt` are never killed midway through an installation.** **While the CT is still in pre-checks or the vzdump backup (before the install), `Enter` stops at once**: pxu sends SIGTERM to `vzdump` (SIGKILL of its group after 120 s), vzdump removes its partial file, its snapshot LV and the CT lock, the CT is marked `canceled` and left untouched, and the bar reads `cancel: [x] → stop now: CT <vmid> is before the install, its backup is aborted and cleaned up, CT untouched`. From the install step on, the bar and the dialog keep the text above (finish the guest, then stop). The run still emits its `run_summary` via Apprise. In a check, `Esc` cancels the probe in progress and those already done keep their result.

## 8. Read-only mode

`[RO]` badge in the header. It is enabled when the config is missing or invalid (wrong permissions, unknown keys or values); `pxu update` in that case exits with code 2. Effects: `u` is disabled (`updates disabled in read-only mode (no valid config)`). Still available: listing, detail, check `c`, search, order, columns, logs and the test notification. `a` and `b` can create the missing config (the modal says so) and on saving it leaves read-only. Internally there is a forced mode (`forced_ro`) that also rejects `a` and `b`; no command-line option enables it today (`main.rs` passes `false`).

## 9. Unverified limit

The NVIDIA window flow and the host update are covered by tests and snapshots, but have not been run against a real Proxmox.
