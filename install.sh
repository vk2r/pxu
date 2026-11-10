#!/bin/sh
# Instala pxu en un host Proxmox (ejecutar como root en el host).
# Uso: ./install.sh [opciones] [ruta-al-binario]
set -eu

usage() {
    cat <<'USO'
Uso: ./install.sh [--timer] [--force] [--uninstall [--purge]] [ruta-al-binario]
  ruta-al-binario  por defecto ./pxu junto al script, si no target/release/pxu
  --timer          instala y activa pxu-update.timer (systemd)
  --force          no exige que el equipo sea Proxmox
  --uninstall      quita binario y unidades; conserva /etc/pxu y /var/lib/pxu
  --purge          con --uninstall, borra también /etc/pxu, /var/lib/pxu y /var/log/pxu
Variable DESTDIR: instala bajo otra raíz (pruebas); no exige root ni Proxmox ni usa systemctl.
USO
}

die() { echo "error: $*" >&2; exit 1; }
say() { echo "pxu: $*"; }

here=$(cd "$(dirname "$0")" && pwd)
timer=0 force=0 uninstall=0 purge=0 bin=""
for a in "$@"; do
    case $a in
        --timer) timer=1 ;;
        --force) force=1 ;;
        --uninstall) uninstall=1 ;;
        --purge) purge=1 ;;
        -h|--help) usage; exit 0 ;;
        -*) usage >&2; die "opción desconocida: $a" ;;
        *) [ -z "$bin" ] || die "solo se admite una ruta de binario"; bin=$a ;;
    esac
done
[ "$purge" = 0 ] || [ "$uninstall" = 1 ] || die "--purge solo se usa con --uninstall"

DESTDIR=${DESTDIR:-}
bindir=$DESTDIR/usr/local/bin
unitdir=$DESTDIR/etc/systemd/system
units="pxu-update.service pxu-update.timer"
dirs="$DESTDIR/etc/pxu $DESTDIR/var/lib/pxu $DESTDIR/var/log/pxu"

if [ -z "$DESTDIR" ]; then
    [ "$(id -u)" = 0 ] || die "hay que ejecutarlo como root"
    if [ "$force" = 0 ] && { ! command -v pct >/dev/null 2>&1 || [ ! -d /etc/pve ]; }; then
        die "este equipo no parece Proxmox (falta pct o /etc/pve); usa --force para instalar igual"
    fi
fi

systemctl_do() {
    if [ -n "$DESTDIR" ]; then return 0; fi
    systemctl "$@"
}

if [ "$uninstall" = 1 ]; then
    if [ -f "$unitdir/pxu-update.timer" ]; then
        systemctl_do disable --now pxu-update.timer 2>/dev/null || true
    fi
    for u in $units; do rm -f "$unitdir/$u"; done
    # drop-in con el horario que guarda la TUI (tecla S)
    rm -rf "$unitdir/pxu-update.timer.d"
    systemctl_do daemon-reload
    rm -f "$bindir/pxu"
    say "binario y unidades quitados"
    if [ "$purge" = 1 ]; then
        # shellcheck disable=SC2086
        rm -rf $dirs
        say "borrados /etc/pxu, /var/lib/pxu y /var/log/pxu"
    else
        say "se conservan /etc/pxu y /var/lib/pxu (usa --purge para borrarlos)"
    fi
    exit 0
fi

if [ -z "$bin" ]; then
    if [ -f "$here/pxu" ]; then bin=$here/pxu
    elif [ -f "$here/target/release/pxu" ]; then bin=$here/target/release/pxu
    else die "no encuentro el binario (./pxu ni target/release/pxu); pasa la ruta"; fi
fi
[ -f "$bin" ] || die "no existe el binario: $bin"
"$bin" --help >/dev/null 2>&1 || die "el binario no se ejecuta en este equipo: $bin"

if [ "$timer" = 1 ]; then
    src=${PXU_UNITS:-$here/dist/systemd}
    for u in $units; do [ -f "$src/$u" ] || die "falta la unidad $src/$u"; done
fi

# shellcheck disable=SC2086
install -d -m 0700 $dirs
install -d -m 0755 "$bindir"
tmp=$bindir/.pxu.new.$$
trap 'rm -f "$tmp"' EXIT INT TERM
install -m 0755 "$bin" "$tmp"
mv -f "$tmp" "$bindir/pxu"
say "binario instalado en /usr/local/bin/pxu"

if [ -f "$DESTDIR/etc/pxu/config.toml" ]; then
    say "config.toml existente: no se toca"
else
    say "sin config.toml: pxu queda en solo lectura (ver README)"
fi

if [ "$timer" = 1 ]; then
    install -d -m 0755 "$unitdir"
    for u in $units; do install -m 0644 "$src/$u" "$unitdir/$u"; done
    systemctl_do daemon-reload
    systemctl_do enable --now pxu-update.timer
    say "timer pxu-update activado"
fi
say "listo"
