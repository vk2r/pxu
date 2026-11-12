English | [Español](DEVELOPMENT.es.md)

# pxu: development

Guide for contributors: build, test, run the demos and find your way around the code. To install and use pxu, see the [README](README.md). How it works, configuration, command line, timer and logs: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

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

Predefined cases and the `World` builder: [`docs/ARCHITECTURE.md` §8.3](docs/ARCHITECTURE.md).

`mise run demo` first builds `target/mock/pxu` (`cargo build --profile mock --features mock`). The production binary rejects `--mock` (exits with code 2), and `--release --features mock` doesn't compile.

## Screenshots

The PNGs in `screenshots/` (used by the README) come from the mock, never from a real server:

```sh
mise run screenshots     # tmux (headless, 120×40) + freeze; builds target/mock/pxu first
```

`screenshots/screenshots.sh` drives the TUI with keys and calls `shot NN-name` for each capture. To add one, add a `run <case>; keys …; shot NN-name` line and reference the PNG from the README. If ImageMagick (`magick`) is installed the PNGs are reduced to a 96-colour palette (~3× smaller). Use only mock data: no real IPs, names or secrets.

## Where documents go

- **`DEVELOPMENT.md`** (this file) and **`docs/`**: permanent definitions, in English and Spanish (`.md` / `.es.md`, linked to each other).
- **`development/`** (root, lowercase): temporary working and research documents (reports, comparisons, plans). It does not exist until needed.

## Code map

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
dist/systemd/         pxu-update and pxu-notify-flush (.service and .timer)
tests/snapshots/      TUI captures
screenshots/          PNGs of the README (mise run screenshots)
docs/             ARCHITECTURE.md, DESIGN.md (TUI) and SECURITY-REVIEW.md
```

### File by file

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
dist/systemd/              # pxu-update and pxu-notify-flush units
install.sh   mise.toml
```

Dependencies (`Cargo.toml`, all without default features except `lexopt` and `libc`): `ratatui`, `crossterm`, `serde`, `toml`, `toml_edit` (to edit the config preserving comments), `serde_json`, `lexopt`, `libc`. There is no async runtime, `clap`, `regex` or `chrono`. The `release` profile uses `opt-level = "s"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` and `strip`.

## Technical documents

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): how pxu works. Update and backups (§3), notifications (§4), configuration with all keys and defaults (§5), command line, exit codes and timer (§6), logs (§7), `Host` trait and tests (§8).
- [`docs/DESIGN.md`](docs/DESIGN.md): TUI design, with all keys.
- [`docs/SECURITY-REVIEW.md`](docs/SECURITY-REVIEW.md): security review.

## License

MIT
