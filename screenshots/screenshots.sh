#!/usr/bin/env bash
# Regenera los PNG de screenshots/ con el binario mock. Headless: tmux (sesión propia, 120x40) + freeze (ANSI -> PNG).
# No toca el escritorio ni se conecta a ningún servidor. Uso: mise run screenshots
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=target/mock/pxu
OUT=screenshots
S="tmux -L pxu-shots"
command -v freeze >/dev/null || { echo "falta freeze (mise install)"; exit 1; }
[ -x "$BIN" ] || { echo "falta $BIN (mise run mock)"; exit 1; }

shot() { # nombre: captura el panel con colores y lo pasa a PNG
  # freeze recorta la imagen a la línea más larga: se rellena cada línea al ancho del panel para no perder el centrado
  $S capture-pane -ep -t 0 | python3 -c 'import re,sys
for l in sys.stdin.read().split("\n"):
    l = l.replace("\x1b[39m", "\x1b[0m")  # freeze deja gris el borde que sigue a un texto atenuado
    print(l + " " * max(0, 120 - len(re.sub(r"\x1b\[[0-9;]*m", "", l))))' | freeze --language ansi --config user --theme dracula --font.family "JetBrainsMono Nerd Font" \
    --font.size 14 --line-height 1.2 --padding 20 --margin 0 --window=false --border.radius 8 -o "$OUT/$1.png" >/dev/null
  # opcional: paleta reducida (PNG8) para que cada captura pese ~10x menos; el texto sigue nítido
  if command -v magick >/dev/null; then magick "$OUT/$1.png" -colors 96 -define png:compression-level=9 "PNG8:$OUT/$1.png"; fi
  echo "$OUT/$1.png"
}
run() { # inicia una sesión nueva con el caso mock dado
  $S kill-server 2>/dev/null || true
  $S new-session -d -x 120 -y 40 "PXU_MOCK_NO_BADGE=1 $BIN --mock $1"
  sleep 1.5
}
keys() { $S send-keys -t 0 "$@"; sleep 0.8; }
trap '$S kill-server 2>/dev/null || true' EXIT

run demo;      shot 01-list
keys Enter;    shot 02-detail
run demo;      keys b; shot 03-backup
run demo;      keys o; shot 04-columns
run demo;      keys u; keys Enter; sleep 1; shot 05-update
run demo;      keys d; sleep 2; keys S; keys S; keys S; keys g; shot 06-storage
# 07: CT 131 (caso arr: disco real al 66% pero bloques del thin al 99.8%) → id ↑, g, 31 veces abajo, Enter
keys S; keys g; keys $(printf 'Down %.0s' {1..31}); keys Enter; sleep 1; shot 07-storage-detail
# 08-11: Config, ventana de notificaciones (con tag), su confirmación y el horario (todo con datos del mock)
run demo-scheduled; keys g; shot 08-config
keys n; keys Tab; keys Tab; keys Tab; keys -l system; shot 09-notifications
keys Enter; shot 10-notifications-confirm
run demo-scheduled; keys S; shot 11-schedule
# 12: `pxu update --host --nvidia --yes` (mini TUI «Live output» a media descarga, con la barra de progreso; caso mock host-nvidia)
run "host-nvidia update --host --nvidia --yes"; sleep 2; shot 12-nvidia
# 13: ventana del salto de rama mayor de `pxu update --host --nvidia --yes` (caso mock host-nvidia-branch: 595.x instalado, 600.x en el índice)
run "host-nvidia-branch update --host --nvidia --yes"; sleep 1; shot 13-nvidia-branch
