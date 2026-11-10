#!/bin/sh
# Installs pxu on a Proxmox host (run as root on the host).
# Usage: ./install.sh [options] [path-to-binary]
set -eu

usage() {
    cat <<'USAGE'
Usage: ./install.sh [--timer] [--force] [--uninstall [--purge]] [path-to-binary]
  path-to-binary   defaults to ./pxu next to the script, else target/release/pxu
  --timer          install and enable pxu-update.timer (systemd)
  --force          do not require the machine to be Proxmox
  --uninstall      remove the binary and units; keep /etc/pxu and /var/lib/pxu
  --purge          with --uninstall, also delete /etc/pxu, /var/lib/pxu and /var/log/pxu
Variable DESTDIR: install under another root (testing); requires no root or Proxmox and skips systemctl.
USAGE
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
        -*) usage >&2; die "unknown option: $a" ;;
        *) [ -z "$bin" ] || die "only one binary path is allowed"; bin=$a ;;
    esac
done
[ "$purge" = 0 ] || [ "$uninstall" = 1 ] || die "--purge only works with --uninstall"

DESTDIR=${DESTDIR:-}
bindir=$DESTDIR/usr/local/bin
unitdir=$DESTDIR/etc/systemd/system
units="pxu-update.service pxu-update.timer"
dirs="$DESTDIR/etc/pxu $DESTDIR/var/lib/pxu $DESTDIR/var/log/pxu"

if [ -z "$DESTDIR" ]; then
    [ "$(id -u)" = 0 ] || die "must be run as root"
    if [ "$force" = 0 ] && { ! command -v pct >/dev/null 2>&1 || [ ! -d /etc/pve ]; }; then
        die "this machine does not look like Proxmox (pct or /etc/pve is missing); use --force to install anyway"
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
    # drop-in with the schedule the TUI saves (S key)
    rm -rf "$unitdir/pxu-update.timer.d"
    systemctl_do daemon-reload
    rm -f "$bindir/pxu"
    say "binary and units removed"
    if [ "$purge" = 1 ]; then
        # shellcheck disable=SC2086
        rm -rf $dirs
        say "deleted /etc/pxu, /var/lib/pxu and /var/log/pxu"
    else
        say "keeping /etc/pxu and /var/lib/pxu (use --purge to delete them)"
    fi
    exit 0
fi

if [ -z "$bin" ]; then
    if [ -f "$here/pxu" ]; then bin=$here/pxu
    elif [ -f "$here/target/release/pxu" ]; then bin=$here/target/release/pxu
    else die "cannot find the binary (./pxu or target/release/pxu); pass its path"; fi
fi
[ -f "$bin" ] || die "binary does not exist: $bin"
"$bin" --help >/dev/null 2>&1 || die "the binary does not run on this machine: $bin"

if [ "$timer" = 1 ]; then
    src=${PXU_UNITS:-$here/dist/systemd}
    for u in $units; do [ -f "$src/$u" ] || die "missing unit $src/$u"; done
fi

# shellcheck disable=SC2086
install -d -m 0700 $dirs
install -d -m 0755 "$bindir"
tmp=$bindir/.pxu.new.$$
trap 'rm -f "$tmp"' EXIT INT TERM
install -m 0755 "$bin" "$tmp"
mv -f "$tmp" "$bindir/pxu"
say "binary installed at /usr/local/bin/pxu"

if [ -f "$DESTDIR/etc/pxu/config.toml" ]; then
    say "existing config.toml left untouched"
else
    say "no config.toml: pxu stays read-only (see README)"
fi

if [ "$timer" = 1 ]; then
    install -d -m 0755 "$unitdir"
    for u in $units; do install -m 0644 "$src/$u" "$unitdir/$u"; done
    systemctl_do daemon-reload
    systemctl_do enable --now pxu-update.timer
    say "pxu-update timer enabled"
fi
say "done"
