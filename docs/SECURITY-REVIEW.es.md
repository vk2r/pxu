# Revisión de seguridad

[English](SECURITY-REVIEW.md) | Español

Estado de seguridad del código actual: superficie, modelo de amenazas y hallazgos con su estado. Cada estado está comprobado contra el código (archivo:línea o nombre del test). Las referencias de arquitectura están en [ARCHITECTURE](ARCHITECTURE.es.md) y las de interfaz en [DESIGN](DESIGN.es.md).

Revisión: confirmada el 2026-11-09 (`f726c66`, commit inicial). La fecha y el commit de la revisión original no quedaron registrados porque se aplanó el historial de git; los estados de abajo no se volvieron a comprobar contra este commit al reorganizar la documentación. Actualización 2026-11-12: los secretos de Apprise ahora se guardan cifrados (filas de abajo y [ARCHITECTURE §4.6](ARCHITECTURE.es.md#46-secretos-cifrado-con-systemd-creds)); no formaba parte de la revisión original y las referencias `archivo:línea` de los hallazgos pueden haberse movido.

Estados: **corregido** (la causa ya no existe), **mitigado** (el riesgo se reduce pero queda un residuo descrito), **aceptado** (decisión consciente), **abierto** (sin corregir).

---

## 1. Superficie y modelo de amenazas

**Qué corre y con qué privilegios.** `pxu` se ejecuta como root en el host Proxmox: a mano (TUI o CLI) o desde el timer systemd (`dist/systemd/pxu-update.service`: `pxu check --notify` y después `pxu update --yes`, con `Nice=10`, `IOSchedulingClass=idle`, `KillMode=mixed`). No hay daemon ni puerto de escucha. Un solo paquete: librería (`share/`) más CLI y TUI.

**Qué toca.**

| Zona | Qué hace | Cómo se limita |
|---|---|---|
| Lectura de PVE | Lee `/etc/pve/**.conf`, cgroups y pidfiles | Solo lectura de ficheros; nada de `pct list`/`qm list` |
| Sondeo en CT | Un `sh -c` constante (`probe.sh`) por CT con `lxc-attach` | `ReadOnlyHost` valida el argv interior (`readonly.rs:86`); sin refresco no escribe |
| Update de CT | Backup (snapshot/vzdump), upgrade, verificación y rollback | Opt-in explícito (`UpdateOptIn`, RAII) y escrituras atadas al CT en curso (`readonly.rs:120`) |
| Update del host Proxmox | `apt-get full-upgrade` del host, copia de `/etc` a `/var/backups/pxu` y snapshot ZFS si la raíz es ZFS; no reinicia | Opt-in propio (`HostUpdateOptIn`) separado del de los CT; lista blanca exacta (`hostupdate.rs:74`, `:85`); a mano exige TTY, desatendido solo con `[update] host_auto = true` |
| Driver NVIDIA | Descarga el `.run` de `download.nvidia.com`, comprueba su `sha256`, lo instala en el host y pone las librerías en todos los CT con GPU NVIDIA (uno apagado se arranca y se vuelve a apagar) | Opt-in `host_nvidia_auto` (apagado por defecto; el manual `--nvidia-reinstall` pide terminal y confirmación); permiso (`nvidia::Grant`) con versión, kernel y vmid concretos; solo sube dentro de la misma rama mayor; curl con `--proto =https` (`nvidia.rs:30`) |
| Avisos | `curl --config -` (Apprise API) o CLI `apprise` | Configuración y secretos por stdin, nunca en argv; `proto` y `max-filesize` fijos (`notify.rs:163`). `api_key` / `urls` se guardan cifradas (`systemd-creds`, `api_key_enc` / `urls_enc`) y se descifran justo antes de enviar; si no se pueden descifrar falla, nunca cae a texto plano. Root en el mismo host aún puede descifrar ([ARCHITECTURE §4.6](ARCHITECTURE.es.md#46-secretos-cifrado-con-systemd-creds)) |
| Estado propio | `/var/lib/pxu`, `/var/log/pxu` (logs de corrida), `/etc/pxu/config.toml` (desde la TUI), drop-in del timer | `own_state_path` (`readonly.rs:149`), escritura atómica con `O_EXCL\|O_NOFOLLOW` (`real.rs:158`), `write_config` y `apply_schedule` solo con su opt-in |

**Fronteras de confianza.**

- **CT → host.** Toda salida de un CT es dato no confiable: se sanea con `fmt::clean` antes de mostrarla, registrarla o enviarla (H1).
- **Config y registro → acciones.** `/etc/pxu/config.toml` decide qué se actualiza y guarda los secretos de Apprise (cifrados al guardar desde la TUI; los viejos en texto plano siguen funcionando y `config check` los avisa): root y `0600` o no se usa (`config.rs:296`). `/var/lib/pxu/backups.json` decide qué backups son de `pxu` y se valida al cargar (M4).
- **Usuarios locales sin privilegios.** Solo pueden afectar a los locks de `/run/pxu` (L3, cerrado). No hay entrada de datos suya hacia comandos de root.
- **Red.** Solo salen `curl` hacia la Apprise API configurada y las descargas de NVIDIA, que solo se hacen con opt-in.

**Garantías de diseño** (probadas): sin `sh -c` en el host (todo `Command` con argv); el binario de producción no lleva `MockHost` (M2); ningún comando de escritura pasa sin opt-in (`without_the_opt_in_the_timer_never_touches_nvidia`, `host_writes_need_their_own_optin_and_ct_optin_is_not_enough`, `without_explicit_opt_in_nothing_can_be_written`, `check_path_is_read_only`); `pxu` no mata nunca `apt`/`dpkg` a mitad (H2, `no_kill_on_timeout` también en host y NVIDIA).

**Límites conocidos.**

- Los comandos de `pct`, `vzdump`, `pvesm` y `lxc-attach` están probados con `MockHost`, no contra todos los tipos de storage de un Proxmox real.
- La descarga de NVIDIA se verifica con el `sha256` que publica el mismo servidor de NVIDIA; no hay una firma independiente.
- No se han revisado CVEs de dependencias con una herramienta automática. El árbol no tiene cliente HTTP ni TLS propios (solo `ratatui`, `crossterm`, `serde`, `toml`, `toml_edit`, `serde_json`, `lexopt`, `libc`; `Cargo.toml`).

---

## 2. Hallazgos

| ID | Sev. | Hallazgo | Estado | Prueba en el código |
|---|---|---|---|---|
| H1 | Alta | La salida de los CT se pintaba sin sanear: secuencias de escape en la TUI de root, en los logs y en los avisos | corregido | `fmt::clean` (`fmt.rs:17`) aplicado en `parse_probe` (`check.rs:52`), `CtResult::detail` (`check.rs:115`), `Ctx::say` (`update/mod.rs:276`), `trunc` (`fmt.rs:49`), informe y visor (`report.rs:32`, `runlog.rs:413`). Tests: `probe_output_cannot_carry_escape_sequences_into_the_ui`, test de `clean` en `fmt.rs:90` |
| H2 | Alta | El timeout del upgrade mataba `apt-get`/`dpkg` a medias | corregido | `no_kill_on_timeout: true` en el upgrade (`update/mod.rs:658`), en el host (`hostupdate.rs:246`) y en NVIDIA (`nvidia.rs:278`); `real.rs:67` no manda señal en ese caso. Al vencer: CT fallido, backup `held`, corrida detenida. Tests: `upgrade_is_never_run_with_a_killing_timeout`, `upgrade_timeout_marks_ct_failed_and_stops_the_run`, `scenario_timeout_never_rolls_back_and_holds_the_backup` |
| M1 | Media | Se ignoraba la reserva del lock de ProxMenux (contenido del fichero) y su marcador de chequeo inválido | corregido | `lock_ct` (`update/mod.rs:548-577`): fichero con contenido ⇒ skip, `proxmenux-lxc-check-invalid-<id>` ⇒ skip. Test: `proxmenux_reservation_and_invalid_marker_skip_the_ct`. Residuo: no interpreta el UPID; ante cualquier contenido omite el CT (falla en cerrado) |
| M2 | Media | Un build `--features mock` podía pisar `target/release/pxu` | corregido | `compile_error!` si se compila release con `mock` (`main.rs:27-28`); el release sin la feature responde `this build has no mock support` y sale con 2 (`main.rs:21-22`); la tarea `mise run check` lo comprueba (`mise.toml`) |
| M3 | Media | La TUI en `[RO]` permitía refrescar índices dentro de los CT | corregido | `start_check` fuerza un chequeo solo lectura en `[RO]` (`c` ya no refresca ahí). Test: `in_read_only_c_checks_without_refreshing_and_says_so` (`tui/tests.rs`) |
| M4 | Media | La lista de escritura de `update` no estaba atada al CT en curso y el registro de backups no se validaba | mitigado | Atado al CT: `host_cmd_is_update(argv, vmid)` (`readonly.rs:120`), `set_update_target` + `TargetScope` (`update/mod.rs:489,507`); `pvesm free` y `pct restore` exigen el vmid en el volid (`readonly.rs:111`). Registro: `entry_check` (`backup.rs:76`) descarta entradas que no cuadran. Tests: `update_writes_are_bound_to_the_ct_in_progress`, `tampered_registry_entries_are_dropped_and_never_reach_pvesm_free`. Residuo: al cargar no se comprueba dueño/modo de `backups.json`; lo protege el directorio `0700` de root (`real.rs:116`) |
| M5 | Media | Tras un timeout en un CT que `pxu` arrancó, el `pct shutdown` podía cortar `dpkg` | corregido | `update/mod.rs:519-521`: con `hold` el CT queda encendido y se avisa. Test: `timeout_on_a_ct_started_by_pxu_never_shuts_it_down` |
| L1 | Baja | `perm_problem` fallaba en abierto si `stat` fallaba | corregido | `config.rs:298-301` devuelve el problema (`cannot stat`). Test: `update_opt_in_is_revoked_on_every_exit` cubre la carga con `cannot stat` (`update/mod.rs:1192`) |
| L2 | Baja | La comprobación de permisos seguía symlinks | mitigado | `file_meta` usa `symlink_metadata` y rechaza symlinks (`real.rs:281-286`). Residuo: no se comprueba que `/etc/pxu` sea de root sin escritura de grupo/otros |
| L3 | Baja | Locks en `/run/lock` (1777) abiertos con `create` y sin `O_NOFOLLOW` | corregido | Los locks propios de pxu (`pxu-run`, `pxu-ct-<vmid>`, `pxu-state`, `pxu-outbox*`) están en `/run/pxu`, creado `0700` y validado antes de cada apertura (directorio real, de root, sin escritura de grupo/otros, sin symlink); `O_NOFOLLOW` en `try_lock`. Los locks de ProxMenux se siguen leyendo en `/run/lock` sin `O_CREAT`. Prueba: `own_locks_live_in_a_validated_private_dir` |
| L4 | Baja | Cualquier error al abrir el lock de ProxMenux se trataba como «nada que respetar» | corregido | Solo `NotFound` pasa; otro error omite el CT (`update/mod.rs:569-572`) |
| L5 | Baja | El lock por CT se soltaba antes de devolver el CT a `stopped` | corregido | `_locks` vive hasta después del `pct shutdown` (`update/mod.rs:508-518`) |
| L6 | Baja | `curl --config -` está clasificado de solo lectura aunque su stdin podría escribir archivos | corregido | El config generado escapa comillas y saltos de línea (`curl_q`) y fija `proto`, `proto-redir` y `max-filesize`. `ReadOnlyHost` además revisa el contenido de stdin con una lista cerrada (`curl_config_is_safe`): rechaza `output`, `config`, `upload-file`, `next`, URLs `file://`, `data = "@…"`, caracteres de control y la falta de stdin. Pruebas: `curl_config_filter_accepts_only_what_pxu_generates`, `curl_config_stdin_is_checked_not_just_argv` |
| L7 | Baja | La lista de lectura incluía comandos que `pxu` nunca ejecuta (`pvesh get` libre, `lxc-info`, `pct config`, `qm`…) | corregido | `host_cmd_is_read_only` (`readonly.rs:57-72`) solo admite `curl --config -`, `apprise` con forma fija, `pvesh get …/feature` validado, las lecturas de `hostcheck`/`PVE_VERSION` y el índice de NVIDIA |
| L8 | Baja | El opt-in de escritura no se retiraba en todas las salidas | corregido | Guards RAII `UpdateOptIn`, `HostUpdateOptIn`, `NvidiaOptIn` con `Drop` (`host/mod.rs:158-203`), usados en CLI (`cli/mod.rs:332,453,497`) y TUI (`tui/mod.rs:90,120`). Test: `update_opt_in_is_revoked_on_every_exit`. El release compila con `panic = "abort"` (`Cargo.toml`) |
| L9 | Baja | `/var/lib/pxu` y `outbox/` se creaban `0755` | corregido | `create_private_dirs` con `0o700` (`real.rs:111`), `check_private_dir` antes de escribir (`real.rs:116`) e `install -d -m 0700` en `install.sh:88` |
| L10 | Baja | `state.json` es lectura-modificación-escritura sin lock | corregido | `state::update_state` toma `pxu-state.lock` (en `/run/pxu`) durante todo el ciclo de leer, modificar y guardar; lo usan check (CLI y TUI), update, la actualización del host y los avisos, y `ReadOnlyHost` lo permite. Si no obtiene el lock en 10 s falla en vez de escribir sin él. Prueba: `concurrent_updates_never_lose_each_others_changes` |
| L11 | Baja | Los logs guardaban la salida cruda del CT, con posibles credenciales en URLs | corregido | `mask_url_creds(clean(l))` en `Ctx::say` (`update/mod.rs:276`; `fmt.rs:28`); los ficheros se escriben `0600`. Test: `masks_url_credentials` (`fmt.rs:101`) |
| L12 | Baja | Un registro de backups corrupto se descartaba en silencio y la siguiente escritura lo borraba | corregido | `Registry::load` (`backup.rs:97-115`): ausente ⇒ vacío; ilegible o corrupto ⇒ copia `backups.json.corrupt-<epoch>`, aviso y sin poda en esa corrida. Test: `corrupt_registry_is_quarantined_and_nothing_is_pruned` |
| L13 | Baja | `write_atomic` abría el `.tmp` sin `O_NOFOLLOW` ni comprobaba el directorio | corregido | `write_atomic` (`real.rs:158-170`): directorio real, del usuario actual y `0700`; `.tmp` con `O_EXCL\|O_NOFOLLOW`. Test: `write_atomic_refuses_unsafe_state_dirs_and_symlinked_tmp` |
| L14 | Baja | `--storage` de vzdump validado con un `safe_name` laxo y `numeric` aceptaba `0` | corregido | La rama de `vzdump` exige `is_pve_id(st)` (`readonly.rs:130`, `backup.rs:195`); `numeric` rechaza `0` y ceros a la izquierda (`readonly.rs:97`). Test: `backup_commands_have_exact_shapes_even_with_the_update_opt_in` |
| L15 | Baja | El opt-in de `update` desbloqueaba el sondeo con refresco de cualquier CT | corregido | En `guest_exec` el opt-in solo vale si el vmid es el CT en curso (`readonly.rs:168`); la TUI bloquea `c`/`C`/`R` mientras corre un update (`tui/app.rs:1107-1110`). Test: `tui/tests.rs:349` |
