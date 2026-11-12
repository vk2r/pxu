# Desarrollo

[English](DEVELOPMENT.md) | Español

Guía para quien contribuye: compilar, probar, ejecutar las demos y orientarse en el código. Para instalar y usar pxu, mira el [README](README.es.md). Cómo funciona, configuración, línea de comandos, timer y logs: [ARCHITECTURE](docs/ARCHITECTURE.es.md).

## Compilar

El toolchain se maneja con [mise](https://mise.jdx.dev) (`mise.toml`: Rust 1.99.1):

```sh
mise install                   # instala el toolchain
mise run build                 # target/release/pxu (~1,3 MB, sin símbolos)
mise run test                  # tests en perfil dev y mock, contra MockHost, sin Proxmox
mise run check                 # tests + release + comprueba que el binario de producción rechaza --mock
mise run lint                  # avisos de clippy (aún no bloqueantes)
mise run release-build         # binario estático musl para las releases (target/x86_64-unknown-linux-musl/release/pxu)
mise run package               # tarball + SHA256SUMS en target/package (tras release-build)
mise tasks                     # lista todas las tareas
```

Para desarrollar sin servidor existe un binario de pruebas con un Proxmox simulado en memoria (`World` en `src/share/mock/scenario.rs`): no hay carpetas ni datos en disco, los CT son genéricos (`ct-101`, `ct-102`…):

```sh
mise run demo                             # TUI con 37 guests genéricos simulados (caso `demo`)
mise run demo -- update-ok update --yes   # otro caso y comando
```

Casos predefinidos y el builder de `World`: [ARCHITECTURE §8.3](docs/ARCHITECTURE.es.md#83-builder-world-y-casos).

`mise run demo` compila antes `target/mock/pxu` (`cargo build --profile mock --features mock`). El binario de producción rechaza `--mock` (sale con código 2), y `--release --features mock` no compila.

## Publicar una release

Las releases las compila y las firma GitHub; el binario nunca se commitea en el repositorio.

Necesita un `.env` en la raíz del repo (git lo ignora) con la base de la cuenta de cada servidor; el script lo lee y se detiene si falta el archivo o alguna de las variables:

```sh
FORGEJO_URL=<URL base de la cuenta en Forgejo>
GITHUB_URL=<URL base de la cuenta en GitHub>
```

1. Ejecuta `mise run check`.
2. En `main`, con el árbol limpio (solo `Cargo.toml` y `Cargo.lock` pueden tener cambios): `mise run publish -- v1.2.3`. `tools/publish.sh` comprueba que el tag tenga la forma `v1.2.3` y no exista todavía, pone `version` en `1.2.3` en `Cargo.toml`, actualiza `Cargo.lock` y hace commit de ambos (`🔖 bump version to 1.2.3`) si cambiaron, crea el tag anotado y empuja `main` y el tag a Forgejo en un solo push, para que la sincronización del espejo lleve ambos a GitHub. Nunca fuerza y no espera: muestra los links a la build en GitHub Actions y a la release. GitHub solo se lee, para rechazar un tag que ya exista ahí.
3. El tag en GitHub lanza `.github/workflows/release.yml`: comprueba tag y versión, corre `mise run check` y `mise run lint`, compila el binario, lo empaqueta, lo atesta (build provenance) y publica la release con `pxu`, `pxu-<versión>-x86_64-linux.tar.gz`, `SHA256SUMS` y `provenance.sigstore.json`.

El binario es estático (`x86_64-unknown-linux-musl`). Uno glibc compilado en ubuntu-24.04 enlaza con glibc 2.39 y no correría en Proxmox 8 (Debian 12, glibc 2.36); el estático corre en 8 y 9. `mise run package` genera el mismo tarball en local para revisarlo. Nada se empuja directo a GitHub: lo que se cambie ahí se pisa en la siguiente sincronización del espejo. `publish` no usa remotos de git: arma las URLs con `FORGEJO_URL` y `GITHUB_URL` más el nombre de la carpeta del repo (`pxu`).

## Screenshots

Los PNG de `screenshots/` (los que usa el README) salen del mock, nunca de un servidor real:

```sh
mise run screenshots     # tmux (sin pantalla, 120×40) + freeze; compila antes target/mock/pxu
```

`screenshots/screenshots.sh` maneja la TUI con teclas y llama a `shot NN-nombre` por cada captura. Para agregar una, añade una línea `run <caso>; keys …; shot NN-nombre` y enlaza el PNG desde el README. Si ImageMagick (`magick`) está instalado, los PNG se reducen a una paleta de 96 colores (~3× más livianos). Usa solo datos del mock: sin IPs, nombres ni secretos reales.

Las capturas se generan con `PXU_MOCK_NO_BADGE=1` (lo pone el script; solo vale con `--mock`), que oculta la etiqueta `[MOCK]` de la cabecera. Al ejecutar `pxu --mock` a mano sigue apareciendo.

## Dónde van los documentos

- **`DEVELOPMENT.es.md`** (este archivo) y **`docs/`**: definiciones permanentes, en inglés y español (`.md` / `.es.md`, enlazados entre sí).
- **`development/`** (raíz, en minúsculas): documentos de trabajo e investigación temporales (informes, comparativas, planes). No existe hasta que hace falta.

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
screenshots/          PNG del README (mise run screenshots)
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

- [ARCHITECTURE](docs/ARCHITECTURE.es.md): cómo funciona pxu. Actualización y backups ([§3](docs/ARCHITECTURE.es.md#3-actualización-pxu-update)), notificaciones ([§4](docs/ARCHITECTURE.es.md#4-avisos-apprise)), configuración con todas las claves y valores por defecto ([§5](docs/ARCHITECTURE.es.md#5-configuración-etcpxuconfigtoml)), línea de comandos, códigos de salida y timer ([§6](docs/ARCHITECTURE.es.md#6-cli-y-timer)), logs ([§7](docs/ARCHITECTURE.es.md#7-logs)), trait `Host` y pruebas ([§8](docs/ARCHITECTURE.es.md#8-estructura-del-código-y-pruebas)).
- [DESIGN](docs/DESIGN.es.md): diseño de la TUI, con todas las teclas.
- [SECURITY-REVIEW](docs/SECURITY-REVIEW.es.md): revisión de seguridad.

## Licencia

MIT
