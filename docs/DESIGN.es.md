[English](DESIGN.md) | Español

# DESIGN — TUI de `pxu`

Referencia de la interfaz de terminal actual (`pxu` sin argumentos). Describe lo que hace el código; para la arquitectura (módulos, estado, modos de chequeo, máquina de estados de la actualización, configuración, systemd) ver [ARCHITECTURE.es.md](ARCHITECTURE.es.md). Los bloques de pantalla son copias literales de los snapshots de `tests/snapshots` (80×24); cada vista tiene también su versión 120×40. Los datos de esos snapshots son de la demo (`[MOCK]`), no de un servidor real.

Código: `src/tui/` — `app.rs` (estado y teclas), `views.rs` (dibujo), `logs.rs` (vista de logs), `sched.rs` / `apprise.rs` (diálogos de Config), `mod.rs` (bucle, ratón, guardado).

---

## 1. Principios

1. **Tabla densa, no tarjetas.** Una fila por guest, estilo `htop`/`k9s`: estado → recursos → updates.
2. **Reducer puro.** `App::on_key` (`app.rs:486`) no hace IO: devuelve una `Action` que ejecuta el bucle (`mod.rs`); `views::draw` solo dibuja. Por eso cada pantalla se prueba con `TestBackend` contra los snapshots (`PXU_UPDATE_SNAPSHOTS=1` los regenera).
3. **El color nunca es la única señal.** Estados en palabras (`run`, `stopped`), marcas en los pasos (`●` en curso, `✓` ok, `✗` fail, `»` skip, `·` pendiente), cursor con `>` además del video inverso. Funciona con `NO_COLOR`.
4. **Teclado primero, ratón opcional.** Todo se hace con teclado; cada pantalla declara sus teclas en el pie. El ratón solo actúa en la vista de logs (§4.7).
5. **Lo caro, bajo pedido.** La vista por defecto vive de archivos de `/etc/pve` y cgroups; el chequeo de paquetes solo corre con `c`/`C`.
6. **Todo cambio pide confirmación.** `u`, `a`, `S`, `N`, `L` y la fila del host abren un diálogo antes de escribir o ejecutar nada.

Mínimo cómodo: 80×24; el pie (`keybar`) y las columnas se adaptan al ancho (§3, §5.1).

## 2. Redibujo y muestreo

El bucle (`mod.rs`, ~380-470) dibuja en cada iteración y luego espera eventos con `poll`:

- **Sin trabajo activo:** espera hasta la próxima muestra de CPU. La columna CPU se muestrea 1 s tras abrir y luego cada 3 s (`CPU_FIRST` / `CPU_EVERY`, `mod.rs:369-370`); lee `cpu.stat` de los LXC en marcha, sin recargar el inventario. Muestra `—` sin dato y en CT apagados.
- **Con un chequeo, una corrida o un comando en curso:** `poll` de como mucho 150 ms para atender teclas y mensajes de los workers.
- El reloj del header (`HH:MM`) se actualiza en cada dibujo.
- **Corrida de update:** el tiempo del paso en curso se actualiza cuando llega una línea de log o cambia el estado (pie: `step times update on log lines or state changes`); no hay spinner. Cada línea del log en vivo lleva su hora.

## 3. Paleta, símbolos y pie de teclas

16 colores ANSI sobre el fondo del terminal (no se pinta fondo). Con `NO_COLOR` no hay colores y la selección sigue en video inverso con `>` en la primera columna.

| Rol | Uso |
|---|---|
| texto | por defecto |
| atenuado | `stopped`, notas, filas secundarias |
| verde | `run`, `ok`, `✓` |
| cian | acento: cursor, filtro activo, paso en curso `●`, updates > 0 |
| amarillo | aviso: security > 0, `warn`, índice `stale` |
| rojo + negrita | `FAIL`, `✗`, errores |
| magenta | tipo `VM` |
| negrita | títulos y encabezados |
| video inverso | fila seleccionada |

**Pie de teclas (`keybar`, `views.rs:61-81`).** Formato `[tecla] acción` separado por dos espacios. Si no cabe, descarta acciones enteras desde el final y conserva siempre las últimas `keep` (las de `?` y `q`). Todas las vistas con pie lo componen igual, de arriba abajo: una regla `─` de lado a lado, la línea de estado, otra regla `─` y las filas de teclas (dos en la lista, Config y detalle —la segunda con una nota—; una en las demás). La ayuda ocupa lo que haga falta. Las dos reglas restan 2 filas a la tabla; las filas visibles se calculan con el alto que queda. Header: título + badges (`[RO]`, `[MOCK]`) a la izquierda y reloj a la derecha (`badges`, `views.rs:134-143`). Tras el header, una línea de contexto (filtros, resumen) y una regla `─`.

Formato de datos: updates `n/s` (total/seguridad), tamaños `x/y GB`, edad del índice `5h`, `32d*` (con `*` = stale), duraciones `0m00s` / `45m 11s`. Texto de UI en inglés, estados en minúscula (`run`, `stopped`, `ok`) y resultados cortos (`ok`, `warn`, `FAIL`, `skip`, `canceled`).

## 4. Mapa de teclas

Prioridad (`on_key`, `app.rs:486-562`): `Ctrl-C` → modales (actualización, columnas, host, NVIDIA, auto-update, horario, Apprise, retención) → ayuda `?` → búsqueda → confirmación de refresco → tecla de la vista. Un modal captura todas las teclas.

`Ctrl-C` con una corrida de update activa abre el diálogo de cancelación (§7); sin corrida sale.

### 4.1 Lista (`list_key`, `app.rs:1015-1064`)

| Tecla | Acción |
|---|---|
| `↑↓` / `j k` | mover cursor |
| `PgUp PgDn` | 10 filas · `Home End` primera/última |
| `1 2 3 4` | filtro All / Running / Stopped / Updates |
| `/` | buscar por nombre, tag o id (`Enter` fija, `Esc` borra) |
| `s` | cicla el orden: vmid · name · updates · ram |
| `r` | recarga los `.conf` de `/etc/pve` (solo archivos) |
| `Enter` | detalle del guest |
| `c` | chequea los CT visibles (solo lectura) |
| `C` | chequeo con refresco de índices; pide `y` |
| `v` | ver el último resultado del chequeo (si existe o hay uno en curso) |
| `o` | elegir columnas visibles |
| `u` | actualizar los CT visibles con updates (pide confirmación); sobre la fila del host, actualiza el host |
| `a` | auto-update del CT / del host (pide confirmación); en un CT con GPU, ventana del driver NVIDIA |
| `g` | vista Config |
| `S` | diálogo de horario del timer |
| `l` | logs de las corridas |
| `?` / `q` | ayuda / salir · `Esc` borra la búsqueda |

### 4.2 Detalle (`detail_key`, `app.rs:1196-1223`)

`↑↓/jk` desplaza · `PgUp PgDn` 10 · `Home` arriba · `u` actualizar este guest · `r` recargar · `c` chequear · `a` auto-update · `g` Config · `S` horario · `?` ayuda · `Esc`/`Backspace` vuelve · `q` sale.

### 4.3 Resultado del chequeo (`check_key`, `app.rs:1139-1172`)

`↑↓/jk` · `PgUp PgDn` · `Home End` · `Enter` detalle del CT · `c` re-chequear · `C` (o `R`) refresco con `y` · `u` actualizar los CT con updates · `Esc`/`Backspace` vuelve (con un chequeo en curso, lo cancela) · `?` · `q`.

### 4.4 Corrida de update (`update_key`, `app.rs:932-`)

`x`, `q` o `Esc` con la corrida activa abren el diálogo de cancelación · `↑↓/jk` y `PgUp PgDn` recorren el log · `f` o `End` siguen el final · `Home` principio · `?` ayuda · `u` queda deshabilitado durante la corrida. Terminada la corrida: `Esc`/`Backspace` vuelve a la lista y `q` sale. No hay foco alterno ni `Tab`.

### 4.5 Diálogos

| Diálogo | Teclas |
|---|---|
| Confirmar update (`modal_key`) | `↑↓/jk` mover · `Space` incluir/excluir · `Enter` empezar · `Esc`/`q` cancelar |
| Confirmar update del host | `↑↓/jk` desplazar · `Enter` instalar · `Esc`/`q`/`n` cancelar |
| Driver NVIDIA | `Space` marcar · `Enter` guardar · `Esc`/`q`/`n` cerrar |
| Auto-update (`a`) | `y`/`Enter` guardar · `Esc`/`n`/`q` cancelar |
| Columnas (`o`) | `Space` mostrar/ocultar · `Enter` cerrar |
| Cancelar corrida | `Enter` terminar el CT en curso y parar · `Esc` seguir |
| Horario (`S`) | `Tab`/`↓` y `BackTab`/`↑` campo · `←/→/Space` cambiar · `Enter` validar · `Esc` cancelar; en la confirmación `y`/`Enter` guarda, `b`/`Backspace` vuelve a editar |
| Apprise (`N`) | igual que Horario |
| Retención (`L`) | dígitos (hasta 5) · `Backspace` · `Enter` guardar · `Esc` cancelar |
| Refresco (`C`) | `y` confirma; cualquier otra tecla cancela |
| Ayuda (`?`) | cualquier tecla cierra; `q` sale |

### 4.6 Config (`config_key`, `app.rs:682-696`)

`S` horario · `N` notificaciones (Apprise) · `L` retención de logs · `t` aviso de prueba · `l` logs · `r` releer · `?` · `Esc`/`Backspace` vuelve · `q` sale.

### 4.7 Logs (`logs.rs:80-140`)

Lista: `↑↓/jk` · `PgUp PgDn` (10) · `Home`/`g` primera · `End`/`G` última · `Enter` abrir · `Esc`/`Backspace`/`l` vuelve · `q` sale.
Visor: `↑↓/jk` · `PgUp PgDn`/`Space` página · `g`/`G` principio/final · `n`/`p` siguiente/anterior log · `q`/`Esc`/`Backspace` vuelve a la lista.
**Ratón** (solo en Logs; el bucle activa la captura al entrar y la desactiva al salir, `sync_mouse`, `mod.rs:317`): la rueda desplaza 3 líneas, un clic selecciona una fila y un doble clic (≤ 500 ms) la abre.

## 5. Pantallas

### 5.1 Lista principal

Columnas: `ID NAME STATE OS CPU RAM UPD AUTO`, más `TYPE`, `DISK`, `TAGS` y `GPU` según el ancho y la elección de `o` (`ID`, `NAME` y `STATE` siempre; la selección se guarda en `config.toml`). El pie de lista (`views.rs:446-451`) y la línea bajo la tabla (`▼ 21 below …`, resumen del guest seleccionado) cambian con la selección.

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
[r] reload  [a] auto  [o] cols  [g] config  [l] logs  [?] help  [q] quit
```

**La fila del host.** El Proxmox mismo aparece como fila `vmid 0` (`GuestKind::Host`) con su resumen de paquetes en el header (`host: 7 upd`). `u` sobre ella abre su propia confirmación (lista de paquetes agrupada por kernel/firmware/pve/security/other, aviso de reinicio, copia de `/etc` previa, sin rollback automático) y nunca entra en una corrida de CT. `a` sobre ella alterna `[update] host_auto`, el opt-in del timer. Antes de `c` el host aparece como "not reviewed yet: press c".

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
[r] reload  [a] auto  [o] cols  [g] config  [l] logs  [?] help  [q] quit
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
[r] reload  [a] auto  [o] cols  [g] config  [l] logs  [?] help  [q] quit
```

**GPU / NVIDIA.** La columna `GPU` (`NV ✓`, `AMD ✓`, `pass`, `—`) aparece cuando hay GPUs. `a` sobre el host o un CT con GPU abre la ventana del driver NVIDIA: marcar `[ ] Upgrade GPU driver` y `Enter` guarda `host_nvidia_auto`; el timer instala el driver `.run` tras actualizar el host, siempre sin reiniciar. Sin marcar no se guarda nada.

```
pxu [MOCK]                                       server · 5/6 up · 1 upd   00:16
[All 6]  Run 5   Stop 1   Upd 1  / search…
─┌ NVIDIA driver ─────────────────────────────────────────────────────────────┐─
 │ NVIDIA driver of CT 134 ct-134                                             │O
>│                                                                            │
 │ [ ] Upgrade GPU driver                                                     │
 │                                                                            │
 │ Installed:  595.112.07                                                     │
 │ Latest:     595.120.01 (newer, same 595 branch)                            │
 │                                                                            │
 │ When checked, the scheduled run (after the host update):                   │
 │   • downloads the newest 595.x and verifies it (sha256 + --check)          │
 │   • test-builds it, stops the GPU CTs, installs it with DKMS               │
 │   • checks nvidia-smi; GPU CTs get the same version (libs only)            │
 │   • this CT restarts after getting the libs                                │
 │ Skipped if a host reboot is pending. pxu never reboots the host.           │
 │                                                                            │
 │ Nothing is saved unless checked                                            │
 │                                                                            │
─│ [Space] check  [Enter] save  [Esc] close                                   │─
C└────────────────────────────────────────────────────────────────────────────┘
────────────────────────────────────────────────────────────────────────────────
[↑↓] move  [1-4] filter  [/] search  [s] sort  [c] check  [u] update (asks)
[r] reload  [a] auto  [o] cols  [g] config  [l] logs  [?] help  [q] quit
```

**Columnas (`o`):**

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
[r] reload  [a] auto  [o] cols  [g] config  [l] logs  [?] help  [q] quit
```

**Ayuda (`?`).** Texto de `draw_help` (`views.rs:1040-1085`), propio de cada vista:

```
px┌ help · any key closes ───────────────────────────────────────────────────┐16
[A│↑↓ / j k     move cursor                                                  │
──│PgUp PgDn    move 10 · Home End first/last                                │──
  │1 2 3 4      filter All / Running / Stopped / Updates                     │TO
> │/            search name, tag or id (Esc clears)                          │
  │s            cycle sort: vmid · name · updates · ram                      │
  │r            reload /etc/pve confs (files only)                           │
  │Enter        guest detail                                                 │s
  │c            check LXC updates (visible CTs, or all) · read-only          │
  │C            check + refresh package indexes (asks y to confirm)          │s
  │v            show the last check result                                   │s
  │o            choose visible columns (Space toggles; saved in config.toml) │
  │u            update the visible CTs with updates (asks first)             │s
  │             temporary backup, verify, auto-rollback on failure           │
  │a            toggle scheduled auto-update of the CT (asks first)          │s
  │             host row: host_auto · host/GPU CT: NVIDIA driver window      │
  │g / S        config view + timer state · S change schedule (asks first)   │
  │l            run logs (one file per update run)                           │
▼ │?            this help · q quit                                           │
──│Columns: UPD pending updates (all/security) · — no data yet (press c)     │──
> │AUTO yes = in the scheduled run · host row: u asks before updating        │
──│                                                                          │──
[↑│read-only: no command that writes can run                                 │
[r└──────────────────────────────────────────────────────────────────────────┘
```

### 5.2 Detalle

Dos paneles: configuración leída del `.conf` (izquierda) y estado/updates (derecha, desde 120 columnas). A 80 columnas queda el panel de configuración. Para una VM el detalle es solo informativo: pxu no actualiza VMs.

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
[↑↓/jk] scroll  [u] update (asks)  [r] reload  [a] auto  [?] help  [q] quit
config is read from the .conf file (no processes); runtime metrics come from cg…
```

### 5.3 Resultado del chequeo

Tabla `ID NAME PM INDEX UPD SEC REBOOT RESULT` y, debajo, el panel de paquetes del CT seleccionado. El pie muestra `a check is already running` mientras corre.

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
[↑↓] select  [Enter] detail  [c] re-check  [C] refresh  [?] help  [q] quit
```

### 5.4 Corrida de update

Cola con una marca por CT (`✓ ● · ✗ »`), contadores en la cabecera y log en vivo. Si hay un aviso, es **uno solo por corrida**: la línea `NOTE: the backup and the rollback cover only the CT's rootfs` del log y el `WARNING` del modal de confirmación (§5.5).

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

Corrida terminada con fallos (el cortacircuito detuvo la cola tras 2 fallos seguidos):

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

### 5.5 Confirmación antes de actualizar

Lista los CT con updates incluidos por defecto (`Space` los excluye), muestra el plan por guest y el alcance del backup: backup y rollback cubren solo el rootfs; los bind mounts / `mp` con `backup=0` no se guardan ni se revierten (ARCHITECTURE §3).

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
[r] reload  [a] auto  [o] cols  [g] config  [l] logs  [?] help  [q] quit
```

### 5.6 Config, horario y Apprise

Muestra el estado del timer `pxu-update` (unidades, activo, horario, próxima ejecución), la config (`/etc/pxu/config.toml`), CT con auto-update, `host_auto`, retención de logs y el estado de Apprise. `S` cambia el horario (escribe un drop-in de systemd), `N` edita Apprise (los secretos no se muestran), `L` los días de retención (0 = no borrar), `t` envía un aviso de prueba. El timer ejecuta `pxu check --notify` y luego `pxu update --yes`.

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
└──────────────────────────────────────────────────────────────────────────────┘
────────────────────────────────────────────────────────────────────────────────
[S] schedule  [N] notifications  [L] log retention  [t] test notice
────────────────────────────────────────────────────────────────────────────────
[S] schedule  [N] notifications  [L] log days  [l] logs  [?] help  [q] quit
the timer runs `pxu check` then `pxu update --yes` (host first if host_auto; th…
```

Edición de Apprise:

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
││   Notify on                [x] updates_found                               ││
││                            [x] update_failed                               ││
││                            [x] run_summary                                 ││
││                            [x] check_error                                 ││
││                                                                            ││
││ secrets are never shown; what you type appears as dots                     ││
││                                                                            ││
││ [Tab] field  [←/→/Space] change  [Enter] validate  [Esc] cancel            ││
│└────────────────────────────────────────────────────────────────────────────┘│
└──────────────────────────────────────────────────────────────────────────────┘
────────────────────────────────────────────────────────────────────────────────
[S] schedule  [N] notifications  [L] log retention  [t] test notice
────────────────────────────────────────────────────────────────────────────────
[S] schedule  [N] notifications  [L] log days  [l] logs  [?] help  [q] quit
the timer runs `pxu check` then `pxu update --yes` (host first if host_auto; th…
```

### 5.7 Logs

`l` desde Lista o Config abre la lista de logs de corridas (más nuevo primero, una fila por archivo de `/var/log/pxu`: fecha, duración, resultado, tamaño); `Enter` abre el visor.

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

## 6. Errores

Un solo lugar: la **línea de estado** (entre las dos reglas del pie), que se borra con la tecla siguiente. Los errores de config son persistentes y fuerzan modo solo lectura (§8). Mensajes reales (`app.rs`): `updates disabled in read-only mode (no valid config)`, `a run is already in progress`, `no CT with a positive check here: run [c] first`, `host not reviewed yet: press c`, `host update blocked: …`, `host: nothing to update`, `auto-update applies to local LXC and the host only`. Los errores no abren diálogos; solo las confirmaciones son modales.

## 7. Cancelación

`x` (también `q`, `Esc` o `Ctrl-C`) durante una corrida abre **Cancel run?**:

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

`Enter` marca stop-after: el CT en curso termina su ciclo (upgrade → verify → restore) y los pendientes pasan a `canceled`. **`dpkg`/`apt` nunca se matan a mitad de instalación.** En pasos no destructivos (pre-checks, refresh, backup) se mata al hijo. La corrida emite igualmente su `run_summary` por Apprise. En un chequeo, `Esc` cancela el sondeo en curso y los ya hechos conservan su resultado.

## 8. Modo solo lectura

Badge `[RO]` en el header. Se activa cuando la config falta o es inválida (permisos incorrectos, claves o valores desconocidos); `pxu update` en ese caso sale con código 2. Efectos: `u` queda deshabilitado (`updates disabled in read-only mode (no valid config)`). Siguen disponibles: listar, detalle, chequeo `c`, búsqueda, orden, columnas, logs y el aviso de prueba. `a` puede crear la config ausente (el modal lo avisa) y al guardar se sale de solo lectura. Internamente existe un modo forzado (`forced_ro`) que además rechaza `a`; ninguna opción de línea de comandos lo activa hoy (`main.rs` pasa `false`).

## 9. Límite sin verificar

El flujo de la ventana NVIDIA y la actualización del host están cubiertos por tests y snapshots, pero no se han ejecutado contra un Proxmox real.
