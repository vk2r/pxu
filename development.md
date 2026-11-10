# pxu: desarrollo

Documentación técnica de pxu: compilar, probar, instalar, estructura del código, referencia de comandos y configuración. Para instalar y usar pxu, mira el [README](README.md).

Actualizador liviano de contenedores LXC para Proxmox VE, pensado como alternativa a ProxMenux. Es un único binario en Rust con TUI (ratatui) y CLI. No tiene daemon ni dependencias en el host y ocupa unos pocos MB de RAM mientras corre.

- **Lista** los LXC y VM leyendo `/etc/pve/{lxc,qemu-server}/*.conf` y el estado desde cgroup y pidfiles, sin lanzar procesos.
- **Chequea** actualizaciones con una sola ejecución por CT. Por defecto es de **solo lectura**: no refresca índices salvo con `--refresh`. Soporta apt, apk, pacman, dnf y zypper.
- **Actualiza** los CT de uno en uno: backup temporal → simulación → upgrade → verificación. Si algo falla, **vuelve atrás automáticamente** al backup y luego lo borra.
- **Notifica** por [Apprise API](https://github.com/caronc/apprise-api) con `curl`. No requiere instalar Python.

> Estado: v0.1, en desarrollo. Probado en PVE 9.2 (actualización y vuelta atrás reales en un CT Debian 13). Las notificaciones Apprise y la vuelta atrás de un CT que estaba apagado solo están probadas con datos simulados.

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

Casos predefinidos: `demo`, `update-ok`, `update-fail`, `distros`, `cluster`, `failures`, `vms`. Cada test arma el suyo con el builder (`World::new().ct(Ct::new(101).distro(Distro::Alpine).stopped())…`).

`mise run demo` compila antes `target/mock/pxu` (`cargo build --profile mock --features mock`). El binario de producción rechaza `--mock` (sale con código 2), y `--release --features mock` no compila.

## Instalar

No hay releases publicadas: el binario se compila con `mise run build` y `install.sh` lo instala en el host.

**A mano:** copia `target/release/pxu`, `install.sh` y `dist/` al host y, como root:

```sh
sh install.sh ./pxu --timer   # --timer es opcional; sh install.sh [ruta-al-binario]
```

`install.sh` instala `/usr/local/bin/pxu`, crea `/etc/pxu`, `/var/lib/pxu` y `/var/log/pxu` (0700), no sobrescribe nunca `config.toml` y se puede repetir. `--force` omite la comprobación de Proxmox; `--uninstall` quita binario y timer (con `--purge` borra también `/etc/pxu`, `/var/lib/pxu` y `/var/log/pxu`).

Sin `/etc/pxu/config.toml`, `pxu` funciona en solo lectura (listar y chequear) y no actualiza nada. En la TUI, `a` sobre un CT (tabla o detalle) crea ese archivo con solo el override del CT, tras pedir confirmación, y `pxu` sale del modo solo lectura.

## Uso

```
pxu                                   TUI
pxu list   [--json] [--all-nodes]
pxu check  [--json] [--refresh] [--notify] [--ct 101,103] [vmid...]
pxu update [--json] [--yes] [--no-notify] [--ct 101,103] [vmid...]
pxu config check                      valida /etc/pxu/config.toml
pxu notify-test                       envía un aviso de prueba por Apprise
```

`update` es el único comando que escribe. Solo toca CT con un `pxu check` positivo reciente (`require_fresh_check_minutes`). Sin `--ct`, actualiza los CT habilitados con `auto_update` o con un tag de `auto_update_tags`.

**Teclas de la TUI:** `↑↓`/`jk` mover · `/` buscar · `s` ordenar · `1-4` filtrar · `Enter` detalle · `c` chequear · `C` refrescar y chequear (pide confirmación) · `u` actualizar (pide confirmación) · `x`/`Esc` cancelar la corrida · `?` ayuda · `q` salir.

### Códigos de salida

| Código | Comando | Significado |
|---|---|---|
| 0 | todos | OK / sin actualizaciones |
| 2 | todos | uso incorrecto, config inválida o con permisos inseguros |
| 3 | `update` | otra corrida en curso |
| 5 | `notify-test` | el envío falló |
| 10 | `check` | hay actualizaciones |
| 12 | `check` | algún CT falló al chequearse |
| 20 | `update` | algún CT falló |
| 30 | `update` | corrida detenida (2 fallos seguidos o Ctrl-C) |

## Configuración

`/etc/pxu/config.toml` debe ser de root con modo `0600`; si no, `pxu` se niega a usarlo. Ejemplo mínimo:

```toml
[update]
auto_update_tags = ["autoupdate"]   # CT con este tag de PVE se actualizan
stopped          = "skip"           # "skip" | "start_and_restore"
reboot           = "never"          # "never" | "if-required"

[backup]
mode    = "auto"     # "auto" (snapshot si el CT lo admite, si no vzdump) | "snapshot" | "vzdump" | "none"
storage = "local"    # destino de vzdump
keep    = 0          # backups que se conservan tras un update OK (0 = temporales)

[apprise]
api_url = "http://apprise.local:8000"
api_key = "pxu"
on      = ["updates_found", "update_failed", "run_summary", "check_error"]

[[ct]]               # override por CT
vmid        = 101
auto_update = true
```

La TUI también escribe esos overrides: `a` alterna el `auto_update` del CT seleccionado (modal con «hoy: … → nuevo estado»; `y`/Enter confirma, Esc cancela). Solo cambia la línea `auto_update` del bloque `[[ct]]` de ese vmid, o añade un bloque al final; comentarios y orden se conservan. Se valida el TOML resultante y se escribe de forma atómica (root, `0600`). Un override `false` gana sobre el tag del CT.

`pxu update` y la TUI validan la config igual que `pxu config check` antes de usarla: valores desconocidos (`on_failure = "Stop"`, `apt_mode = "Upgrade"`) o claves mal escritas (`on_falure`) son errores. `pxu update` sale con código 2 y los muestra; la TUI arranca en solo lectura con el motivo en la barra de estado. Los avisos (warnings) no bloquean.

### Aviso y log de cada corrida

Cada corrida de update (timer, `pxu update` o la TUI) termina con **un solo aviso**: una tabla Markdown (`Upgrades | Result | Packages | GPU driver | Time | Details | Manual`, el host primero y luego cada CT, también los fallidos y omitidos) bajo el título `pxu: update run on <host> — N ok, N warn, N failed · <dur>` (+ `STOPPED` si se detuvo, + `· NVIDIA failed`). Con más de 25 filas van primero las que no están ok, luego hasta 25 ok y `+N more ok (see log)`. El nivel del aviso es success, warning o failure según el peor resultado.

- Evento `run_summary`: se envía siempre al acabar la corrida. Evento `update_failed`: si solo está este (sin `run_summary`), el mismo aviso se envía solo cuando algún CT o el driver NVIDIA falló. Las configs viejas siguen valiendo.
- Cada corrida escribe **un log**: `/var/log/pxu/run-AAAAMMDD-HHMM.log` (UTC en el nombre; `-SS` si ya existe uno de ese minuto; `0600`). Se crea al empezar y crece mientras corre: una sección por host, driver NVIDIA y CT, con la hora de cada línea, y al terminar el resumen igual que el aviso; también se escribe si la corrida se detiene o se cancela (con el motivo). Ya no se crean `/var/log/pxu/<vmid>/…`, `host-….log` ni `nvidia-….log`. Un corte brusco (SIGKILL, caída del host) deja el log parcial, que la página Logs lista como `⛔ INCOMPLETE`.
- `[general] log_retention_days = 30` (por defecto): al acabar cada corrida se borran los logs de pxu con más de N días, incluidos los formatos viejos y las carpetas `<vmid>` vacías; `0` = no borrar nunca (máx. 3650). Solo se tocan archivos normales con nombre de pxu dentro de `/var/log/pxu` (no se siguen enlaces simbólicos). `logs_per_ct` ya no existe: se acepta y se ignora (aviso de config obsoleta).
- TUI: `l` abre la página **Logs** (fecha, duración, resultado y tamaño, del más nuevo al más viejo; ↑↓/jk, clic para elegir, doble clic o Enter para abrir). En el visor: flechas, PgUp/PgDn y rueda para desplazar, `g`/`G` principio/final, `n`/`p` log siguiente/anterior, `q`/Esc vuelve a la lista con el mismo log elegido. `L` en la vista Config edita `log_retention_days`.

### Apprise: desde la TUI

En la vista Config (`g`) la sección «Notifications (Apprise)» muestra el estado de `[apprise]` con los secretos tapados: la `api_url` solo con esquema y host, la `api_key` como `•••` y cada URL de destino como `tgram://…`.

- `N` abre el diálogo: dirección de la API, modo (clave guardada en Apprise API **o** URLs directas, separadas por coma) y los eventos que avisan (`updates_found`, `update_failed`, `run_summary`, `check_error`). El secreto guardado no se muestra: con el campo vacío se conserva, y lo que escribes se pinta como puntos.
- Con Enter valida la config resultante entera con el mismo código que `pxu config check`; si no valida (o no queda ningún transporte utilizable) no escribe nada y dice por qué. Luego pide confirmar con «hoy → nuevo» (secretos tapados): `y`/Enter guarda, `b` vuelve a editar, Esc cancela.
- Guarda solo `[apprise]`: el resto de `config.toml` y sus comentarios se conservan, la escritura es atómica (root, `0600`) y, si el archivo no existe, lo crea. La clave y las URLs se excluyen entre sí: al cambiar de modo se quita la otra.
- `t` envía un aviso de prueba, igual que `pxu notify-test` (muestra «enviando…», y luego OK o el error con código 5, sin volcar secretos).
- Con `--read-only` la edición y la prueba se rechazan: la prueba sale por red, así que cuenta como acción.

Todas las claves y sus valores por defecto están en [`docs/ARCHITECTURE.md` §5](docs/ARCHITECTURE.md).

## Backups y vuelta atrás

- En modo `auto`, `pxu` pregunta a Proxmox si el CT admite snapshots. Si no los admite, hace un `vzdump` en `[backup] storage`.
- Si el upgrade o la verificación fallan, restaura el CT con `pct rollback` o con `pct restore … --force --storage <storage del rootfs>`, lo enciende y borra el backup.
- Si la vuelta atrás falla o el upgrade se pasa de tiempo, el backup **se conserva**, la corrida se detiene y se muestra el comando para restaurar a mano.
- **El backup cubre solo el rootfs.** Los bind mounts (`mpN` hacia `/mnt/pve/...`) no entran en vzdump ni en la vuelta atrás.
- `pxu` solo borra los backups de su propio registro (`/var/lib/pxu/backups.json`), y cada comando de escritura está atado al CT que se está actualizando.

## Ejecución programada

`install.sh --timer` instala y activa `pxu-update.timer` (diario 04:30, con retraso aleatorio de 15 min). Las unidades están en `dist/systemd/` y puedes editarlas antes de instalar:

```sh
cat dist/systemd/pxu-update.service dist/systemd/pxu-update.timer
```

El `-` delante del `check` hace que su código 10 ("hay actualizaciones") no detenga la unidad.

### Desde la TUI

Sin tocar `systemctl` a mano: en la vista Config (`g`) se ve el estado real del timer (unidades instaladas, activado, horario efectivo y próxima ejecución), y `S` (también desde la lista o el detalle) abre el diálogo del horario:

- Activar o desactivar el timer, y elegir diario a HH:MM, lunes a viernes a HH:MM, semanal (día + HH:MM) o un `OnCalendar` libre. Las horas son las del servidor.
- Antes de confirmar valida el horario con `systemd-analyze calendar` y muestra la próxima ejecución; si no valida, no escribe nada.
- Tras la confirmación («hoy: … → nuevo»; `y`/Enter guarda, Esc cancela) escribe el drop-in `/etc/systemd/system/pxu-update.timer.d/schedule.conf` (`OnCalendar=` vacío + el nuevo, 0644, atómico), hace `systemctl daemon-reload` y `enable --now` o `disable --now`.
- Si las unidades no están instaladas, el diálogo las instala (son las mismas de `dist/systemd/`, embebidas en el binario).
- Avisa si no hay `config.toml` o ningún CT con auto-update (el update no haría nada) o si la config es inválida (fallaría con código 2). Rechazado en solo lectura forzado.

`install.sh --uninstall` borra también ese drop-in.

## Estructura

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

## Documentos de diseño

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): arquitectura y todas las claves de configuración con sus valores por defecto (§5).
- [`docs/DESIGN.md`](docs/DESIGN.md): diseño de la TUI.
- [`docs/SECURITY-REVIEW.md`](docs/SECURITY-REVIEW.md): revisión de seguridad.

## Licencia

MIT
