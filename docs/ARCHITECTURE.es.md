# Architecture

[English](ARCHITECTURE.md) | Español

pxu es un actualizador liviano de contenedores LXC para Proxmox VE. Es un único binario en Rust con TUI (ratatui) y CLI. No tiene daemon ni dependencias en el host y ocupa unos pocos MB de RAM mientras corre.


Referencia técnica de cómo funciona `pxu` hoy. La TUI está descrita en [DESIGN](DESIGN.es.md); el análisis de seguridad, en [SECURITY-REVIEW](SECURITY-REVIEW.es.md). Instalación y uso: [README](../README.es.md); compilar y probar: [DEVELOPMENT](../DEVELOPMENT.es.md).

Dónde se probó: las pruebas automáticas corren contra un servidor simulado (`MockHost`, §8), y pxu corre a diario en un servidor real con Proxmox VE 9.2 y storages `dir`, `lvmthin` y `pbs`. Otros tipos de storage (ZFS, Ceph, NFS…) y otras versiones de Proxmox todavía no se han probado en un servidor real.

Estado: v0.1, en desarrollo. Probado en PVE 9.2 (actualización y vuelta atrás reales en un CT Debian 13). Las notificaciones Apprise y la vuelta atrás de un CT que estaba apagado solo están probadas con datos simulados.

---

## 0. Decisiones

| Tema | Decisión | Dónde se ve |
|---|---|---|
| Lenguaje / TUI | Rust + ratatui (crossterm); `libc` para locks, prioridades y señales | `Cargo.toml` |
| Proceso | **Sin daemon.** Se ejecuta, hace su trabajo y termina. La automatización es un timer de systemd | `dist/systemd/` |
| Concurrencia | **Sin runtime async.** La TUI lanza un hilo por trabajo (chequeo, update); la CLI trabaja en el hilo principal | `src/tui/mod.rs`, `src/cli/mod.rs` |
| Listado | Se lee `/etc/pve/**/*.conf` y `/sys/fs/cgroup`; sin lanzar procesos | §1 |
| Ejecución en CT | **Una sola ejecución por CT por chequeo** (script `probe.sh` embebido) con `lxc-attach` | §2.1 |
| Chequeo | Refresca los índices por defecto (`[check] refresh`); `--no-refresh` / `C` = solo lectura | §2.3 |
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
@@cache_age <segundos>                   # edad del índice DESPUÉS del refresco (0 si salió bien); -1 si no se sabe; se emite una vez, tras @@refresh
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

- **Refresco (por defecto):** `[check] refresh = true`. `pxu check` y la tecla `c` de la TUI refrescan primero el índice dentro de cada CT. Con `refresh_min_age_hours` (12) solo se refresca si el índice es más viejo (12 h y no 24: con un timer diario, un límite de 24 h se saltaría un día sí y otro no). `--refresh` se sigue aceptando.
- **Solo lectura:** `pxu check --no-refresh` o la tecla `C` de la TUI (también `c` en `[RO]`) no escriben nunca en los guests, ni con `[[ct]] refresh = true`. `[check] refresh = false` y `[[ct]] refresh = false` apagan el refresco desde la config. Muestra el estado del índice que ya tiene el CT, con su edad.
- Un índice más viejo que `[check] index_stale_days` (7) se marca `stale`.
- Los CT detenidos, las VM y los guests remotos no se chequean (`check::targets`).

### 2.4 Ejecución y persistencia

- Secuencial, un CT tras otro; el timeout por CT es `[check] timeout_s` (60 s; el triple con refresco).
- El resultado se guarda en `/var/lib/pxu/state.json` (escritura atómica): fecha, gestor, paquetes, nº de seguridad, edad del índice y una **huella** (FNV-1a de la lista ordenada `nombre=versión_nueva`, `state::fingerprint`).
- La huella evita repetir avisos: §4.2. Un chequeo es «reciente» para `update` si tiene menos de `[update] require_fresh_check_minutes` (60).
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
5. Al terminar: informe, log final y aviso único (§4.4).

Cancelación (Ctrl-C/SIGINT/SIGTERM/SIGHUP en la CLI, diálogo en la TUI): la corrida termina el CT en curso y se detiene; los siguientes quedan `cancelled` y el exit es 30. `dpkg`/`apt` nunca se matan por una cancelación. Excepción: antes de instalar (pre-checks/backup) la cancelación es inmediata: solo se señala a `vzdump` (SIGTERM; SIGKILL de su grupo a los 120 s), vzdump limpia su parcial/snapshot/lock (pxu lo verifica) y el CT queda `cancelled`, intacto.

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
   - **Chequeo de espacio:** antes de vzdump pxu estima el tamaño (usado del rootfs con `df -Pk /` + `mpN` no bind sin `backup=0` a su tamaño asignado, × `[backup] space_ratio`) y lo compara con el espacio libre del storage (`pvesh get /nodes/<nodo>/storage/<storage>/status`, solo lectura). Si quedaría menos de max(`min_free_gb`, 10 % del total), o el estado es ilegible, no se lanza vzdump: el CT falla con `backup skipped: needs ~X GB, storage S has Y GB free — CT untouched` y la corrida sigue con el siguiente CT (también en `pxu update` y en el auto-update).
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

`/var/lib/pxu/backups.json` anota cada backup de pxu con estado `active` (en curso; si pxu muere, la siguiente corrida lo pasa a `held`), `held` (nunca se borra) o `kept` (candidato a poda). Solo se borra lo que pxu creó: snapshots `pxu-<14 dígitos>[-N]` y volúmenes vzdump anotados en el registro. Snapshots ajenos (`manual-*`, `zfs_auto_*`…) no se tocan. Cada entrada se valida al cargar; un registro corrupto se aparta como `backups.json.corrupt-<epoch>` y esa corrida no poda nada. pxu solo borra los backups de su propio registro, y cada comando de escritura queda atado al CT que se está actualizando.

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

Solo con `[update] host_nvidia_auto = true` (host) y `[[ct]] nvidia_auto = true` (CT), o desde la casilla «Include GPU driver update» de la ventana de la TUI (`a`, junto a «Automatic updates»; solo guarda lo que cambió; marcarla en un CT activa también el flag del host, desmarcarla en un CT solo ese CT, desmarcar el auto-update la desmarca; la columna GPU muestra `+drv` si está activa). Aplica a un driver instalado con el `.run` oficial (existe `/usr/bin/nvidia-uninstall`). Descarga de `download.nvidia.com` el `.run` y su `.sha256sum`, verifica y solo sube dentro de la misma rama mayor. Los CT reciben las librerías de la versión que tiene el host. Nunca reinicia el host.

---

## 4. Avisos (Apprise)

### 4.1 Transporte (`share/notify.rs`)

| `[apprise] transport` | Comportamiento |
|---|---|
| `auto` (por defecto) | `api` si hay `api_url`; si no, `cli` si `apprise` está en el `PATH`; si no, `none` (avisos desactivados, con aviso visible) |
| `api` | `curl` hacia la Apprise API: con `api_key` es el modo stateful (`POST {api_url}/notify/<key>`, los destinos viven en la API); sin clave, stateless (`POST {api_url}/notify/` con `urls`) |
| `cli` | el CLI `apprise` con las URLs en la variable `APPRISE_URLS` |
| `none` | sin avisos |

`pxu` nunca instala Python ni pip. Los secretos van **fuera de argv**: a `curl` se le pasa su configuración y el cuerpo por stdin (`curl --config -`). Timeout `timeout_s` (10 s) y reintentos con las esperas de `retry_delays_ms` (por defecto 5 s, 30 s, 2 min y 5 min, ~7,5 min), solo ante errores transitorios (fallo de curl, HTTP 408/424/429/5xx); otro 4xx (clave o petición inválida) no se reintenta. `pxu check --notify` usa solo las dos primeras esperas para no demorar el update que sigue. Los mensajes de error se limpian de claves y URLs. `api_key` y `urls` se guardan cifradas y solo se descifran justo antes de enviar (§4.6).

### 4.2 Eventos (`[apprise] on`)

| Evento | Cuándo | Deduplicación |
|---|---|---|
| `updates_found` | tras `check --notify`: CT (y el host) con paquetes nuevos | solo si la huella cambió desde el último aviso; el host usa la misma huella con clave `0` |
| `check_error` | un CT no se pudo chequear, o falló la revisión del host | — |
| `run_summary` | fin de `update`: **un solo aviso por corrida** con tabla (host, NVIDIA y cada CT: resultado, paquetes, tiempo, detalle y acción manual) | — |
| `update_failed` | fin de `update` con fallos, solo si `run_summary` no está activado | — |

Nunca se mandan dos avisos por la misma corrida. Con todos los eventos en `on` (valor por defecto), una corrida produce un mensaje (`update/report.rs`).

### 4.3 Cola

Si el envío falla, el mensaje se guarda en `/var/lib/pxu/outbox/` (un JSON por mensaje) y se reintenta al final de la corrida siguiente y cada 15 minutos con `pxu notify-flush` (unidades `pxu-notify-flush.service` + `.timer`, `OnCalendar=*:0/15`, `Persistent=true`, instaladas por la tecla `S` de la TUI). `pxu notify-flush` solo escribe en la cola: imprime `outbox: N sent, M still queued`, sale 0 si la cola queda vacía y 5 si queda algo, y con la cola vacía no toca la red. Máximo 50 mensajes y 7 días.

### 4.4 Formato del aviso

Cada corrida de update (timer, `pxu update` o la TUI) termina con **un solo aviso**: una tabla Markdown (`Upgrades | Result | Packages | GPU driver | Time | Details | Manual`, el host primero y luego cada CT, también los fallidos y omitidos) bajo el título `pxu: update run on <host> — N ok, N warn, N failed · <dur>` (+ `STOPPED` si se detuvo, + `· NVIDIA failed`). Con más de 25 filas van primero las que no están ok, luego hasta 25 ok y `+N more ok (see log)`. La columna `Manual` solo aparece si alguna fila tiene algo que hacer a mano. El nivel del aviso es success, warning o failure según el peor resultado.

- Evento `run_summary`: se envía siempre al acabar la corrida. Evento `update_failed`: si solo está este (sin `run_summary`), el mismo aviso se envía solo cuando algún CT o el driver NVIDIA falló. Las configs viejas siguen valiendo.

### 4.5 Configuración desde la TUI

En la vista Config (`g`) la sección «Notifications (Apprise)» muestra el estado de `[apprise]` con los secretos tapados: la `api_url` solo con esquema y host, la `api_key` como `•••` con `(set, encrypted)` o `(set, plain text)` (y una nota para guardarla en `n` y cifrarla) y cada URL de destino como `tgram://…`; muestra también `tag` (o `—`).

- `n` abre el diálogo: dirección de la API, modo (clave guardada en Apprise API **o** URLs directas, separadas por coma), la etiqueta `tag` (texto visible, vacía = se borra la clave; letras, dígitos, `-_.,`; la Apprise API sin etiqueta solo avisa a los destinos sin etiqueta) y los eventos que avisan (`updates_found`, `update_failed`, `run_summary`, `check_error`). El secreto guardado no se muestra: con el campo vacío se conserva, y lo que escribes se pinta como puntos. Al guardar, el secreto se escribe **cifrado** (§4.6).
- Con Enter valida la config resultante entera con el mismo código que `pxu config check`; si no valida (o no queda ningún transporte utilizable) no escribe nada y dice por qué. Luego pide confirmar con «hoy → nuevo» (secretos tapados): `y`/Enter guarda, `b` vuelve a editar, Esc cancela.
- Guarda solo `[apprise]`: el resto de `config.toml` y sus comentarios se conservan, la escritura es atómica (root, `0600`) y, si el archivo no existe, lo crea. La clave y las URLs se excluyen entre sí: al cambiar de modo se quita la otra.
- `t` envía un aviso de prueba, igual que `pxu notify-test` (muestra «enviando…», y luego OK o el error con código 5, sin volcar secretos).
- Con `--read-only` la edición y la prueba se rechazan: la prueba sale por red, así que cuenta como acción.

### 4.6 Secretos: cifrado con `systemd-creds`

`systemd-creds` se lee **solo por stdout** (stderr va aparte: siempre avisa que la llave del host no está en un medio cifrado, y ese texto nunca debe quedar dentro del secreto). `seal` comprueba que la salida sea base64 y la descifra de vuelta: si el valor no coincide, falla con «round-trip check failed» y no se escribe nada. `unseal` solo quita el salto de línea final.

`[apprise] api_key` y `urls` son los únicos secretos que guarda pxu. Al guardarlos desde la TUI (`g` → `n`) quedan **cifrados**:

- **Cómo:** `systemd-creds encrypt --name=pxu-apprise --with-key=auto - -` (el secreto entra por stdin). `auto` usa el chip TPM2 si la máquina tiene uno utilizable y, si no, la llave del host `/var/lib/systemd/credential.secret` (la crea systemd, solo root). pxu no tiene criptografía propia ni una llave dentro del binario.
- **En el archivo:** el resultado (base64, una línea) se escribe como `api_key_enc` / `urls_enc` (las URLs se cifran una por línea) y la clave en claro `api_key` / `urls` se **borra** en la misma escritura atómica. Solo se conserva el secreto del modo elegido.
- **Cuándo se descifra:** solo justo antes de enviar (`notify::send_once`, la única entrada de todo aviso, prueba, vaciado de la cola y reintento): `systemd-creds decrypt --name=pxu-apprise - -`, entrada y salida por stdin/stdout. El valor descifrado vive en una copia de la config que se descarta tras el envío; nunca va a argv, logs, pantalla, la cola (outbox) ni a la lista de `ps`, y los mensajes de error se limpian de él.
- **Si no se puede descifrar** (sin `systemd-creds`, otra máquina, valor dañado): no se envía nada, el error es `cannot decrypt apprise secret: …` y el aviso queda en la cola (outbox) como cualquier fallo (no se reintenta en bucle: esperar no lo arregla). `pxu notify-test` y la TUI muestran el error. pxu **nunca** cae a texto plano en silencio.
- **Configs antiguas:** una `api_key` / `urls` en claro sigue funcionando. `pxu config check` y la vista Config avisan `stored in plain text — save it in Config → n to encrypt`; guardar en `n` (aunque el campo del secreto quede vacío) o `pxu config encrypt-secrets` cifra lo guardado. Si conviven un valor en claro y uno cifrado, gana el claro y `config check` avisa.
- **Vista Config:** `api_key : ••• (set, encrypted)` o `(set, plain text)`; la confirmación «hoy → nuevo» muestra `•••` y la misma palabra.

Qué protege, con honestidad: **respaldos, copias de `config.toml` y otras máquinas** (el texto cifrado está atado a la llave o al TPM de este host y al nombre `pxu-apprise`, así que no sirve en otro lado). Qué **no** protege: **root en el mismo host** puede ejecutar `systemd-creds decrypt --name=pxu-apprise` (o leer la memoria de pxu) y obtener el secreto; ningún programa lo puede impedir, porque la llave vive en la misma máquina. El archivo sigue en `0600` root. Un respaldo que también copie `/var/lib/systemd/credential.secret` se lleva la llave. El tráfico hacia la Apprise API es tan privado como su URL: con `http://` la clave viaja sin cifrar por la red.

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
refresh = true                # por defecto refresca los índices (false = chequeo solo lectura)
refresh_min_age_hours = 12    # con refresh, solo si el índice es más viejo
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
space_ratio = 1.0              # estimated backup size / used size (zstd 1:1)
min_free_gb = 5               # minimum spare GiB on the storage (or 10% of total, whichever is larger)

[apprise]
transport = "auto"            # "auto" | "api" | "cli" | "none"
api_url = ""
api_key = ""                 # en claro (configs antiguas); la TUI guarda api_key_enc (§4.6)
urls = []                     # modo stateless / CLI (en claro, igual)
api_key_enc = ""              # la escribe la TUI: api_key cifrada con systemd-creds
urls_enc = ""
tag = ""
on = ["updates_found", "update_failed", "run_summary", "check_error"]
timeout_s = 10
retry_delays_ms = [5000, 30000, 120000, 300000]

[ui]
columns = ["type", "os", "cpu", "ram", "disk", "gpu", "upd", "auto", "backup"]   # opcionales: type os cpu ram disk tags gpu upd auto backup (tags oculta por defecto)

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

### 5.1 Validación y edición desde la TUI

La TUI también escribe esos overrides: `a` alterna el `auto_update` del CT seleccionado (modal con «hoy: … → nuevo estado»; `y`/Enter confirma, Esc cancela). Solo cambia la línea `auto_update` del bloque `[[ct]]` de ese vmid, o añade un bloque al final; comentarios y orden se conservan. Se valida el TOML resultante y se escribe de forma atómica (root, `0600`). Un override `false` gana sobre el tag del CT.

`b` hace lo mismo con el backup previo al update: escribe `backup = "none"` en el bloque `[[ct]]` del CT (o añade el bloque), y al reactivarlo quita la clave `backup` para que el CT vuelva a seguir `[backup] mode` (el bloque se queda, con sus otras claves). El modal muestra el modo actual y de dónde viene (override `[[ct]]` o `[backup] mode`) y, al desactivar, avisa de que el update corre sin backup y sin rollback automático. El host (vmid 0) no se cambia aquí. La lista puede mostrar el modo efectivo en la columna opcional `BACKUP` (`auto`/`snap`/`vzdump`/`none`) y el detalle del CT también.

`pxu update` y la TUI validan la config igual que `pxu config check` antes de usarla: valores desconocidos (`on_failure = "Stop"`, `apt_mode = "Upgrade"`) o claves mal escritas (`on_falure`) son errores. `pxu update` sale con código 2 y los muestra; la TUI arranca en solo lectura con el motivo en la barra de estado. Los avisos (warnings) no bloquean.

---

## 6. CLI y timer

```
pxu                                   # TUI
pxu list   [--json] [--all-nodes]
pxu check  [--json] [--no-refresh] [--notify] [--ct 101,103] [vmid...]
pxu update [--json] [--yes] [--no-notify] [--ct 101,103] [vmid...]
pxu update --host [--yes]
pxu config check
pxu config encrypt-secrets
pxu notify-test
pxu notify-flush
pxu --version | -v
pxu --help | -h
```

- `update` sin argumentos actualiza los CT con `auto_update` que tengan un chequeo positivo reciente. Con `[update] host_auto`, empieza por el host. `--yes` / `-y` omite las confirmaciones (el timer lo usa); `--no-notify` suprime el aviso.
- `--json`: un documento por ejecución con `"schema": 1`.
- `pxu config check` valida TOML, valores, claves desconocidas, modo `0600` y dueño `root`; no imprime secretos y avisa de un secreto de `[apprise]` guardado en claro.
- `pxu config encrypt-secrets` cifra con `systemd-creds` la `api_key` / `urls` de `[apprise]` que estén en claro (mismo resultado que guardar en `n`; requiere root; sin nada que cifrar → exit 0).
- `--mock CASE` o `PXU_MOCK=CASE` solo existe en builds con la feature `mock` (§8); el binario de producción lo rechaza con exit 2.

**Códigos de salida**

| Código | Comando | Significado |
|---|---|---|
| `0` | todos | OK / sin updates / aviso enviado |
| `2` | todos | uso incorrecto, config inválida o permisos |
| `3` | `update` | otra corrida en curso |
| `5` | `notify-test`, `notify-flush` | el envío falló / quedan avisos en cola |
| `10` | `check` | hay updates |
| `12` | `check` | al menos un CT falló al chequearse (prevalece sobre 10) |
| `20` | `update` | corrida con fallos parciales |
| `30` | `update` | corrida detenida: cortacircuito o cancelada |

(`check.rs` y `update/mod.rs`, constantes `EXIT_*`.)

### Instalador (`install.sh`)

`install.sh [--force] [--release vX.Y.Z] [--uninstall [--purge]] [ruta-al-binario]`. Hay que ejecutarlo como root; sin `--force` exige que parezca un Proxmox (`pct` y `/etc/pve`). Se puede repetir sin problema.

- **Instala** el binario en `/usr/local/bin/pxu` (reemplazo atómico) y crea `/etc/pxu`, `/var/lib/pxu` y `/var/log/pxu` con modo `0700`. **Nunca toca** un `/etc/pxu/config.toml` existente; si no hay, avisa de que pxu queda en solo lectura.
- **`--uninstall`** deshabilita los timers, borra las unidades, el drop-in `pxu-update.timer.d` (el horario que guarda la TUI) y el binario; conserva `/etc/pxu`, `/var/lib/pxu` y `/var/log/pxu`. Con `--purge` borra también esos tres directorios.
- Variables para pruebas: `DESTDIR` (instala bajo otra raíz, sin root, sin systemctl) y `PXU_UNITS` (carpeta de las unidades).
- Para **actualizar** pxu basta con repetir la instalación: el binario se reemplaza y la configuración se conserva.
- Compilación, copia al servidor y primeros pasos: [README](../README.es.md).


### Timer de systemd

Las unidades están en `dist/systemd/` (la misma fuente que embebe `share/schedule.rs` y que copia `install.sh` al actualizar):

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

`dist/systemd/pxu-notify-flush.service` (`pxu notify-flush`, `Nice=10`, `IOSchedulingClass=idle`, `TimeoutStartSec=5min`) y `pxu-notify-flush.timer` (`OnCalendar=*:0/15`, `Persistent=true`) reenvían los avisos en cola; se instalan y activan junto con el timer de update.

La tecla `S` de la TUI guarda otro horario como un drop-in `pxu-update.timer.d` que reemplaza `OnCalendar`; `install.sh --uninstall` lo borra. El log de cada corrida programada y su retención: §7.

Puedes editar las unidades antes de instalarlas:

```sh
cat dist/systemd/pxu-update.service dist/systemd/pxu-update.timer
```

El `-` delante del `check` hace que su código 10 ("hay actualizaciones") no detenga la unidad.

### Programación desde la TUI

Sin tocar `systemctl` a mano: en la vista Config (`g`) se ve el estado real del timer (unidades instaladas, activado, horario efectivo y próxima ejecución), y `S` (también desde la lista o el detalle) abre el diálogo del horario:

- Activar o desactivar el timer, y elegir diario a HH:MM, lunes a viernes a HH:MM, semanal (día + HH:MM) o un `OnCalendar` libre. Las horas son las del servidor.
- Antes de confirmar valida el horario con `systemd-analyze calendar` y muestra la próxima ejecución; si no valida, no escribe nada.
- Tras la confirmación («hoy: … → nuevo»; `y`/Enter guarda, Esc cancela) escribe el drop-in `/etc/systemd/system/pxu-update.timer.d/schedule.conf` (`OnCalendar=` vacío + el nuevo, 0644, atómico), hace `systemctl daemon-reload` y `enable --now` o `disable --now`.
- Si las unidades no están instaladas, el diálogo las instala (son las mismas de `dist/systemd/`, embebidas en el binario).
- Avisa si no hay `config.toml` o ningún CT con auto-update (el update no haría nada) o si la config es inválida (fallaría con código 2). Rechazado en solo lectura forzado.

`install.sh --uninstall` borra también ese drop-in.

---

## 7. Logs

Un log por corrida: `/var/log/pxu/run-AAAAMMDD-HHMM[SS][-N].log` (hora UTC en el nombre; `update/runlog.rs`).

- Se crea al tomar el lock con una cabecera mínima (`pxu-run-log 1`, `started`, `node`, línea `---`) y crece mientras corre, con una sección por host, NVIDIA y CT y la hora local de cada línea. Se escribe también si la corrida se detiene o se cancela (con el motivo); `0600`.
- Al terminar se reescribe entero: cabecera completa (`duration`, `result`), las secciones y el mismo resumen que el aviso (una sola fuente, `update/report.rs`). Un log sin `result` (corte brusco) se muestra como `⛔ INCOMPLETE`.
- Retención por días: `[general] log_retention_days` (30; `0` = no borrar). Al terminar cada corrida se borran los `run-*.log` más viejos; `prune_in` solo toca nombres del formato de pxu. Los logs con formato anterior (`host-*.log`, `nvidia-*.log`, carpetas por CT) también caducan por días, pero la página Logs solo lista los `run-*.log`. Se borran también las carpetas `<vmid>` vacías del formato viejo; máximo 3650 días; solo se tocan archivos regulares con nombre de pxu (no se siguen enlaces simbólicos). `logs_per_ct` ya no existe: se acepta y se ignora (aviso de config obsoleta).
- El log enmascara las URLs con usuario y clave (`fmt::mask_url_creds`) y sustituye caracteres de control (`fmt::clean`).
- La TUI los lista y los abre con la tecla `l` (hasta `LIST_MAX = 1000` logs y `VIEW_MAX_LINES = 100 000` líneas); ver DESIGN §5.
- TUI: `l` abre la página **Logs** (fecha, duración, resultado y tamaño, del más nuevo al más viejo; ↑↓/jk, clic para elegir, doble clic o Enter para abrir). En el visor: flechas, PgUp/PgDn y rueda para desplazar, `g`/`G` principio/final, `n`/`p` log siguiente/anterior, `q`/Esc vuelve a la lista con el mismo log elegido. `d` en la vista Config edita `log_retention_days`.
- Resultados que muestra la página Logs: `✅ 5 ok`, `⚠️ 3 ok · 1 warn`, `❌` si algo falló, `⛔ STOPPED` si la corrida se detuvo, y `⛔ INCOMPLETE` si se cortó de golpe (por ejemplo, el servidor se apagó a mitad) o sigue en marcha. La lista dice «no run logs yet» hasta que haya una corrida.

---

## 8. Estructura del código y pruebas

### 8.1 Árbol

El árbol del código, las dependencias y el perfil de compilación están en [DEVELOPMENT](../DEVELOPMENT.es.md) («Mapa del código»). Las secciones siguientes describen el trait `Host`, el builder de pruebas y las pruebas.

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
- `ReadOnlyHost` envuelve a cualquier host y solo deja pasar comandos de una **lista blanca**; las escrituras exigen opt-ins explícitos y con alcance (CT en curso, host, NVIDIA). Detalle de seguridad: [SECURITY-REVIEW](SECURITY-REVIEW.es.md).
- `MockHost` sirve un sistema de archivos virtual en memoria y responde a los comandos según un `Scenario`; lo arma el builder `World` (`share/mock/scenario.rs`). No hay datos versionados en disco.

### 8.3 Builder `World` y casos

Un caso declara solo lo que cambia (distro, encendido/apagado, storage con o sin snapshot, fallos inyectados, locks de ProxMenux, VM y guests de otro nodo, política de backup). Casos predefinidos: `demo`, `update-ok`, `update-fail`, `distros`, `cluster`, `failures`, `vms`, `host`. Los valores son inventados, no capturas de un servidor. Cada test arma el suyo con el builder (`World::new().ct(Ct::new(101).distro(Distro::Alpine).stopped())…`).

### 8.4 Tests

- Unitarios: parser `.conf`, protocolo `@@`, parsers de cada gestor, config, notificaciones (curl simulado), horario del timer.
- Integración: `check` y `update` completos contra `MockHost`; se comprueba la secuencia exacta de comandos, lo que también prueba que el modo solo lectura nunca emite `apt-get update`/`apk update`/etc. (`check_path_is_read_only`).
- TUI: capturas de `TestBackend` contra `tests/snapshots/`.
- `mise run test` corre los tests en los perfiles dev y `mock`; `mise run check` además comprueba que el binario de producción rechaza `--mock`. Ver [DEVELOPMENT](../DEVELOPMENT.es.md).

## 9. Página Storage (`share/storage.rs`, `tui/storage.rs`)

- `share/storage.rs`: parsers (`parse_storage_list`, `parse_content`, `parse_api_guests`, `parse_lvs`, `parse_storage_cfg`), `read_base` + `read_content` (solo E/S; el content en paralelo), `build` (puro: datos crudos + guests → `StorageData`, alertas, orden, filtro) y `load` / `load_progressive` (envía primero un `StorageData` parcial y luego el completo). Los umbrales son constantes: `OLD_BACKUP_DAYS=30`, `FULL_DISK_PCT=98`, `FULL_STORAGE_PCT=90`.
- `tui/storage.rs`: `StorageState` (selección, orden, filtro de alertas, detalle) y teclas; el dibujo está en `tui/views.rs` (`draw_storage`, `storage_detail_lines`).
- Sin bloquear: `tui/mod.rs::spawn_storage` ejecuta `load` en un hilo de `std::thread::scope` y devuelve el resultado por un canal `mpsc`; `Action::OpenStorage` / `RefreshStorage`.
- Lecturas nuevas, todas en la allowlist de `ReadOnlyHost` (`share/host/readonly.rs`) con validación de argumentos (nodo e ids de storage verificados, vmids con `is_pve_id`; lo demás se rechaza, ver `storage_page_reads_are_exact_and_validated`):
  - `pvesh get /nodes/<node>/storage --output-format json`
  - `pvesh get /nodes/<node>/lxc --output-format json` y `…/qemu` (`disk`/`maxdisk` por guest: la fuente de USED/% y de la alerta de disco lleno, igual que la GUI de Proxmox)
  - `pvesh get /nodes/<node>/storage/<storage>/status --output-format json`
  - `pvesh get /nodes/<node>/storage/<storage>/content --output-format json`
  - `lvs --reportformat json --units b --nosuffix -o vg_name,lv_name,lv_size,data_percent,pool_lv,lv_attr`
  - más lectura de `/etc/pve/storage.cfg` y de los `.conf` de los guests (archivos ya legibles).
- Sin `pct exec`/`df` dentro de los CT: el uso de disco sale solo de `pvesh` (`disk`/`maxdisk`). El `Data%` de `lvs` son bloques del thin pool (incluye archivos borrados sin trim), así que va aparte (`DiskUse.thin_pct`) y solo alimenta el aviso de trim del detalle, nunca USED/%, el orden ni las alertas. Un guest apagado informa `disk = 0`: `—`. Una lectura fallida da `—`, no una pantalla de error.
- Mock: `share/mock/storage.rs` arma un fixture (dir `local`, `local-lvm` lvmthin, `local-zfs`, `remote-backup-3` con dedup) a partir del escenario con `World::storage_demo()`; `demo` incluye un CT sin backup, uno con backup viejo, uno al 99% (CT 133), uno con bloques thin al 99.8% pero uso real 66% (CT 131, el aviso de trim), uno apagado y uno con snapshot `pxu-*`.
