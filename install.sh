#!/bin/sh
# Installs or updates pxu on a Proxmox host (run as root on the host).
# Usage: ./install.sh [options] [path-to-binary]
# Without a local binary it downloads the release bundle from GitHub and checks it against the
# release's SHA256SUMS and verifies its GitHub build attestation with `cosign` (installed through apt
# if missing, as are curl and tar) before extracting or installing anything. If pxu is already installed it says which
# version is there: the same version stops without changes, another one is replaced, together
# with the systemd units if they were installed. The timers are set up from the TUI (S key).
set -eu

# GitHub repository the releases are downloaded from (owner/name).
repo=${PXU_REPO:-vk2r/pxu}

usage() {
    cat <<'USAGE'
Usage: ./install.sh [--force] [--release vX.Y.Z] [--uninstall [--purge]] [path-to-binary]
  path-to-binary   defaults to ./pxu next to the script, else target/release/pxu; if neither
                   exists, the release bundle is downloaded from GitHub; its SHA-256 and its
                   GitHub build attestation (checked with `cosign`, installed through apt if
                   missing) are checked before it is extracted
  --release TAG    download that release instead of the latest one (implies downloading)
  --force          do not require the machine to be Proxmox
  --uninstall      remove the binary and units; keep /etc/pxu and /var/lib/pxu
  --purge          with --uninstall, also delete /etc/pxu, /var/lib/pxu and /var/log/pxu
Variable DESTDIR: install under another root (testing); requires no root or Proxmox and skips systemctl.
Variable PXU_SKIP_ATTESTATION=1: download without verifying the attestation (SHA-256 only, which cannot
detect a replaced release); it prints a warning. Without it, a missing cosign is installed through apt;
if that is not possible the install stops.
USAGE
}

die() { echo "error: $*" >&2; exit 1; }
say() { echo "pxu: $*"; }

here=$(cd "$(dirname "$0")" && pwd)
force=0 uninstall=0 purge=0 bin="" release="" want_release=0
for a in "$@"; do
    if [ "$want_release" = 1 ]; then release=$a; want_release=0; continue; fi
    case $a in
        --release) want_release=1 ;;
        --release=*) release=${a#--release=} ;;
        --force) force=1 ;;
        --uninstall) uninstall=1 ;;
        --purge) purge=1 ;;
        -h|--help) usage; exit 0 ;;
        -*) usage >&2; die "unknown option: $a" ;;
        *) [ -z "$bin" ] || die "only one binary path is allowed"; bin=$a ;;
    esac
done
[ "$want_release" = 0 ] || die "--release needs a tag, e.g. --release v1.0.2"
[ "$purge" = 0 ] || [ "$uninstall" = 1 ] || die "--purge only works with --uninstall"
[ -z "$release" ] || [ -z "$bin" ] || die "--release and a binary path cannot be used together"

DESTDIR=${DESTDIR:-}
bindir=$DESTDIR/usr/local/bin
unitdir=$DESTDIR/etc/systemd/system
units="pxu-update.service pxu-update-failed.service pxu-update-notify-failed.service pxu-update.timer pxu-notify-flush.service pxu-notify-flush.timer"
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
    if [ -f "$unitdir/pxu-notify-flush.timer" ]; then
        systemctl_do disable --now pxu-notify-flush.timer 2>/dev/null || true
    fi
    for u in $units; do rm -f "$unitdir/$u"; done
    rm -f "$bindir/pxu-failure-notice"
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

# Version of the pxu already installed, empty if there is none.
installed=""
if [ -x "$bindir/pxu" ]; then
    installed=$("$bindir/pxu" --version 2>/dev/null | awk '{print $2}') || installed=""
    [ -n "$installed" ] || installed=unknown
fi
# Units installed earlier from the TUI (S key): an update refreshes them.
[ -f "$unitdir/pxu-update.timer" ] && had_units=1 || had_units=0

# Stops when version $1 is the one installed.
stop_if_current() {
    [ "$installed" = "$1" ] || return 0
    say "pxu $1 is already installed and up to date; nothing to do"
    exit 0
}

work=""
cleanup() {
    [ -z "$work" ] || rm -rf "$work"
    [ -z "${tmp:-}" ] || rm -f "$tmp"
}
trap cleanup EXIT
trap 'exit 1' INT TERM

# Installs apt package $1 (needed for $2) as root, or stops with the command to run by hand.
apt_install() {
    say "installing $1 (needed $2)"
    if [ "$(id -u)" != 0 ] || ! command -v apt-get >/dev/null 2>&1; then
        die "$1 is needed $2 but cannot be installed here (needs root and apt-get); install it yourself (apt-get install -y $1)${3:-}"
    fi
    export DEBIAN_FRONTEND=noninteractive
    apt-get install -y "$1" >&2 || { apt-get update >&2 && apt-get install -y "$1" >&2; } || die "apt could not install $1; run: apt-get update && apt-get install -y $1${3:-}"
}

# Installs apt package $2 if command $1 is missing; $3 says what it is for.
need_tool() {
    command -v "$1" >/dev/null 2>&1 || apt_install "$2" "$3"
    command -v "$1" >/dev/null 2>&1 || die "$1 is still missing after installing $2"
}

# cosign >= 2.4.0 reads GitHub's attestation bundles; succeeds if the one on PATH is that new.
# Debian's package reports GitVersion "devel": then the package version (e.g. 2.5.0-2+b4) decides, and
# with no version at all cosign is not rejected (the verification itself will tell).
cosign_ok() {
    command -v cosign >/dev/null 2>&1 || return 1
    v=$(cosign version 2>&1 | sed -n 's/^GitVersion: *v\{0,1\}\([0-9][0-9]*\)\.\([0-9][0-9]*\).*/\1 \2/p' | head -n 1)
    if [ -z "$v" ] && command -v dpkg-query >/dev/null 2>&1; then
        v=$(dpkg-query -W -f='${Version}' cosign 2>/dev/null | sed -n 's/^\([0-9]*:\)\{0,1\}\([0-9][0-9]*\)\.\([0-9][0-9]*\).*/\2 \3/p' | head -n 1)
    fi
    [ -n "$v" ] || return 0
    set -- $v
    [ "$1" -gt 2 ] || { [ "$1" -eq 2 ] && [ "$2" -ge 4 ]; }
}

# Makes sure cosign >= 2.4.0 is there, installing or upgrading it through apt.
ensure_cosign() {
    cosign_ok && return 0
    skiphint="; or set PXU_SKIP_ATTESTATION=1 to skip the check knowingly (SHA-256 only)"
    if command -v cosign >/dev/null 2>&1; then
        say "cosign is older than 2.4.0; upgrading it"
    fi
    apt_install cosign "to verify the build attestation" "$skiphint"
    cosign_ok || die "cosign 2.4.0 or newer is needed to verify the build attestation, and apt provides an older one; install it from https://github.com/sigstore/cosign/releases$skiphint"
}

# Downloads the release bundle into $work, checks it against SHA256SUMS and its GitHub build
# attestation (cosign), and points $here and $bin at its contents.
download() {
    need_tool curl curl "to download pxu"
    need_tool tar tar "to extract pxu"
    need_tool sha256sum coreutils "to check the SHA-256 of the download"
    base=https://github.com/$repo/releases
    # HTTPS only, also across redirects
    dl() { curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL "$@"; }
    if [ -z "$release" ]; then
        # /releases/latest redirects to /releases/tag/<tag>.
        url=$(dl -o /dev/null -w '%{url_effective}' "$base/latest") || die "cannot reach $base/latest"
        release=${url##*/}
    fi
    case $release in
        v[0-9]*) ;;
        *) die "no release found at $base (got '$release')" ;;
    esac
    stop_if_current "${release#v}"
    name=pxu-${release#v}-x86_64-linux
    work=$(mktemp -d)
    say "downloading $release from $base"
    dl -o "$work/SHA256SUMS" "$base/download/$release/SHA256SUMS" || die "cannot download SHA256SUMS of $release"
    dl -o "$work/$name.tar.gz" "$base/download/$release/$name.tar.gz" || die "cannot download $name.tar.gz"
    line=$(grep "  $name.tar.gz\$" "$work/SHA256SUMS") || die "$name.tar.gz is not listed in SHA256SUMS"
    (cd "$work" && printf '%s\n' "$line" | sha256sum -c --quiet -) || die "SHA-256 of $name.tar.gz does not match SHA256SUMS"
    say "SHA-256 checked"
    # The SHA-256 comes from the same place as the tarball, so it only detects accidents. The attestation is
    # signed by GitHub for this repository's release workflow: it is verified BEFORE anything is extracted.
    if [ "${PXU_SKIP_ATTESTATION:-0}" = 1 ]; then
        say "WARNING: PXU_SKIP_ATTESTATION=1: the build attestation was NOT verified (SHA-256 only)"
    else
        ensure_cosign
        dl -o "$work/provenance.sigstore.json" "$base/download/$release/provenance.sigstore.json" || die "cannot download provenance.sigstore.json of $release"
        # Identity required: GitHub Actions' issuer and this repository's release workflow on a tag.
        # --new-bundle-format is what cosign 2.x needs for GitHub's bundles (3.x accepts it, deprecated).
        cosign verify-blob-attestation --new-bundle-format --type slsaprovenance1 --bundle "$work/provenance.sigstore.json" \
            --certificate-oidc-issuer https://token.actions.githubusercontent.com \
            --certificate-identity-regexp "^https://github.com/$repo/\\.github/workflows/release\\.yml@refs/tags/v" \
            "$work/$name.tar.gz" >"$work/cosign.out" 2>&1 || { tail -n 15 "$work/cosign.out" >&2; die "the build attestation of $name.tar.gz is not valid for $repo; nothing was installed"; }
        say "build attestation verified"
    fi
    tar -xzf "$work/$name.tar.gz" -C "$work"
    here=$work/$name
    bin=$here/pxu
}

if [ -n "$release" ]; then
    download
elif [ -z "$bin" ]; then
    if [ -f "$here/pxu" ]; then bin=$here/pxu
    elif [ -f "$here/target/release/pxu" ]; then bin=$here/target/release/pxu
    else download; fi
fi
[ -f "$bin" ] || die "binary does not exist: $bin"
"$bin" --help >/dev/null 2>&1 || die "the binary does not run on this machine: $bin"
new=$("$bin" --version 2>/dev/null | awk '{print $2}') || new=""
[ -n "$new" ] || die "cannot read the version of $bin"
stop_if_current "$new"
if [ -z "$installed" ]; then
    say "installing pxu $new"
elif [ "$installed" = unknown ]; then
    say "a pxu of unknown version is installed; replacing it with $new"
elif [ "$(printf '%s\n%s\n' "$installed" "$new" | sort -V | tail -n 1)" = "$new" ]; then
    say "pxu $installed is installed; updating to $new"
else
    say "pxu $installed is installed, newer than $new; going back to $new"
fi
# An update refreshes the units installed earlier, without enabling them again.
refresh_units=$had_units

if [ "$refresh_units" = 1 ]; then
    src=${PXU_UNITS:-$here/dist/systemd}
    for u in $units; do [ -f "$src/$u" ] || die "missing unit $src/$u"; done
    [ -f "$src/pxu-failure-notice" ] || die "missing $src/pxu-failure-notice"
fi

# shellcheck disable=SC2086
install -d -m 0700 $dirs
install -d -m 0755 "$bindir"
tmp=$bindir/.pxu.new.$$
install -m 0755 "$bin" "$tmp"
mv -f "$tmp" "$bindir/pxu"
say "pxu $new installed at /usr/local/bin/pxu"

if [ -f "$DESTDIR/etc/pxu/config.toml" ]; then
    say "existing config.toml left untouched"
else
    say "no config.toml: pxu stays read-only (see README)"
fi

if [ "$refresh_units" = 1 ]; then
    install -d -m 0755 "$unitdir"
    for u in $units; do install -m 0644 "$src/$u" "$unitdir/$u"; done
    install -m 0755 "$src/pxu-failure-notice" "$bindir/pxu-failure-notice"
    systemctl_do daemon-reload
    say "systemd units updated (timers left as they were)"
fi
say "done"
