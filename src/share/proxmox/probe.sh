# pxu probe v1: un solo exec por CT, solo lectura salvo refresh=1 ($1). POSIX sh (busybox ash).
# Protocolo de lineas "@@..." (ARCHITECTURE §2.1). Sin "@@end" => el resultado es error.
export LC_ALL=C
R=${1:-0}
echo "@@v 1"
OSID=unknown; OSVER=""; OSLIKE=""
if [ -r /etc/os-release ]; then
  OSID=$(. /etc/os-release >/dev/null 2>&1; echo "${ID:-unknown}")
  OSVER=$(. /etc/os-release >/dev/null 2>&1; echo "${VERSION_ID:-}")
  OSLIKE=$(. /etc/os-release >/dev/null 2>&1; echo "${ID_LIKE:-}")
fi
echo "@@os $OSID ${OSVER:-0} \"$OSLIKE\""
has() { command -v "$1" >/dev/null 2>&1; }
PM=none
for c in apt-get apk pacman dnf yum zypper; do
  if has $c; then PM=$c; break; fi
done
case $PM in apt-get) PM=apt;; yum) PM=dnf;; esac
echo "@@pm $PM"
NOW=$(date +%s 2>/dev/null || echo 0)
newest() { # mtime mas reciente de los archivos dados (o vacio)
  m=0
  for f in "$@"; do
    [ -e "$f" ] || continue
    t=$(stat -c %Y "$f" 2>/dev/null) || continue
    [ "$t" -gt "$m" ] && m=$t
  done
  [ "$m" -gt 0 ] && echo "$m"
}
case $PM in
  apt) T=$(newest /var/lib/apt/periodic/update-success-stamp /var/lib/apt/lists/*Packages* /var/lib/apt/lists/*InRelease);;
  apk) T=$(newest /var/cache/apk/*.tar.gz /lib/apk/db/installed);;
  pacman) T=$(newest /var/lib/pacman/sync/*.db);;
  dnf) T=$(newest /var/cache/dnf/*.solv /var/cache/dnf/*/repodata/repomd.xml /var/cache/yum/*/*/repomd.xml);;
  zypper) T=$(newest /var/cache/zypp/solv/*/solv);;
  *) T=;;
esac
if [ -n "$T" ] && [ "$NOW" -gt 0 ]; then echo "@@cache_age $((NOW - T))"; else echo "@@cache_age -1"; fi
NOTE=""
if [ "$R" = 1 ] && [ "$T" ] && [ "$((NOW - T))" -lt "${2:-0}" ]; then R=0; NOTE=fresh; fi
if [ "$R" = 1 ]; then
  case $PM in
    apt) if apt-get update -qq >/dev/null 2>&1; then echo "@@refresh ok"; else echo "@@refresh failed:$?"; fi;;
    apk) if apk update >/dev/null 2>&1; then echo "@@refresh ok"; else echo "@@refresh failed:$?"; fi;;
    pacman) if has checkupdates; then echo "@@refresh unavailable-checkupdates-used"; else echo "@@refresh unavailable"; fi;;
    dnf) if $(command -v dnf || command -v yum) -q makecache >/dev/null 2>&1; then echo "@@refresh ok"; else echo "@@refresh failed:$?"; fi;;
    zypper) if zypper -n -q refresh >/dev/null 2>&1; then echo "@@refresh ok"; else echo "@@refresh failed:$?"; fi;;
    *) echo "@@refresh skipped";;
  esac
else
  echo "@@refresh ${NOTE:-skipped}"
fi
RC=0
case $PM in
  apt)
    OUT=$(apt-get -s -o Debug::NoLocking=1 dist-upgrade 2>/dev/null); RC=$?
    echo "$OUT" | awk '/^Inst /{
      name=$2; cur="-"; i=3
      if ($3 ~ /^\[/) { cur=$3; gsub(/[\[\]]/, "", cur); i=4 }
      nw=$i; sub(/^\(/, "", nw)
      sec=($0 ~ /[Ss]ecurity/) ? 1 : 0
      print "@@pkg " name " " cur " " nw " " sec }'
    has apt-mark && apt-mark showhold 2>/dev/null | awk 'NF{print "@@held " $1}'
    ;;
  apk)
    OUT=$(apk --no-network list -u 2>/dev/null); RC=$?
    echo "$OUT" | awk '/upgradable from:/{
      nw=$1; cur=$NF; sub(/\]$/, "", cur)
      name=nw; sub(/-[0-9][^ ]*-r[0-9]+$/, "", name)
      print "@@pkg " name " " substr(cur, length(name)+2) " " substr(nw, length(name)+2) " 0" }'
    ;;
  pacman)
    # -Qu devuelve 1 cuando no hay updates: no es un error
    OUT=$(pacman -Qu 2>/dev/null)
    echo "$OUT" | awk '$3=="->"{print "@@pkg " $1 " " $2 " " $4 " 0"}'
    ;;
  dnf)
    BIN=$(command -v dnf || command -v yum)
    OUT=$($BIN -q -C check-update 2>/dev/null); RC=$?
    [ "$RC" = 100 ] && RC=0
    echo "$OUT" | awk '/^Obsoleting/{exit} NF>=3 && $1 ~ /\./ {n=$1; sub(/\.[^.]*$/, "", n); print "@@pkg " n " - " $2 " 0"}'
    ;;
  zypper)
    OUT=$(zypper --no-refresh -n -q list-updates 2>/dev/null); RC=$?
    echo "$OUT" | awk -F'|' '$1 ~ /^v/ {
      for (i=1;i<=NF;i++) gsub(/^ +| +$/, "", $i)
      print "@@pkg " $3 " " $4 " " $5 " 0" }'
    ;;
esac
if [ -e /run/reboot-required ] || [ -e /var/run/reboot-required ]; then echo "@@reboot_required 1"; else echo "@@reboot_required 0"; fi
echo "@@end $RC"
