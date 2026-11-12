[English](README.md) | Español

# pxu

pxu mantiene al día los contenedores (LXC) de tu servidor Proxmox. Muestra qué contenedores tienen actualizaciones, los actualiza de uno en uno y, si algo sale mal, **vuelve atrás solo** al estado anterior. Puede ejecutarse cada día y avisarte por cualquier servicio compatible con Apprise.

![Lista de contenedores con las columnas de actualizaciones, backup y auto-update](screenshots/01-list.png)

![Resultado de una actualización](screenshots/05-update.png)

![Página Storage: espacio por storage y por contenedor, con alertas](screenshots/06-storage.png)

Se usa desde una pantalla en la terminal (la «TUI»). No deja nada corriendo en segundo plano.

> Versión 0.1, en desarrollo. Probado en Proxmox VE 9.2.

## Requisitos

- Un servidor Proxmox VE, y acceso como `root`.
- Para los avisos (opcional): un servidor [Apprise API](https://github.com/caronc/apprise-api) accesible desde Proxmox.

## Instalar pxu

Compila pxu en otro equipo (con Rust, vía [mise](https://mise.jdx.dev)) e instálalo en Proxmox. Por ahora no hay versiones precompiladas.

1. Compila el binario:

   ```sh
   mise install        # una sola vez
   mise run build      # deja el programa en target/release/pxu
   ```

2. Copia `target/release/pxu`, `install.sh` y `dist/` a Proxmox. Por ejemplo, con `scp`:

   ```sh
   ssh root@tu-proxmox mkdir -p /root/pxu-install
   scp target/release/pxu install.sh root@tu-proxmox:/root/pxu-install/
   scp -r dist root@tu-proxmox:/root/pxu-install/
   ```

3. En Proxmox, como root:

   ```sh
   cd /root/pxu-install && sh install.sh ./pxu --timer
   ```

`--timer` activa la corrida diaria. Sin él, se instala solo el programa.
El instalador se puede repetir sin problema y nunca sobrescribe tu configuración. Para actualizar pxu, repite estos tres pasos.

Rutas: programa `/usr/local/bin/pxu`; configuración `/etc/pxu/config.toml` (la creas tú); logs `/var/log/pxu/`. El instalador crea `/etc/pxu`, `/var/lib/pxu` y `/var/log/pxu` con modo `0700`.

## Probar pxu en modo solo lectura

En el servidor, como root:

```sh
pxu
```

Sin archivo de configuración, pxu está en **solo lectura**: puedes ver tus contenedores y comprobar actualizaciones, pero no actualiza nada.
Usa `↑↓` para moverte, `c` para comprobar, `a` para marcar un contenedor, `u` para actualizar y `q` para salir. Pulsa `?` para ver todas las teclas.

## Elegir qué contenedores actualizar

**pxu solo actualiza los contenedores marcados con `a` o con una etiqueta listada en `auto_update_tags` (por ejemplo `autoupdate`). Los demás contenedores no se tocan.**

1. Pulsa `a` en un contenedor. pxu pide confirmación y crea el archivo de configuración.
2. Pulsa `c` para comprobar actualizaciones.
3. Pulsa `u` para actualizar. pxu pide confirmación antes.

Solo se actualizan los contenedores que comprobaste hace menos de una hora.

## Seguridad y vuelta atrás

Con cada contenedor, de uno en uno: copia temporal → simulación → actualización → verificación.

- La copia es una instantánea si el almacenamiento lo permite; si no, una copia con vzdump.
- Si la actualización o la verificación fallan, pxu restaura el contenedor, lo enciende y borra la copia.
- Si la restauración falla o la actualización se pasa de tiempo, **la copia se conserva**, la corrida se detiene y el log y el aviso muestran el comando para restaurar a mano.
- **La copia cubre solo el disco principal del contenedor.** Los discos montados desde fuera (`/mnt/pve/...`) no entran.

## Activar las actualizaciones automáticas

Con `--timer` en la instalación, pxu comprueba y actualiza cada día a las **04:30** (hora del servidor, con un retraso aleatorio de hasta 15 minutos). Actualiza los contenedores marcados como arriba. Los contenedores apagados se omiten por defecto.

Para cambiar la hora o desactivarla, pulsa `S` en la lista o en Config (`g`).

Cada corrida deja un log en `/var/log/pxu/`; ábrelo con `l`. Los logs se borran pasados 30 días (cámbialo con `d` en Config; `0` los conserva siempre).

## Configurar los avisos

Cada corrida termina con **un solo aviso**: una tabla con el servidor primero y luego cada contenedor.

1. En Config (`g`), pulsa `n`.
2. Escribe la dirección de tu Apprise API y la clave, y confirma.
3. Pulsa `t` para enviar un aviso de prueba.

Las claves nunca se muestran en pantalla.

## Desinstalar

En el servidor, como root:

```sh
./install.sh --uninstall           # quita el programa y la corrida diaria; conserva configuración y logs
./install.sh --uninstall --purge   # además borra /etc/pxu, /var/lib/pxu y /var/log/pxu
```

## Problemas comunes

- **"must be run as root"** («hay que ejecutarlo como root») al instalar: ejecuta `install.sh` como root en el servidor.
- **"this machine does not look like Proxmox"** («este equipo no parece Proxmox») al instalar: ejecútalo en el servidor Proxmox, o usa `--force`, que se salta esa comprobación.
- **La TUI no deja actualizar y dice «read-only»** (solo lectura): falta `/etc/pxu/config.toml` o tiene errores. Marca un contenedor con `a`, o revisa el archivo con `pxu config check`. El archivo debe ser de root y con permisos `0600`.
- **Un contenedor no se actualiza**: tiene que estar marcado con `a` (o tener una etiqueta listada en `auto_update_tags`), y haberlo comprobado con `c` hace menos de una hora.
- **No llegan avisos**: prueba con `t` en Config. Revisa la dirección de tu Apprise API.
- **La lista de Logs está vacía**: aún no hubo ninguna corrida con esta versión. Los logs antiguos siguen en `/var/log/pxu` pero no aparecen en la lista.
- **Un log dice `⛔ INCOMPLETE`**: la corrida se cortó (o sigue en marcha). El detalle está en el propio log.

## Más documentación

- [development.es.md](development.es.md): compilar, probar y contribuir.
- [docs/ARCHITECTURE.es.md](docs/ARCHITECTURE.es.md): cómo funciona pxu, configuración, línea de comandos, logs.
- [docs/DESIGN.es.md](docs/DESIGN.es.md): la TUI, pantalla por pantalla, con todas las teclas.
- [docs/SECURITY-REVIEW.es.md](docs/SECURITY-REVIEW.es.md): revisión de seguridad.

## Licencia

MIT
