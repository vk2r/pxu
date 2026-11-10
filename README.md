# pxu

pxu mantiene al día los contenedores (LXC) de tu servidor Proxmox. Muestra qué contenedores tienen actualizaciones, los actualiza de uno en uno y, si algo sale mal, **vuelve atrás solo** al estado anterior. Puede hacerlo cada día de forma automática y avisarte por Fluxer u otro servicio compatible con Apprise.

Se usa desde una pantalla en la terminal (la «TUI») y no deja nada corriendo en segundo plano.

> Versión 0.1, en desarrollo. Probado en Proxmox VE 9.2.

## Requisitos

- Un servidor Proxmox VE, y acceso como `root`.
- Para los avisos (opcional): un servidor [Apprise API](https://github.com/caronc/apprise-api) accesible desde Proxmox.

## Instalación

Por ahora no hay descargas publicadas: el programa se compila en otro equipo (con Rust, vía [mise](https://mise.jdx.dev)) y se copia a Proxmox.

1. En el equipo con la carpeta del proyecto:

```sh
mise install        # una sola vez
mise run build      # deja el programa en target/release/pxu
```

2. Copia al Proxmox, como prefieras, `target/release/pxu`, `install.sh` y la carpeta `dist/` (que `install.sh` busca junto a sí mismo). Por ejemplo, con `scp`:

```sh
ssh root@tu-proxmox mkdir -p /root/pxu-install
scp target/release/pxu install.sh root@tu-proxmox:/root/pxu-install/
scp -r dist root@tu-proxmox:/root/pxu-install/
```

3. En el Proxmox, como root:

```sh
cd /root/pxu-install && sh install.sh ./pxu --timer
```

Sin `--timer` se instala solo el programa, sin la corrida diaria.

Se puede repetir sin problema y nunca sobrescribe tu configuración. Si el equipo no parece Proxmox, el instalador se niega; `--force` lo obliga.

Quedan instalados:
- el programa en `/usr/local/bin/pxu`
- tu configuración en `/etc/pxu/config.toml` (la creas tú; ver abajo)
- los logs en `/var/log/pxu/`

## Primer uso

En el servidor, como root:

```sh
pxu
```

Sin archivo de configuración, pxu funciona en **solo lectura**: puedes ver tus contenedores y comprobar actualizaciones, pero no actualiza nada. Para empezar a actualizar, marca un contenedor con `a` (ver abajo); pxu crea el archivo por ti, tras pedirte confirmación.

## La pantalla (TUI)

Pulsa `?` en cualquier pantalla para ver la ayuda con las teclas. Todo se maneja con el teclado.

### Lista de contenedores

| Tecla | Qué hace |
|---|---|
| `↑` `↓` (o `j` `k`) | moverte |
| `/` | buscar |
| `s` | ordenar |
| `1` `2` `3` `4` | filtrar: todos / encendidos / apagados / con actualizaciones |
| `Enter` | ver el detalle de un contenedor |
| `c` | comprobar si hay actualizaciones |
| `C` | refrescar los índices del contenedor y comprobar (pide confirmación) |
| `u` | actualizar (pide confirmación) |
| `a` | activar o desactivar la actualización automática de ese contenedor |
| `o` | elegir columnas |
| `r` | recargar la lista |
| `g` | pantalla Config |
| `l` | pantalla Logs |
| `S` | horario de la corrida diaria |
| `q` | salir |

### Actualizar (`u`)

Aparece una lista con los contenedores a actualizar. Con `↑↓` te mueves, con la barra espaciadora quitas o pones uno, y con `Enter` empieza. Mientras corre, `x` o `Esc` pide cancelar. Una vez dentro puedes desplazarte por el progreso con `↑↓`, `PgUp`/`PgDn`, y `f` vuelve al final.

Qué hace con cada contenedor, de uno en uno: copia de seguridad temporal → prueba en seco → actualización → verificación. Si falla, restaura la copia sola. Solo se actualizan los contenedores a los que hiciste una comprobación (`c`) hace menos de una hora.

### Config (`g`)

Muestra el estado de la corrida diaria, de los avisos y de la retención de logs. Desde aquí:

| Tecla | Qué hace |
|---|---|
| `S` | cambiar el horario de la corrida diaria |
| `N` | configurar los avisos (Apprise) |
| `t` | enviar un aviso de prueba |
| `L` | cambiar cuántos días se guardan los logs |
| `l` | abrir Logs |
| `Esc` | volver |

### Logs (`l`)

Lista cada corrida con su fecha, duración, resultado y tamaño, de la más nueva a la más vieja. `↑↓` para elegir, `Enter` para abrirla, `Esc` para volver. Dentro de un log: `↑↓`, `PgUp`/`PgDn` para desplazarte, `g`/`G` para ir al principio o al final, `n`/`p` para el log siguiente o anterior.

Resultados que verás: `✅ 5 ok`, `⚠️ 3 ok · 1 warn`, `❌` si algo falló, `⛔ STOPPED` si la corrida se detuvo, y `⛔ INCOMPLETE` si se cortó de golpe (por ejemplo, el servidor se apagó a mitad). Un log de una corrida que sigue en marcha también aparece como `⛔ INCOMPLETE` hasta que termina.

Al principio la lista dice «no run logs yet» hasta que haya una corrida.

## Corrida diaria automática

Con `--timer` en la instalación, pxu comprueba y actualiza cada día a las **04:30** (hora del servidor, con un retraso aleatorio de hasta 15 minutos). Actualiza los contenedores marcados con `auto_update` (tecla `a`) o con la etiqueta `autoupdate` de Proxmox.

Para cambiar la hora, o desactivarla: tecla `S` en la lista o en Config. Eliges diario, lunes a viernes o semanal, ves la próxima ejecución y confirmas.

Si el contenedor está apagado, por defecto se omite.

## Avisos (Fluxer / Apprise)

Cada corrida termina con **un solo aviso**: una tabla con el servidor primero y luego cada contenedor (resultado, paquetes, tiempo y qué hacer a mano si hace falta). El título resume la corrida, por ejemplo `pxu: update run on server — 3 ok, 1 warn`.

Para configurarlo: en Config (`g`) pulsa `N`, escribe la dirección de tu Apprise API y la clave, y confirma. Con `t` envías un aviso de prueba. Las claves nunca se muestran en pantalla.

## Log de cada corrida y retención

Cada corrida deja **un archivo de log** en `/var/log/pxu/run-AAAAMMDD-HHMM.log` (la hora del nombre es UTC), que se va escribiendo mientras corre. Ábrelo desde la pantalla Logs.

Los logs se borran solos pasados **30 días**. Para cambiarlo, pulsa `L` en Config; `0` significa no borrar nunca.

## Copias de seguridad y vuelta atrás

- Antes de actualizar un contenedor, pxu hace una copia temporal: una instantánea si el almacenamiento lo permite, si no una copia con vzdump.
- Si la actualización o la verificación fallan, pxu restaura el contenedor, lo enciende y borra la copia.
- Si la restauración falla o la actualización se pasa de tiempo, **la copia se conserva**, la corrida se detiene y el log y el aviso muestran el comando para restaurar a mano.
- La copia cubre solo el disco principal del contenedor. Los discos montados desde fuera (`/mnt/pve/...`) no entran.

## Actualizar pxu

Compila de nuevo con `mise run build`, copia otra vez los archivos y repite `sh install.sh ./pxu --timer` en el Proxmox. Tu configuración se conserva.

## Desinstalar

En el servidor, como root:

```sh
./install.sh --uninstall           # quita el programa y la corrida diaria; conserva configuración y logs
./install.sh --uninstall --purge   # además borra /etc/pxu, /var/lib/pxu y /var/log/pxu
```

## Problemas comunes

- **«hay que ejecutarlo como root»** al instalar: ejecuta `install.sh` como root en el servidor.
- **La TUI no deja actualizar y dice «read-only»** (solo lectura): falta `/etc/pxu/config.toml` o tiene errores. Marca un contenedor con `a`, o revisa el archivo con `pxu config check`. El archivo debe ser de root y con permisos `0600`.
- **«este equipo no parece Proxmox»** al instalar: ejecútalo en el servidor Proxmox, o usa `--force`.
- **No llegan avisos**: prueba con `t` en Config. Revisa la dirección de tu Apprise API.
- **Un contenedor no se actualiza**: tiene que estar marcado con `a` (o con la etiqueta `autoupdate`), y haberlo comprobado con `c` hace menos de una hora.
- **La lista de Logs está vacía**: aún no hubo ninguna corrida con esta versión. Los logs antiguos siguen en `/var/log/pxu` pero no aparecen en la lista.
- **Un log dice `⛔ INCOMPLETE`**: la corrida se cortó (o sigue en marcha). El detalle está en el propio log.

## Desarrollo

Para compilar, probar o contribuir: [development.md](development.md).

## Licencia

MIT
