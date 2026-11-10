[English](ARCHITECTURE.md) | Español

# ARCHITECTURE — `pxu`

Referencia técnica de cómo funciona `pxu` hoy. La TUI está descrita en [DESIGN.md](DESIGN.es.md); el análisis de seguridad, en [SECURITY-REVIEW.md](SECURITY-REVIEW.es.md). Instalación y uso: [README.md](../README.es.md); compilar y probar: [development.md](../development.es.md).

Limitación conocida: la lógica se prueba contra un servidor simulado (`MockHost`, §8). El comportamiento con todos los tipos de storage y versiones de Proxmox no se ha comprobado en un servidor real.

---

## 0. Decisiones

| Tema | Decisión | Dónde se ve |
|---|---|---|
| Lenguaje / TUI | Rust + ratatui (crossterm); `libc` para locks, prioridades y señales | `Cargo.toml` |
| Proceso | **Sin daemon.** Se ejecuta, hace su trabajo y termina. La automatización es un timer de systemd | `dist/systemd/` |
| Concurrencia | **Sin runtime async.** La TUI lanza un hilo por trabajo (chequeo, update); la CLI trabaja en el hilo principal | `src/tui/mod.rs`, `src/cli/mod.rs` |
| Listado | Se lee `/etc/pve/**/*.conf` y `/sys/fs/cgroup`; sin lanzar procesos | §1 |
| Ejecución en CT | **Una sola ejecución por CT por chequeo** (script `probe.sh` embebido) con `lxc-attach` | §2.1 |
| Chequeo | Solo lectura por defecto: no refresca los índices | §2.3 |
| Actualización | Secuencial; lock global + lock por CT; backup temporal previo (snapshot o vzdump) con rollback automático; verificación posterior | §3 |
| Host Proxmox y NVIDIA | Se revisa en cada `check`; solo se actualiza con orden expresa o con opt-in en config | §3.6, §3.7 |
| Avisos | Apprise API vía `curl` (respaldo: CLI `apprise`); un solo aviso por corrida | §4 |
| Config | `/etc/pxu/config.toml`, 0600 root | §5 |
| Pruebas | Trait `Host` con `RealHost` y `MockHost` (servidor simulado en memoria) | §8 |

Nada de lo que `pxu` ejecuta pasa por un shell del host: todo `Command` lleva argv fijo. El único `sh -c` corre **dentro** del CT con el script constante del binario.

---

## 1. Listado de LXC y VM

`pxu list` y la TUI leen archivos; `src/share/proxmox/guests.rs` no lanza procesos.

| Dato | Fuente |
|---|---|
| Config de cada guest | `/etc/pve/nodes/<nodo>/{lxc,qemu-server}/<vmid>.conf` |
| Guest local o remoto | el directorio del nodo; los de otros nodos se listan como `remote` y no se tocan |
| LXC encendido | `/sys/fs/cgroup/lxc/<vmid>/cgroup.events` con `populated 1` (en PVE 9 `cgroup.procs` viene vacío) |
| VM encendida | `/var/run/qemu-server/<vmid>.pid` + `/proc/<pid>` existente |
| RAM y CPU en vivo (LXC) | `memory.current` y `cpu.stat` del cgroup; la TUI muestrea la CPU cada 3 s (la primera, a 1 s) |

`pxu list --all-nodes` incluye los guests de otros nodos; sin la opción solo salen los del nodo local. Además de los guests, el inventario incluye una fila del **host Proxmox** (vmid `0`, §3.6).

### Parser de `.conf` (`proxmox/conf.rs`)

- Las líneas `#…` iniciales son la descripción.
- `clave: valor` hasta la primera sección `[nombre]`; las secciones siguientes son snapshots o `[pending]`.
- Las claves `lxc.*` pueden repetirse y se guardan en orden.
- Una clave `lock:` en la config deja el guest bloqueado: nunca se actualiza. `template: 1` lo marca como plantilla: se lista, nunca se actualiza.

---

## 2. Chequeo de actualizaciones

### 2.1 Una ejecución por CT

`pxu` ejecuta **un** script POSIX `sh` (compatible con busybox) embebido con `include_str!` desde `src/share/proxmox/probe.sh`, pasado por argv; no se escribe nada en el CT. En el CT corre así:

```
lxc-attach -n <vmid> --clear-env --set-var PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin -- sh -c "$PROBE" pxu-probe <refresh:0|1> <edad_mínima_s>
```

(`host/real.rs::lxc_attach_argv`; `pct exec` no se usa.) El script imprime un protocolo de líneas con prefijo `@@`:

```
@@v 1
@@os <ID> <VERSION_ID> "<ID_LIKE>"
@@pm apt|apk|pacman|dnf|zypper|none     # detectado por binario presente
@@cache_age <segundos>                   # -1 si no se puede saber
@@refresh skipped|fresh|ok|failed:<rc>|unavailable|unavailable-checkupdates-used
@@pkg <nombre> <actual> <nueva> <seguridad 0|1>
@@held <nombre>
@@reboot_required 0|1
@@end <rc>
```

Siempre con `LC_ALL=C`. Sin `@@end` el resultado es error (timeout, CT parado, `sh` ausente). La salida del CT se sanea con `fmt::clean` antes de usarla (SECURITY-REVIEW, H1).

### 2.2 Por gestor de paquetes

| Gestor | Chequeo (solo lectura) | Refresco (opt-in) | Upgrade (`update_argv`) |
|---|---|---|---|
| **apt** | `apt-get -s -o Debug::NoLocking=1 dist-upgrade` (líneas `Inst`; el origen `*Security*` marca seguridad) + `apt-mark showhold` | `apt-get update -qq` | `env DEBIAN_FRONTEND=noninteractive apt-get -y -o Dpkg::Options::=--force-confold -o Dpkg::Options::=--force-confdef dist-upgrade` (`upgrade` con `[update] apt_mode = "upgrade"`) |
| **apk** | `apk --no-network list -u` | `apk update` | `apk upgrade --no-cache` |
| **pacman** | `pacman -Qu` (base de datos local) | no se refresca: informa `unavailable` y no se hace `pacman -Sy` en el chequeo | `pacman -Syu --noconfirm` |
| **dnf** (o `yum`) | `dnf -q -C check-update` (código 100 = hay updates) | `dnf -q makecache` | `dnf -y upgrade` |
| **zypper** | `zypper --no-refresh -n -q list-updates` | `zypper -n -q refresh` | `zypper -n update` |
| otro | `@@pm none` → estado `unsupported`: se lista, no se toca | — | — |

Los upgrades están en `update/mod.rs::upgrade_argv`, que es la única fuente del motor, del mock y de la lista blanca.

### 2.3 Modos

- **Solo lectura (por defecto):** `[check] refresh = false`. Muestra el estado del índice que ya tiene el CT, con su edad.
- **Refresco:** `[check] refresh = true`, `[[ct]] refresh = true`, `pxu check --refresh` o la tecla de refresco de la TUI. Con `refresh_min_age_hours` (24) solo se refresca si el índice es más viejo.
- Un índice más viejo que `[check] index_stale_days` (7) se marca `stale`.
- Los CT detenidos, las VM y los guests remotos no se chequean (`check::targets`).

### 2.4 Ejecución y persistencia

- Secuencial, un CT tras otro; el timeout por CT es `[check] timeout_s` (60 s; el triple con refresco).
- El resultado se guarda en `/var/lib/pxu/state.json` (escritura atómica): fecha, gestor, paquetes, nº de seguridad, edad del índice y una **huella** (FNV-1a de la lista ordenada `nombre=versión_nueva`, `state::fingerprint`).
- La huella evita repetir avisos: §4.3. Un chequeo es «reciente» para `update` si tiene menos de `[update] require_fresh_check_minutes` (60).
- `pxu check` también revisa el **host Proxmox** (`proxmox/hostcheck.rs`, solo lectura): paquetes pendientes, repositorios, reinicio pendiente y GPU/driver NVIDIA. Los únicos comandos que usa son `apt-get update` (solo con refresco), `apt list --upgradable`, `apt-get -s … full-upgrade`, `lspci -nn` y `dkms status`.

---

## 3. Actualización (`pxu update`)

Código: `src/share/update/mod.rs`. Es el único comando que escribe.

### 3.1 Selección

`update::select` toma, sin argumentos, los LXC con `auto_update` (por `[[ct]]` o por tag, §5); con vmids o `--ct` toma esos. Cada candidato puede quedar **omitido** (`skipped`) con un motivo, sin tocarse:

- VM, CT de otro nodo, plantilla, con `lock:` de PVE, o detenido sin `stopped = "start_and_restore"`.
- Nunca chequeado, o chequeo más viejo que `require_fresh_check_minutes`.
- Sin paquetes pendientes: queda omitido (en la corrida sin selección explícita ni siquiera se lista).

### 3.2 Flujo de la corrida

1. **Lock global** `/run/lock/pxu-run.lock` (no bloqueante): impide dos corridas a la vez (timer y TUI). Si está tomado, `RunError::Locked` → exit 3. El log de la corrida se crea al tomarlo (§6).
2. Si `[update] host_auto = true`, la corrida «de todos» empieza por el **host** (§3.6); después, el driver NVIDIA si `host_nvidia_auto` (§3.7). Solo si el host acabó bien se sigue con los CT.
3. Cada CT, por vmid ascendente, pasa por los pasos de §3.3.
4. **Cortacircuito:** `max_consecutive_failures` fallos seguidos (2) detienen la corrida (con `on_failure = "stop"`, el primer fallo). Los CT restantes quedan `skipped`.
5. Al terminar: informe, log final y aviso único (§4.3).

Cancelación (Ctrl-C/SIGINT/SIGTERM/SIGHUP en la CLI, diálogo en la TUI): la corrida termina el CT en curso y se detiene; los siguientes quedan `cancelled` y el exit es 30. `dpkg`/`apt` nunca se matan por una cancelación.

### 3.3 Pasos por CT

```
Lock → Preflight → Backup → Refresh+Simulate → Upgrade → Verify → (Rollback) → Restore
```

1. **Lock (`lock_ct`).** Lock por CT `/run/lock/pxu-ct-<vmid>.lock`. Convivencia con ProxMenux: si existe `/run/lock/proxmenux-lxc-update-<vmid>.lock` se abre **sin `O_CREAT`** y se intenta `flock`; ocupado, o con contenido (una reserva), o con `proxmenux-lxc-check-invalid-<vmid>`, el CT se omite. Cualquier error distinto de «no existe» también lo omite (falla en cerrado). Los locks duran hasta devolver el CT a su estado inicial.
2. **Preflight.** Si el CT está detenido (`stopped = "start_and_restore"`) se arranca con `pct start`. Comprueba espacio libre en `/` (`df -Pk /` ≥ `min_free_mb`, 512) y anota las unidades fallidas de systemd (`systemctl --failed`) para compararlas luego.
3. **Backup** (`backup.mode`, por defecto `auto`; `[[ct]] backup` lo cambia por CT):
   - `auto`: pregunta a PVE (`pvesh get /nodes/<nodo>/lxc/<vmid>/feature --feature snapshot`, solo lectura); con `hasFeature = 1` usa `pct snapshot <vmid> pxu-<AAAAMMDDHHMMSS>` (sufijo `-2`… `-99` si el nombre existe); si no, o si la consulta falla, cae a vzdump.
   - `snapshot` / `vzdump`: solo esa vía, sin caída a la otra.
   - vzdump: `vzdump <vmid> --mode snapshot --storage <[backup] storage> --compress zstd --zstd 1 --ionice 7 --notes-template "pxu pre-update {{guestname}}"`. El storage del `rootfs:` se lee del `.conf` y se anota; sin él no se hace el backup (el rollback restauraría en `local`).
   - `none` (solo si se pone a mano): sin backup ni rollback; el log y `config check` lo avisan.
   - **Si el backup falla, el CT no se toca** (fallo en el paso `backup`).
   - **Alcance:** el backup y el rollback cubren solo el rootfs; los bind mounts y `mpN` con `backup=0` no se guardan ni se revierten (`BACKUP_SCOPE_NOTE`, escrito en el log de cada CT).
4. **Refresh + Simulate.** Sondeo con refresco (siempre necesario para actualizar). Con 0 paquetes el CT queda `skipped: already up to date after refresh` y el backup se descarta.
5. **Upgrade.** El comando de §2.2, con `nice` e `ionice` de `[general]`. Timeout `[update] timeout_min` (30 min) **sin matar el proceso** (`ExecOpts.no_kill_on_timeout`): al vencer, el CT queda fallido con un `WARNING` en el log, la corrida se detiene y el backup se conserva sin rollback (el CT puede estar a medias; el aviso trae el comando manual).
6. **Verify.** En apt, `dpkg --audit` debe salir vacío. Se comparan las unidades fallidas antes y después: **unidades nuevas fallidas ⇒ fallo**. Se repite el sondeo sin refrescar, se guarda en `state.json` y se anota si hay `reboot-required`. Con reinicio pendiente: `reboot = "if-required"` ejecuta `pct reboot`; con `"never"` el CT queda `warn: reboot required`.
7. **Settle** (`settle`), según el resultado:
   - Éxito o `warn`: el backup pasa a `kept` y se poda a los `keep` más nuevos de ese CT (`keep = 0` ⇒ se borra).
   - `skipped`: el backup se borra.
   - Fallo: **rollback automático**: `pct shutdown`, `pct stop` si sigue encendido, y `pct rollback <vmid> <snapshot>` o `pct restore <vmid> <volid> --force --storage <storage del rootfs>`; vuelve a arrancarlo si estaba encendido y comprueba que responde (`df`). Solo entonces borra el backup (`pct delsnapshot` / `pvesm free`).
   - Rollback fallido o timeout del upgrade: el backup queda `held`, nunca se borra solo, el CT se marca fallido, la corrida se detiene y se da el comando manual (`pct rollback …` o `pct stop …; pct restore … --force --storage …`, que pxu nunca ejecuta por su cuenta).
8. **Restore.** Si pxu arrancó el CT, lo apaga (`pct shutdown --timeout 60`); tras un timeout del upgrade **no** se apaga (dpkg puede seguir vivo) y se avisa.

Resultados por CT: `ok`, `warn`, `failed`, `skipped`, `cancelled`.

### 3.4 Registro de backups (`update/backup.rs`)

`/var/lib/pxu/backups.json` anota cada backup de pxu con estado `active` (en curso; si pxu muere, la siguiente corrida lo pasa a `held`), `held` (nunca se borra) o `kept` (candidato a poda). Solo se borra lo que pxu creó: snapshots `pxu-<14 dígitos>[-N]` y volúmenes vzdump anotados en el registro. Snapshots ajenos (`manual-*`, `zfs_auto_*`…) no se tocan. Cada entrada se valida al cargar; un registro corrupto se aparta como `backups.json.corrupt-<epoch>` y esa corrida no poda nada.

### 3.5 Prioridad de procesos y salida

- Cada proceso hijo lleva `nice` y `ioprio` (`pre_exec`), `process_group(0)` y un único pipe para stdout+stderr (`host/real.rs::run`), de modo que el orden se conserva y un Ctrl-C del terminal no llega a `apt`. `nice`/`ioprio` se heredan hasta `apt`/`dpkg` dentro del CT.
- Los timeouts de lectura (sondeos) se implementan con un hilo temporal que mata el grupo con SIGKILL; el del upgrade no mata (§3.3).
- La salida va línea a línea al log de la corrida y a la TUI, que guarda un buffer de `LOG_RING = 2000` líneas. El motor corta a 20 000 líneas por CT (`LOG_CAP`).

### 3.6 Host Proxmox (`update/hostupdate.rs`)

- **A mano:** `pxu update --host` (pide un terminal y confirmación; con `--yes` solo omite la pregunta) o la tecla `u` sobre la fila del host en la TUI.
- **Programado:** con `[update] host_auto = true`, el host va primero en la corrida «de todos»; exige un chequeo del host reciente.
- Orden: locks → `dpkg --audit` → espacio en `/var/cache/apt/archives` → lista fresca (a mano debe coincidir con la confirmada) → simulación → copia de `/etc` en `/var/backups/pxu` (se conservan 5) → snapshot ZFS si la raíz es ZFS → `apt-get full-upgrade` (tope 3 h, sin matar) → `autoremove` si es seguro → nueva revisión. **Nunca reinicia el host.**
- Usa su propio opt-in y su propia lista blanca de comandos (`is_write_argv`).

### 3.7 Driver NVIDIA (`update/nvidia.rs`)

Solo con `[update] host_nvidia_auto = true` (host) y `[[ct]] nvidia_auto = true` (CT), o desde el diálogo de la TUI. Aplica a un driver instalado con el `.run` oficial (existe `/usr/bin/nvidia-uninstall`). Descarga de `download.nvidia.com` el `.run` y su `.sha256sum`, verifica y solo sube dentro de la misma rama mayor. Los CT reciben las librerías de la versión que tiene el host. Nunca reinicia el host.

---

## 4. Avisos (Apprise)

### 4.1 Transporte (`share/notify.rs`)

| `[apprise] transport` | Comportamiento |
|---|---|
| `auto` (por defecto) | `api` si hay `api_url`; si no, `cli` si `apprise` está en el `PATH`; si no, `none` (avisos desactivados, con aviso visible) |
| `api` | `curl` hacia la Apprise API: con `api_key` es el modo stateful (`POST {api_url}/notify/<key>`, los destinos viven en la API); sin clave, stateless (`POST {api_url}/notify/` con `urls`) |
| `cli` | el CLI `apprise` con las URLs en la variable `APPRISE_URLS` |
| `none` | sin avisos |

`pxu` nunca instala Python ni pip. Los secretos van **fuera de argv**: a `curl` se le pasa su configuración y el cuerpo por stdin (`curl --config -`). Timeout `timeout_s` (10 s) y reintentos con las esperas de `retry_delays_ms` (por defecto 2 s y 8 s). Los mensajes de error se limpian de claves y URLs.

### 4.2 Eventos (`[apprise] on`)

| Evento | Cuándo | Deduplicación |
|---|---|---|
| `updates_found` | tras `check --notify`: CT (y el host) con paquetes nuevos | solo si la huella cambió desde el último aviso; el host usa la misma huella con clave `0` |
| `check_error` | un CT no se pudo chequear, o falló la revisión del host | — |
| `run_summary` | fin de `update`: **un solo aviso por corrida** con tabla (host, NVIDIA y cada CT: resultado, paquetes, tiempo, detalle y acción manual) | — |
| `update_failed` | fin de `update` con fallos, solo si `run_summary` no está activado | — |

Nunca se mandan dos avisos por la misma corrida. Con todos los eventos en `on` (valor por defecto), una corrida produce un mensaje (`update/report.rs`).

### 4.3 Cola

Si el envío falla, el mensaje se guarda en `/var/lib/pxu/outbox/` (un JSON por mensaje) y se reintenta al final de la corrida siguiente. Máximo 50 mensajes y 7 días.

---

## 5. Configuración (`/etc/pxu/config.toml`)

- Dueño root, modo `0600`; `pxu` **se niega a usar** un archivo que otro usuario pueda escribir o no sea de root (`config::perm_problem`; falla en cerrado, también si no se puede hacer `stat`). Sin archivo, `pxu` funciona en solo lectura y nada se actualiza.
- No existen comandos personalizados: los comandos son constantes del binario.
- Las claves desconocidas se avisan en `config check` y **bloquean** `update` y la TUI al escribir (`load_checked`); `logs_per_ct` y `keep_snapshots` están marcadas como obsoletas.

```toml
[general]
nice = 10                     # 0..19 para los procesos hijos
io_class = "idle"             # "idle" | "best-effort"
log_retention_days = 30       # días de logs en /var/log/pxu; 0 = no borrar; tope 3650

[check]
refresh = false               # por defecto NO refresca índices
refresh_min_age_hours = 24    # con refresh, solo si el índice es más viejo
timeout_s = 60
index_stale_days = 7          # índice más viejo ⇒ stale

[update]
auto_update_tags = []         # CT con alguno de estos tags de PVE se autoactualizan
stopped = "skip"              # "skip" | "start_and_restore"
require_fresh_check_minutes = 60
min_free_mb = 512
reboot = "never"              # "never" | "if-required"
apt_mode = "dist-upgrade"     # "dist-upgrade" | "upgrade"
on_failure = "continue"       # "continue" | "stop"
max_consecutive_failures = 2  # >= 1
timeout_min = 30
host_auto = false             # la corrida programada actualiza también el host
host_nvidia_auto = false      # …y el driver NVIDIA del host

[backup]
mode = "auto"                 # "auto" | "snapshot" | "vzdump" | "none"
storage = "local"             # storage de PVE para los vzdump
keep = 0                      # backups de pxu que se conservan por CT tras un update correcto (0 = temporales)

[apprise]
transport = "auto"            # "auto" | "api" | "cli" | "none"
api_url = ""
api_key = ""
urls = []                     # modo stateless / CLI
tag = ""
on = ["updates_found", "update_failed", "run_summary", "check_error"]
timeout_s = 10
retry_delays_ms = [2000, 8000]

[ui]
columns = ["type", "os", "cpu", "ram", "disk", "gpu", "upd", "auto"]   # opcionales: type os cpu ram disk tags gpu upd auto

# Overrides por CT (opcionales)
[[ct]]
vmid = 101
auto_update = true
nvidia_auto = false
refresh = false
backup = "snapshot"
keep = 2
```

Prioridad de `auto_update`: `[[ct]] auto_update` > tag de `auto_update_tags` > no. El host (vmid 0) no entra por aquí: `[[ct]] vmid = 0` con `auto_update` o `nvidia_auto` es un error de config; se usan `host_auto` / `host_nvidia_auto`.

---

## 6. CLI y timer

```
pxu                                   # TUI
pxu list   [--json] [--all-nodes]
pxu check  [--json] [--refresh] [--notify] [--ct 101,103] [vmid...]
pxu update [--json] [--yes] [--no-notify] [--ct 101,103] [vmid...]
pxu update --host [--yes]
pxu config check
pxu notify-test
```

- `update` sin argumentos actualiza los CT con `auto_update` que tengan un chequeo positivo reciente. Con `[update] host_auto`, empieza por el host. `--yes` / `-y` omite las confirmaciones (el timer lo usa); `--no-notify` suprime el aviso.
- `--json`: un documento por ejecución con `"schema": 1`.
- `pxu config check` valida TOML, valores, claves desconocidas, modo `0600` y dueño `root`; no imprime secretos.
- `--mock CASE` o `PXU_MOCK=CASE` solo existe en builds con la feature `mock` (§8); el binario de producción lo rechaza con exit 2.

**Códigos de salida**

| Código | Comando | Significado |
|---|---|---|
| `0` | todos | OK / sin updates / aviso enviado |
| `2` | todos | uso incorrecto, config inválida o permisos |
| `3` | `update` | otra corrida en curso |
| `5` | `notify-test` | el envío falló |
| `10` | `check` | hay updates |
| `12` | `check` | al menos un CT falló al chequearse (prevalece sobre 10) |
| `20` | `update` | corrida con fallos parciales |
| `30` | `update` | corrida detenida: cortacircuito o cancelada |

(`check.rs` y `update/mod.rs`, constantes `EXIT_*`.)

### Timer de systemd

Las unidades están en `dist/systemd/` (la misma fuente que embebe `share/schedule.rs` y que copia `install.sh --timer`):

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

La tecla `S` de la TUI guarda otro horario como un drop-in `pxu-update.timer.d` que reemplaza `OnCalendar`; `install.sh --uninstall` lo borra. El log de cada corrida programada y su retención: §7.

---

## 7. Logs

Un log por corrida: `/var/log/pxu/run-AAAAMMDD-HHMM[SS][-N].log` (hora UTC en el nombre; `update/runlog.rs`).

- Se crea al tomar el lock con una cabecera mínima (`pxu-run-log 1`, `started`, `node`, línea `---`) y crece mientras corre, con una sección por host, NVIDIA y CT y la hora local de cada línea.
- Al terminar se reescribe entero: cabecera completa (`duration`, `result`), las secciones y el mismo resumen que el aviso (una sola fuente, `update/report.rs`). Un log sin `result` (corte brusco) se muestra como `⛔ INCOMPLETE`.
- Retención por días: `[general] log_retention_days` (30; `0` = no borrar). Al terminar cada corrida se borran los `run-*.log` más viejos; `prune_in` solo toca nombres del formato de pxu. Los logs con formato anterior (`host-*.log`, `nvidia-*.log`, carpetas por CT) también caducan por días, pero la página Logs solo lista los `run-*.log`.
- El log enmascara las URLs con usuario y clave (`fmt::mask_url_creds`) y sustituye caracteres de control (`fmt::clean`).
- La TUI los lista y los abre con la tecla `l` (hasta `LIST_MAX = 1000` logs y `VIEW_MAX_LINES = 100 000` líneas); ver DESIGN §5.

---

## 8. Estructura del código y pruebas

### 8.1 Árbol

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

### 8.2 El trait `Host`

`share/host/mod.rs`. Todo el código de la librería habla con `&dyn Host`:

```rust
fn read_file / read_dir / exists / read_head / file_meta
fn guest_exec(vmid, argv, opts, sink)      // dentro del CT
fn host_cmd(argv, opts, sink)              // en el host
fn write_file / write_config / append_file / remove_file / remove_empty_dir
fn try_lock(path, create) / lock_dir
fn timer_status / validate_calendar / apply_schedule
fn set_update_enabled / set_update_target / now
```

- `RealHost` usa `/` y `std::process::Command`.
- `ReadOnlyHost` envuelve a cualquier host y solo deja pasar comandos de una **lista blanca**; las escrituras exigen opt-ins explícitos y con alcance (CT en curso, host, NVIDIA). Detalle de seguridad: [SECURITY-REVIEW.md](SECURITY-REVIEW.es.md).
- `MockHost` sirve un sistema de archivos virtual en memoria y responde a los comandos según un `Scenario`; lo arma el builder `World` (`share/mock/scenario.rs`). No hay datos versionados en disco.

### 8.3 Builder `World` y casos

Un caso declara solo lo que cambia (distro, encendido/apagado, storage con o sin snapshot, fallos inyectados, locks de ProxMenux, VM y guests de otro nodo, política de backup). Casos predefinidos: `demo`, `update-ok`, `update-fail`, `distros`, `cluster`, `failures`, `vms`, `host`. Los valores son inventados, no capturas de un servidor.

### 8.4 Tests

- Unitarios: parser `.conf`, protocolo `@@`, parsers de cada gestor, config, notificaciones (curl simulado), horario del timer.
- Integración: `check` y `update` completos contra `MockHost`; se comprueba la secuencia exacta de comandos, lo que también prueba que el modo solo lectura nunca emite `apt-get update`/`apk update`/etc. (`check_path_is_read_only`).
- TUI: capturas de `TestBackend` contra `tests/snapshots/`.
- `mise run test` corre los tests en los perfiles dev y `mock`; `mise run check` además comprueba que el binario de producción rechaza `--mock`. Ver [development.md](../development.es.md).
