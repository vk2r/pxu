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
  $S capture-pane -ep -t 0 | freeze --language ansi --config user --theme dracula --font.family "JetBrainsMono Nerd Font" \
    --font.size 14 --line-height 1.2 --padding 20 --margin 0 --window=false --border.radius 8 -o "$OUT/$1.png" >/dev/null
  echo "$OUT/$1.png"
}
run() { # inicia una sesión nueva con el caso mock dado
  $S kill-server 2>/dev/null || true
  $S new-session -d -x 120 -y 40 "$BIN --mock $1"
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
