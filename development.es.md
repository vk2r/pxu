[English](development.md) | Español

# pxu: desarrollo

Guía para quien contribuye: compilar, probar, ejecutar las demos y orientarse en el código. Para instalar y usar pxu, mira el [README](README.es.md). Cómo funciona, configuración, línea de comandos, timer y logs: [`docs/ARCHITECTURE.es.md`](docs/ARCHITECTURE.es.md).

## Compilar

El toolchain se maneja con [mise](https://mise.jdx.dev) (`mise.toml`: Rust 1.99.1):

```sh
mise install                   # instala el toolchain
mise run build                 # target/release/pxu (~1,3 MB, sin símbolos)
mise run test                  # tests en perfil dev y mock, contra MockHost, sin Proxmox
mise run check                 # tests + release + comprueba que el binario de producción rechaza --mock
mise run lint                  # avisos de clippy (aún no bloqueantes)
mise tasks                     # lista todas las tareas
```

Para desarrollar sin servidor existe un binario de pruebas con un Proxmox simulado en memoria (`World` en `src/share/mock/scenario.rs`): no hay carpetas ni datos en disco, los CT son genéricos (`ct-101`, `ct-102`…):

```sh
mise run demo                             # TUI con 37 guests genéricos simulados (caso `demo`)
mise run demo -- update-ok update --yes   # otro caso y comando
```

Casos predefinidos y el builder de `World`: [`docs/ARCHITECTURE.es.md` §8.3](docs/ARCHITECTURE.es.md).

`mise run demo` compila antes `target/mock/pxu` (`cargo build --profile mock --features mock`). El binario de producción rechaza `--mock` (sale con código 2), y `--release --features mock` no compila.

## Mapa del código

```
src/main.rs           punto de entrada: elige CLI o TUI
src/cli/              comandos (list, check, update, config) y parseo de argumentos
src/tui/              interfaz de terminal
src/share/            lo que usan ambos:
  config.rs, model.rs, state.rs, notify.rs, fmt.rs, labels.rs   configuración, modelos, estado, Apprise, textos
  proxmox/            lectura de CT/VM y de los .conf de Proxmox
  update/             chequeo, backups y actualización con vuelta atrás
  host/               Host real y de solo lectura
  mock/               Proxmox simulado en memoria (solo tests / --features mock)
install.sh            instalador para el host Proxmox
dist/systemd/         pxu-update.service y .timer
tests/snapshots/      capturas de la TUI
docs/             ARCHITECTURE.md, DESIGN.md (TUI) y SECURITY-REVIEW.md
```

### Archivo por archivo

Un solo paquete `pxu`: una librería (`src/lib.rs` → `share/`, sin terminal) y el binario (`main.rs`, `cli/`, `tui/`) encima. `cli/` y `tui/` solo dependen de `share/`, no entre sí.

```
src/
├─ main.rs                 # elige CLI o TUI; envuelve el host en ReadOnlyHost
├─ lib.rs                  # pub mod share
├─ cli/{mod,args}.rs       # comandos; lexopt
├─ tui/{mod,app,views,logs,sched,apprise,tests}.rs
└─ share/
   ├─ config.rs  model.rs  fmt.rs  labels.rs  state.rs  schedule.rs
   ├─ notify.rs                         # Apprise
   ├─ proxmox/{conf,guests,hostcheck}.rs + probe.sh
   ├─ update/{mod,check,backup,hostupdate,nvidia,report,runlog}.rs
   ├─ host/{mod,real,readonly}.rs       # trait Host, RealHost, ReadOnlyHost
   └─ mock/{mod,scenario}.rs            # MockHost + builder World
tests/snapshots/           # capturas de la TUI (80×24 y 120×40)
dist/systemd/              # pxu-update.service y pxu-update.timer
install.sh   mise.toml
```

Dependencias (`Cargo.toml`, todas sin características por defecto salvo `lexopt` y `libc`): `ratatui`, `crossterm`, `serde`, `toml`, `toml_edit` (para editar la config conservando comentarios), `serde_json`, `lexopt`, `libc`. No hay runtime async, `clap`, `regex` ni `chrono`. El perfil `release` usa `opt-level = "s"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` y `strip`.

## Documentos técnicos

- [`docs/ARCHITECTURE.es.md`](docs/ARCHITECTURE.es.md): cómo funciona pxu. Actualización y backups (§3), notificaciones (§4), configuración con todas las claves y valores por defecto (§5), línea de comandos, códigos de salida y timer (§6), logs (§7), trait `Host` y pruebas (§8).
- [`docs/DESIGN.es.md`](docs/DESIGN.es.md): diseño de la TUI, con todas las teclas.
- [`docs/SECURITY-REVIEW.es.md`](docs/SECURITY-REVIEW.es.md): revisión de seguridad.

## Licencia

MIT
