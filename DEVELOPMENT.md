# Development

English | [Español](DEVELOPMENT.es.md)

Guide for contributors: build, test, run the demos and find your way around the code. To install and use pxu, see the [README](README.md). How it works, configuration, command line, timer and logs: [ARCHITECTURE](docs/ARCHITECTURE.md).

## Build

The toolchain is managed with [mise](https://mise.jdx.dev) (`mise.toml`: Rust 1.99.1):

```sh
mise install                   # installs the toolchain
mise run build                 # target/release/pxu (~1.3 MB, stripped)
mise run test                  # tests in dev and mock profile, against MockHost, no Proxmox needed
mise run check                 # tests + release + checks that the production binary rejects --mock
mise run lint                  # clippy warnings (not blocking yet)
mise run release-build         # static musl binary for releases (target/x86_64-unknown-linux-musl/release/pxu)
mise run package               # tarball + SHA256SUMS in target/package (after release-build)
mise tasks                     # lists all tasks
```

To develop without a server there is a test binary with an in-memory simulated Proxmox (`World` in `src/share/mock/scenario.rs`): there are no folders or data on disk, and the CTs are generic (`ct-101`, `ct-102`…):

```sh
mise run demo                             # TUI with 37 generic simulated guests (`demo` case)
mise run demo -- update-ok update --yes   # another case and command
```

Predefined cases and the `World` builder: [ARCHITECTURE §8.3](docs/ARCHITECTURE.md).

`mise run demo` first builds `target/mock/pxu` (`cargo build --profile mock --features mock`). The production binary rejects `--mock` (exits with code 2), and `--release --features mock` doesn't compile.

## Publishing a release

Releases are built and signed by GitHub; the binary is never committed to the repository.

Requires a `.env` at the repository root (ignored by git) with the account base of each host; the script reads it and stops if it or either variable is missing:

```sh
FORGEJO_URL=<forgejo account base URL>
GITHUB_URL=<github account base URL>
```

1. Run `mise run check`.
2. On `main`, with a clean tree (only `Cargo.toml` and `Cargo.lock` may have changes): `mise run publish -- v1.2.3`. `tools/publish.sh` checks that the tag looks like `v1.2.3` and doesn't exist yet, sets `version` in `Cargo.toml` to `1.2.3`, updates `Cargo.lock` and commits both (`🔖 bump version to 1.2.3`) if they changed, creates the annotated tag and pushes `main` and the tag to Forgejo in a single push, so the mirror sync it triggers carries both to GitHub. It never forces and doesn't wait: it prints the links to the build in GitHub Actions and to the release. GitHub is only read, to refuse a tag that already exists there.
3. The tag on GitHub starts `.github/workflows/release.yml`: it checks tag and version, runs `mise run check` and `mise run lint`, builds the binary, packages it, attests it (build provenance) and publishes the release with `pxu`, `pxu-<version>-x86_64-linux.tar.gz`, `SHA256SUMS` and `provenance.sigstore.json`.

The binary is static (`x86_64-unknown-linux-musl`). A glibc build on ubuntu-24.04 links against glibc 2.39 and would not run on Proxmox 8 (Debian 12, glibc 2.36); the static one runs on 8 and 9. `mise run package` builds the same tarball locally to inspect it. Nothing is pushed straight to GitHub: anything changed there is overwritten by the next mirror sync. `publish` doesn't use git remotes: it builds the URLs from `FORGEJO_URL` and `GITHUB_URL` plus the name of the repository folder (`pxu`).

## Screenshots

The PNGs in `screenshots/` (used by the README) come from the mock, never from a real server:

```sh
mise run screenshots     # tmux (headless, 120×40) + freeze; builds target/mock/pxu first
```

`screenshots/screenshots.sh` drives the TUI with keys and calls `shot NN-name` for each capture. To add one, add a `run <case>; keys …; shot NN-name` line and reference the PNG from the README. If ImageMagick (`magick`) is installed the PNGs are reduced to a 96-colour palette (~3× smaller). Use only mock data: no real IPs, names or secrets.

Screenshots are generated with `PXU_MOCK_NO_BADGE=1` (set by the script; only honoured with `--mock`), which hides the `[MOCK]` header badge. Running `pxu --mock` by hand still shows it.

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

- [ARCHITECTURE](docs/ARCHITECTURE.md): how pxu works. Update and backups (§3), notifications (§4), configuration with all keys and defaults (§5), command line, exit codes and timer (§6), logs (§7), `Host` trait and tests (§8).
- [DESIGN](docs/DESIGN.md): TUI design, with all keys.
- [SECURITY-REVIEW](docs/SECURITY-REVIEW.md): security review.

## License

MIT
