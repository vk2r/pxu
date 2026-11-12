# Security Review

English | [Español](SECURITY-REVIEW.es.md)

Security status of the current code: attack surface, threat model and findings with their status. Each status has been checked against the code (file:line or test name). Architecture references are in [ARCHITECTURE](ARCHITECTURE.md) and interface references in [DESIGN](DESIGN.md).

Revision: committed on 2026-11-09 (`f726c66`, initial commit). The date and commit of the original review were not recorded because the git history was flattened; the statuses below were not re-checked against this commit when the documentation was reorganized. Update 2026-11-12: the Apprise secrets are now stored encrypted (rows below and [ARCHITECTURE §4.6](ARCHITECTURE.md)); this was not part of the original review, and the `file:line` references of the findings may have drifted.

Statuses: **fixed** (the cause no longer exists), **mitigated** (the risk is reduced but a described residue remains), **accepted** (a conscious decision), **open** (not fixed).

---

## 1. Surface and threat model

**What runs and with what privileges.** `pxu` runs as root on the Proxmox host: by hand (TUI or CLI) or from the systemd timer (`dist/systemd/pxu-update.service`: `pxu check --notify` and then `pxu update --yes`, with `Nice=10`, `IOSchedulingClass=idle`, `KillMode=mixed`). There is no daemon and no listening port. A single package: library (`share/`) plus CLI and TUI.

**What it touches.**

| Area | What it does | How it is limited |
|---|---|---|
| PVE reads | Reads `/etc/pve/**.conf`, cgroups and pidfiles | File reads only; no `pct list`/`qm list` |
| CT probe | A constant `sh -c` (`probe.sh`) per CT via `lxc-attach` | `ReadOnlyHost` validates the inner argv (`readonly.rs:86`); without refresh it writes nothing |
| CT update | Backup (snapshot/vzdump), upgrade, verification and rollback | Explicit opt-in (`UpdateOptIn`, RAII) and writes bound to the CT in progress (`readonly.rs:120`) |
| Proxmox host update | Host `apt-get full-upgrade`, copy of `/etc` to `/var/backups/pxu` and ZFS snapshot if the root is ZFS; does not reboot | Its own opt-in (`HostUpdateOptIn`), separate from the CT one; exact allowlist (`hostupdate.rs:74`, `:85`); by hand it requires a TTY, unattended only with `[update] host_auto = true` |
| NVIDIA driver | Downloads the `.run` from `download.nvidia.com`, checks its `sha256`, installs it on the host and puts the libraries into the CTs with `nvidia_auto` | Opt-ins `host_nvidia_auto` / `nvidia_auto` (off by default); a grant (`nvidia::Grant`) with a specific version, kernel and vmid; only upgrades within the same major branch; curl with `--proto =https` (`nvidia.rs:30`) |
| Notifications | `curl --config -` (Apprise API) or the `apprise` CLI | Configuration and secrets via stdin, never in argv; fixed `proto` and `max-filesize` (`notify.rs:163`). `api_key` / `urls` are stored encrypted (`systemd-creds`, `api_key_enc` / `urls_enc`) and decrypted only right before sending; if it cannot be decrypted it fails, never falls back to plain text. Root on the same host can still decrypt (ARCHITECTURE §4.6) |
| Own state | `/var/lib/pxu`, `/var/log/pxu` (run logs), `/etc/pxu/config.toml` (from the TUI), timer drop-in | `own_state_path` (`readonly.rs:149`), atomic write with `O_EXCL\|O_NOFOLLOW` (`real.rs:158`), `write_config` and `apply_schedule` only with their opt-in |

**Trust boundaries.**

- **CT → host.** All CT output is untrusted data: it is sanitized with `fmt::clean` before being displayed, logged or sent (H1).
- **Config and registry → actions.** `/etc/pxu/config.toml` decides what gets updated and holds the Apprise secrets (encrypted when saved from the TUI; old plain-text ones keep working and `config check` flags them): root and `0600` or it is not used (`config.rs:296`). `/var/lib/pxu/backups.json` decides which backups belong to `pxu` and is validated on load (M4).
- **Unprivileged local users.** They can only affect the locks in `/run/lock` (L3). There is no data path from them into root commands.
- **Network.** Only `curl` calls to the configured Apprise API and the NVIDIA downloads go out, and the latter only happen with opt-in.

**Design guarantees** (tested): no `sh -c` on the host (every `Command` uses argv); the production binary does not carry `MockHost` (M2); no write command passes without opt-in (`without_the_opt_in_the_timer_never_touches_nvidia`, `host_writes_need_their_own_optin_and_ct_optin_is_not_enough`, `without_explicit_opt_in_nothing_can_be_written`, `check_path_is_read_only`); `pxu` never kills `apt`/`dpkg` midway (H2, `no_kill_on_timeout` also on host and NVIDIA).

**Known limits.**

- The `pct`, `vzdump`, `pvesm` and `lxc-attach` commands are tested with `MockHost`, not against every storage type of a real Proxmox.
- The NVIDIA download is verified with the `sha256` published by the same NVIDIA server; there is no independent signature.
- Dependency CVEs have not been reviewed with an automated tool. The tree has no HTTP or TLS client of its own (only `ratatui`, `crossterm`, `serde`, `toml`, `toml_edit`, `serde_json`, `lexopt`, `libc`; `Cargo.toml`).

---

## 2. Findings

| ID | Sev. | Finding | Status | Evidence in the code |
|---|---|---|---|---|
| H1 | High | CT output was rendered unsanitized: escape sequences in the root TUI, in logs and in notifications | fixed | `fmt::clean` (`fmt.rs:17`) applied in `parse_probe` (`check.rs:52`), `CtResult::detail` (`check.rs:115`), `Ctx::say` (`update/mod.rs:276`), `trunc` (`fmt.rs:49`), report and viewer (`report.rs:32`, `runlog.rs:413`). Tests: `probe_output_cannot_carry_escape_sequences_into_the_ui`, test of `clean` in `fmt.rs:90` |
| H2 | High | The upgrade timeout killed `apt-get`/`dpkg` midway | fixed | `no_kill_on_timeout: true` in the upgrade (`update/mod.rs:658`), in the host (`hostupdate.rs:246`) and in NVIDIA (`nvidia.rs:278`); `real.rs:67` sends no signal in that case. On expiry: CT failed, backup `held`, run stopped. Tests: `upgrade_is_never_run_with_a_killing_timeout`, `upgrade_timeout_marks_ct_failed_and_stops_the_run`, `scenario_timeout_never_rolls_back_and_holds_the_backup` |
| M1 | Medium | ProxMenux's lock reservation (file contents) and its invalid-check marker were ignored | fixed | `lock_ct` (`update/mod.rs:548-577`): file with contents ⇒ skip, `proxmenux-lxc-check-invalid-<id>` ⇒ skip. Test: `proxmenux_reservation_and_invalid_marker_skip_the_ct`. Residue: it does not interpret the UPID; with any contents it skips the CT (fails closed) |
| M2 | Medium | A `--features mock` build could overwrite `target/release/pxu` | fixed | `compile_error!` if release is built with `mock` (`main.rs:27-28`); the release without the feature answers `this build has no mock support` and exits with 2 (`main.rs:21-22`); the `mise run check` task verifies it (`mise.toml`) |
| M3 | Medium | The TUI in `[RO]` allowed refreshing indexes inside the CTs | fixed | `start_check` forces a read-only check in `[RO]` (`c` no longer refreshes there). Test: `in_read_only_c_checks_without_refreshing_and_says_so` (`tui/tests.rs`) |
| M4 | Medium | The `update` write list was not bound to the CT in progress and the backup registry was not validated | mitigated | Bound to the CT: `host_cmd_is_update(argv, vmid)` (`readonly.rs:120`), `set_update_target` + `TargetScope` (`update/mod.rs:489,507`); `pvesm free` and `pct restore` require the vmid in the volid (`readonly.rs:111`). Registry: `entry_check` (`backup.rs:76`) discards entries that do not match. Tests: `update_writes_are_bound_to_the_ct_in_progress`, `tampered_registry_entries_are_dropped_and_never_reach_pvesm_free`. Residue: owner/mode of `backups.json` is not checked on load; it is protected by the root-owned `0700` directory (`real.rs:116`) |
| M5 | Medium | After a timeout on a CT that `pxu` started, `pct shutdown` could cut `dpkg` | fixed | `update/mod.rs:519-521`: with `hold` the CT stays on and a warning is shown. Test: `timeout_on_a_ct_started_by_pxu_never_shuts_it_down` |
| L1 | Low | `perm_problem` failed open if `stat` failed | fixed | `config.rs:298-301` returns the problem (`cannot stat`). Test: `update_opt_in_is_revoked_on_every_exit` covers loading with `cannot stat` (`update/mod.rs:1192`) |
| L2 | Low | The permission check followed symlinks | mitigated | `file_meta` uses `symlink_metadata` and rejects symlinks (`real.rs:281-286`). Residue: it is not checked that `/etc/pxu` is owned by root without group/other write |
| L3 | Low | Locks in `/run/lock` (1777) opened with `create` and without `O_NOFOLLOW` | mitigated | `O_NOFOLLOW` in `try_lock` (`real.rs:269`) and `lock_path_ok` (`readonly.rs:141`). Residue: the locks are still in `/run/lock` (`real.rs:15`); a local user could pre-create and hold `pxu-*.lock` and cause "busy" (DoS without corruption) |
| L4 | Low | Any error opening ProxMenux's lock was treated as "nothing to respect" | fixed | Only `NotFound` passes; any other error skips the CT (`update/mod.rs:569-572`) |
| L5 | Low | The per-CT lock was released before returning the CT to `stopped` | fixed | `_locks` lives until after `pct shutdown` (`update/mod.rs:508-518`) |
| L6 | Low | `curl --config -` is classified as read-only although its stdin could write files | mitigated | The generated config escapes quotes and line breaks (`curl_q`, `notify.rs:125`) and sets `proto = "=http,https"` and `max-filesize = 65536` (`notify.rs:163`). Test: `curl_config_has_no_file_writing_options_and_resists_injection`. Residue: the classification is still by argv (`readonly.rs:59`), so it depends on `curl_config` being the only producer |
| L7 | Low | The read list included commands that `pxu` never runs (free-form `pvesh get`, `lxc-info`, `pct config`, `qm`…) | fixed | `host_cmd_is_read_only` (`readonly.rs:57-72`) only admits `curl --config -`, `apprise` with a fixed shape, validated `pvesh get …/feature`, the `hostcheck`/`PVE_VERSION` reads and the NVIDIA index |
| L8 | Low | The write opt-in was not withdrawn on every exit | fixed | RAII guards `UpdateOptIn`, `HostUpdateOptIn`, `NvidiaOptIn` with `Drop` (`host/mod.rs:158-203`), used in the CLI (`cli/mod.rs:332,453,497`) and TUI (`tui/mod.rs:90,120`). Test: `update_opt_in_is_revoked_on_every_exit`. Release builds with `panic = "abort"` (`Cargo.toml`) |
| L9 | Low | `/var/lib/pxu` and `outbox/` were created `0755` | fixed | `create_private_dirs` with `0o700` (`real.rs:111`), `check_private_dir` before writing (`real.rs:116`) and `install -d -m 0700` in `install.sh:88` |
| L10 | Low | `state.json` is read-modify-write without a lock | open | `check::persist` (`check.rs:301`) and `persist_info` (`update/mod.rs:999`) load, mutate and save without a lock, and `pxu check` takes no lock. Two simultaneous `pxu check` runs (or one during an update) can lose each other's results; the write is atomic, so there is no corruption |
| L11 | Low | Logs stored raw CT output, with possible credentials in URLs | fixed | `mask_url_creds(clean(l))` in `Ctx::say` (`update/mod.rs:276`; `fmt.rs:28`); files are written `0600`. Test: `masks_url_credentials` (`fmt.rs:101`) |
| L12 | Low | A corrupt backup registry was silently discarded and the next write erased it | fixed | `Registry::load` (`backup.rs:97-115`): absent ⇒ empty; unreadable or corrupt ⇒ copy `backups.json.corrupt-<epoch>`, warning and no pruning in that run. Test: `corrupt_registry_is_quarantined_and_nothing_is_pruned` |
| L13 | Low | `write_atomic` opened the `.tmp` without `O_NOFOLLOW` and did not check the directory | fixed | `write_atomic` (`real.rs:158-170`): real directory, owned by the current user and `0700`; `.tmp` with `O_EXCL\|O_NOFOLLOW`. Test: `write_atomic_refuses_unsafe_state_dirs_and_symlinked_tmp` |
| L14 | Low | vzdump's `--storage` validated with a lax `safe_name` and `numeric` accepted `0` | fixed | The `vzdump` branch requires `is_pve_id(st)` (`readonly.rs:130`, `backup.rs:195`); `numeric` rejects `0` and leading zeros (`readonly.rs:97`). Test: `backup_commands_have_exact_shapes_even_with_the_update_opt_in` |
| L15 | Low | The `update` opt-in unlocked the refresh probe of any CT | fixed | In `guest_exec` the opt-in only applies if the vmid is the CT in progress (`readonly.rs:168`); the TUI blocks `c`/`C`/`R` while an update runs (`tui/app.rs:1107-1110`). Test: `tui/tests.rs:349` |

---

## 3. Remaining risks

- **L10 (open).** `state.json` can lose the result of a check if two `pxu` processes write it at the same time; it does not corrupt the file or give access to anything. A lock around `persist` would mitigate it.
- **L3 (mitigated).** An unprivileged local user can leave a `pxu-*.lock` held in `/run/lock` and make updates answer "busy". A root-owned lock directory would close it.
- **L2 and M4 (mitigated).** They depend on `/etc/pxu` and `/var/lib/pxu` being owned by root with no access for others; the installer creates them `0700`, but only `/var/lib/pxu` is re-checked on write.
- **L6 (mitigated).** If other code were to generate a `curl` config with unescaped data in the future, the read list would not detect it.
