#!/usr/bin/env bash
# Instalador de vpn-tui: compila, instala el binario y configura sudoers.
#
#   ./install.sh            compila e instala en ~/.local/bin
#   sudo ./install.sh       además escribe /etc/sudoers.d/vpn-tui
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_USER="${SUDO_USER:-$USER}"
TARGET_HOME="$(getent passwd "$TARGET_USER" | cut -d: -f6)"
BIN_DIR="$TARGET_HOME/.local/bin"
SUDOERS_FILE="/etc/sudoers.d/vpn-tui"

info()  { printf '\033[36m→\033[0m %s\n' "$*"; }
ok()    { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[33m!\033[0m %s\n' "$*"; }
die()   { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

as_user() {
  if [[ $EUID -eq 0 && "$TARGET_USER" != root ]]; then
    sudo -u "$TARGET_USER" "$@"
  else
    "$@"
  fi
}

# --- dependencias -------------------------------------------------------------
check_deps() {
  as_user command -v cargo >/dev/null || die "falta cargo (instala Rust: https://rustup.rs)"
  for cmd in openvpn3 openfortivpn; do
    if command -v "$cmd" >/dev/null; then
      ok "$cmd disponible"
    else
      warn "$cmd no está instalado; los perfiles de ese tipo fallarán"
    fi
  done
}

# --- compilación --------------------------------------------------------------
build() {
  info "compilando en modo release…"
  as_user env -C "$HERE" cargo build --release --quiet
  ok "binario en $HERE/target/release/vpn-tui"
}

install_bin() {
  as_user mkdir -p "$BIN_DIR"
  as_user install -m 755 "$HERE/target/release/vpn-tui" "$BIN_DIR/vpn-tui"
  ok "instalado en $BIN_DIR/vpn-tui"
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR no está en tu PATH; añádelo a ~/.zshrc" ;;
  esac
}

# --- configuración ------------------------------------------------------------
install_config() {
  local target="$HERE/config.toml"
  if [[ -f "$target" ]]; then
    as_user chmod 600 "$target"
    ok "config.toml ya existe (chmod 600)"
  else
    as_user cp "$HERE/config.example.toml" "$target"
    as_user chmod 600 "$target"
    warn "creé config.toml desde la plantilla: edítalo con tus datos"
  fi
}

# --- sudoers ------------------------------------------------------------------
# openfortivpn necesita root, y la TUI no puede pedir contraseña (usa `sudo -n`).
# Las reglas de pkill están acotadas a la señal y al nombre exacto del proceso.
install_sudoers() {
  if [[ $EUID -ne 0 ]]; then
    warn "sin root: no toco $SUDOERS_FILE (vuelve a ejecutar con sudo si lo necesitas)"
    return
  fi

  local forti pkill_bin
  forti="$(command -v openfortivpn || true)"
  pkill_bin="$(command -v pkill || true)"
  [[ -n "$forti" ]] || { warn "openfortivpn no encontrado: omito sudoers"; return; }
  [[ -n "$pkill_bin" ]] || die "no encuentro pkill"

  local ipv6_rule=""
  local ipv6_script
  ipv6_script="$(sed -n 's/^ *ipv6_script *= *"\(.*\)"/\1/p' "$HERE/config.toml" 2>/dev/null | head -1)"
  if [[ -n "$ipv6_script" && -f "$ipv6_script" ]]; then
    ipv6_rule="$TARGET_USER ALL=(ALL) NOPASSWD: $ipv6_script"
  fi

  local tmp
  tmp="$(mktemp)"
  cat > "$tmp" <<EOF
# Generado por vpn-tui/install.sh
$TARGET_USER ALL=(ALL) NOPASSWD: $forti
$TARGET_USER ALL=(ALL) NOPASSWD: $pkill_bin -INT -x openfortivpn
$TARGET_USER ALL=(ALL) NOPASSWD: $pkill_bin -TERM -x openfortivpn
$TARGET_USER ALL=(ALL) NOPASSWD: $pkill_bin -KILL -x openfortivpn
$ipv6_rule
EOF

  visudo -cqf "$tmp" || { rm -f "$tmp"; die "sudoers inválido, no instalé nada"; }
  install -o root -g root -m 440 "$tmp" "$SUDOERS_FILE"
  rm -f "$tmp"
  ok "reglas sudo en $SUDOERS_FILE"
}

main() {
  info "vpn-tui → usuario $TARGET_USER"
  check_deps
  build
  install_bin
  install_config
  install_sudoers
  echo
  ok "listo — ejecuta: vpn-tui"
}

main "$@"
