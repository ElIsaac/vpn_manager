#!/usr/bin/env bash
# Abre `vpn` (la TUI) dentro de un emulador de terminal, para el lanzador de
# aplicaciones del escritorio.
#
# Por qué existe: `Terminal=true` en el .desktop delega en el escritorio, que
# resuelve la terminal vía `xdg-terminal-exec` o una lista propia. En sesiones
# sin esa herramienta (p. ej. Hyprland sin xdg-terminal-exec instalado) eso
# falla o cae en una terminal mínima con otra paleta y otra fuente, y la TUI se
# ve distinta a como se ve en la terminal del usuario. Aquí se elige
# explícitamente, respetando primero lo que el usuario haya configurado.
set -euo pipefail

APP="${VPN_BIN:-/usr/local/bin/vpn}"

# `$TERMINAL` es la convención de facto para "mi terminal preferida"; si está
# definida, gana sobre cualquier heurística.
candidates=()
[[ -n "${TERMINAL:-}" ]] && candidates+=("$TERMINAL")
candidates+=(kitty ghostty alacritty foot wezterm ptyxis gnome-terminal konsole xfce4-terminal xterm)

for term in "${candidates[@]}"; do
  command -v "$term" >/dev/null 2>&1 || continue
  case "$(basename "$term")" in
    # --class/--title dejan que el compositor (Hyprland, etc.) le aplique reglas
    # propias a esta ventana en vez de tratarla como una terminal cualquiera.
    kitty)     exec "$term" --class vpn-tui --title VPN -e "$APP" ;;
    ghostty)   exec "$term" --class=vpn-tui --title=VPN -e "$APP" ;;
    alacritty) exec "$term" --class vpn-tui --title VPN -e "$APP" ;;
    foot)      exec "$term" --app-id=vpn-tui --title=VPN "$APP" ;;
    wezterm)   exec "$term" start --class vpn-tui -- "$APP" ;;
    ptyxis)    exec "$term" --title=VPN -- "$APP" ;;
    gnome-terminal) exec "$term" --title=VPN -- "$APP" ;;
    konsole)   exec "$term" --hide-menubar -p tabtitle=VPN -e "$APP" ;;
    xfce4-terminal) exec "$term" --title=VPN --command="$APP" ;;
    xterm)     exec "$term" -class vpn-tui -title VPN -e "$APP" ;;
    *)         exec "$term" -e "$APP" ;;
  esac
done

# Sin terminal disponible no hay nada que mostrar en pantalla: se avisa por los
# canales que sí ve un usuario de escritorio.
msg="No encontré ningún emulador de terminal para abrir vpn. Instala uno (kitty, alacritty, ghostty…) o define \$TERMINAL."
command -v notify-send >/dev/null 2>&1 && notify-send -u critical "VPN" "$msg"
echo "$msg" >&2
exit 1
