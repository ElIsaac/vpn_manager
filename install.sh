#!/usr/bin/env bash
# Instalador de vpn (antes vpn-tui): instala las dependencias del sistema
# (openvpn3, openfortivpn, tailscale), compila, instala el binario y
# configura sudoers.
#
#   ./install.sh            instala dependencias que falten + compila + instala en ~/.local/bin
#   sudo ./install.sh       además escribe /etc/sudoers.d/vpn-tui
#
# Soporta Fedora/RHEL-like (dnf) y Debian/Ubuntu-like (apt). El resto del
# sistema (systemd, sudoers) es igual en ambos.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_USER="${SUDO_USER:-$USER}"
TARGET_HOME="$(getent passwd "$TARGET_USER" | cut -d: -f6)"
BIN_DIR="$TARGET_HOME/.local/bin"
SUDOERS_FILE="/etc/sudoers.d/vpn-tui"
DB_PATH="${VPN_TUI_DB:-$TARGET_HOME/.local/share/vpn-tui/vpn-tui.db}"

info()  { printf '\033[36m→\033[0m %s\n' "$*"; }
ok()    { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[33m!\033[0m %s\n' "$*"; }
die()   { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

as_user() {
  # sudo -u arranca un shell no interactivo/no login: no lee .zshenv/.profile,
  # así que ~/.cargo/bin (rustup) no queda en PATH aunque el usuario sí lo
  # tenga en su sesión normal. Se agrega explícito para no depender de eso.
  if [[ $EUID -eq 0 && "$TARGET_USER" != root ]]; then
    sudo -u "$TARGET_USER" env "PATH=$TARGET_HOME/.cargo/bin:$PATH" "$@"
  else
    PATH="$TARGET_HOME/.cargo/bin:$PATH" "$@"
  fi
}

# --- distro --------------------------------------------------------------
PKG_MANAGER=""
DISTRO_CODENAME=""
if [[ -r /etc/os-release ]]; then
  # shellcheck source=/dev/null
  . /etc/os-release
  DISTRO_CODENAME="${VERSION_CODENAME:-}"
  case "${ID:-}:${ID_LIKE:-}" in
    fedora:*|*:*fedora*|rhel:*|*:*rhel*) PKG_MANAGER="dnf" ;;
    debian:*|ubuntu:*|*:*debian*) PKG_MANAGER="apt" ;;
  esac
fi
[[ -n "$PKG_MANAGER" ]] || warn "no reconozco el gestor de paquetes de esta distro; omito instalación de dependencias del sistema"

# --- openvpn3 / openfortivpn ----------------------------------------------
# openfortivpn es paquete oficial en Fedora y Debian/Ubuntu. openvpn3 NO está
# en los repos base de ninguna de las dos: en Fedora viene del COPR de
# terceros `dsommers/openvpn3`; en Debian/Ubuntu, del repo APT oficial del
# proyecto (`swupdate.openvpn.net`), documentado en
# https://community.openvpn.net/openvpn/wiki/OpenVPN3Linux.
install_openvpn3_fedora() {
  command -v openvpn3 >/dev/null && { ok "openvpn3 ya está instalado"; return; }
  info "habilitando COPR dsommers/openvpn3…"
  dnf -y copr enable dsommers/openvpn3
  dnf install -y openvpn3
  ok "openvpn3 instalado (COPR dsommers/openvpn3)"
}

install_openvpn3_debian() {
  command -v openvpn3 >/dev/null && { ok "openvpn3 ya está instalado"; return; }

  # Antes de tocar repos de terceros: algunas distros ya lo empaquetan nativo
  # (Kali `main`, Debian sid/testing) — siempre preferible al repo del vendor.
  apt-get update -qq
  if apt-get install -y openvpn3-client 2>/dev/null && command -v openvpn3 >/dev/null; then
    ok "openvpn3 instalado (paquete nativo de la distro: openvpn3-client)"
    return
  fi

  if [[ -z "$DISTRO_CODENAME" ]]; then
    warn "no pude determinar VERSION_CODENAME de /etc/os-release; omito openvpn3"
    return
  fi

  # El repo oficial de OpenVPN3 Linux (swupdate.openvpn.net) NO publica lista
  # para todos los codenames vigentes — confirmado con pruebas reales: 200 en
  # bullseye/buster/jammy/focal/lunar, 404 en bookworm/noble/trixie/sid y en
  # cualquier cosa que no sea un codename de Debian/Ubuntu (p. ej. kali-rolling,
  # que de todos modos ya se resuelve arriba por el paquete nativo). No se
  # inventa un codename de reemplazo (mezclar un .deb de otra versión es un
  # riesgo de compatibilidad, no una garantía) — si no hay lista para el
  # codename real, se omite con un aviso claro en vez de tronar todo el
  # instalador.
  local list_url="https://swupdate.openvpn.net/community/openvpn3/repos/openvpn3-${DISTRO_CODENAME}.list"
  local http_code
  http_code="$(curl -fsSL -o /dev/null -w '%{http_code}' "$list_url" 2>/dev/null || true)"
  if [[ "$http_code" != "200" ]]; then
    warn "el repo oficial de openvpn3-linux no tiene paquete para '$DISTRO_CODENAME' (HTTP $http_code en $list_url)"
    warn "omito openvpn3 — instálalo manualmente cuando haya soporte, o desde: https://community.openvpn.net/openvpn/wiki/OpenVPN3Linux"
    return
  fi

  info "agregando el repo APT oficial de OpenVPN3 Linux ($DISTRO_CODENAME)…"
  local key="/etc/apt/trusted.gpg.d/openvpn-repo-pkg-keyring.gpg"
  curl -fsSL https://swupdate.openvpn.net/repos/openvpn-repo-pkg-key.pub \
    | gpg --dearmor -o "$key"
  curl -fsSL -o /etc/apt/sources.list.d/openvpn3.list "$list_url"
  apt-get update -qq
  if apt-get install -y openvpn3; then
    ok "openvpn3 instalado (repo oficial swupdate.openvpn.net)"
  else
    warn "el repo de openvpn3 se agregó pero la instalación falló; revisa manualmente"
  fi
}

install_openfortivpn() {
  command -v openfortivpn >/dev/null && { ok "openfortivpn ya está instalado"; return; }
  case "$PKG_MANAGER" in
    dnf) dnf install -y openfortivpn ;;
    apt) apt-get update -qq && apt-get install -y openfortivpn ;;
  esac
  ok "openfortivpn instalado"
}

# --- tailscale -------------------------------------------------------------
# El instalador oficial de Tailscale (https://tailscale.com/docs/install/linux)
# ya detecta la distro y cubre tanto Fedora/dnf como Debian/Ubuntu/apt, así
# que no hace falta duplicar la lógica de repos por distro aquí.
install_tailscale() {
  command -v tailscale >/dev/null && { ok "tailscale ya está instalado"; return; }
  info "instalando tailscale (instalador oficial tailscale.com/install.sh)…"
  curl -fsSL https://tailscale.com/install.sh | sh
  ok "tailscale instalado"
}

install_system_deps() {
  if [[ $EUID -ne 0 ]]; then
    warn "sin root: no instalo dependencias del sistema (openvpn3/openfortivpn/tailscale)"
    warn "vuelve a ejecutar con sudo, o instálalas tú mismo"
    return
  fi
  case "$PKG_MANAGER" in
    dnf) install_openvpn3_fedora ;;
    apt) install_openvpn3_debian ;;
    *) warn "omito openvpn3: distro no reconocida" ;;
  esac
  install_openfortivpn
  install_tailscale
}

# --- compilación -----------------------------------------------------------
ensure_cargo() {
  as_user command -v cargo >/dev/null && return
  warn "cargo no está instalado para $TARGET_USER"
  if [[ ! -t 0 ]]; then
    die "falta cargo y no hay terminal para preguntar; instálalo: https://rustup.rs"
  fi
  read -rp "¿Instalar Rust (rustup) ahora para $TARGET_USER? [s/N] " reply
  case "$reply" in
    [sSyY]*)
      command -v curl >/dev/null || die "falta curl, no puedo instalar rustup"
      info "instalando rustup para $TARGET_USER…"
      as_user bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable'
      as_user command -v cargo >/dev/null || die "rustup se instaló pero no encuentro cargo; revisa manualmente"
      ok "cargo instalado"
      ;;
    *) die "falta cargo (instala Rust: https://rustup.rs)" ;;
  esac
}

build() {
  ensure_cargo
  info "compilando en modo release…"
  as_user env -C "$HERE" cargo build --release --quiet
  ok "binario en $HERE/target/release/vpn"
}

install_bin() {
  as_user mkdir -p "$BIN_DIR"
  as_user install -m 755 "$HERE/target/release/vpn" "$BIN_DIR/vpn"
  ok "instalado en $BIN_DIR/vpn"
  ensure_bin_on_path
}

# Bajo `sudo`, $PATH es el de root (secure_path), no el del usuario real, así
# que no sirve para detectar si falta $BIN_DIR — se revisa/edita el rc de cada
# shell directamente. Idempotente: si ya hay una línea con .local/bin, no toca
# nada.
append_line_as_user() {
  as_user bash -c 'printf "%s\n" "$1" >> "$2"' _ "$2" "$1"
}

ensure_bin_on_path() {
  local rc changed=0
  for rc in "$TARGET_HOME/.bashrc" "$TARGET_HOME/.zshrc"; do
    [[ -f "$rc" ]] || as_user touch "$rc"
    if ! grep -q '\.local/bin' "$rc" 2>/dev/null; then
      append_line_as_user "$rc" ''
      append_line_as_user "$rc" '# agregado por vpn-tui/install.sh'
      append_line_as_user "$rc" 'export PATH="$HOME/.local/bin:$PATH"'
      ok "agregué \$HOME/.local/bin al PATH en $(basename "$rc")"
      changed=1
    fi
  done
  [[ $changed -eq 1 ]] && warn "abre una terminal nueva (o: source ~/.zshrc / source ~/.bashrc)"
}

# --- alias viejo -------------------------------------------------------------
disable_old_alias() {
  local rc="$TARGET_HOME/.zshrc"
  local old='alias vpn="python3 /home/isaac/apps/vpns/vpn_manager.py"'
  [[ -f "$rc" ]] || return
  grep -qF "$old" "$rc" || return
  as_user sed -i "s|^${old}$|# reemplazado por vpn-tui/install.sh — # ${old}|" "$rc"
  warn "comenté el alias viejo de 'vpn' (vpn_manager.py) en $rc; abre una terminal nueva"
}

# --- sudoers ------------------------------------------------------------------
# openfortivpn necesita root, y la TUI no puede pedir contraseña (usa `sudo -n`).
# Las reglas de pkill están acotadas a la señal y al nombre exacto del proceso.
# El toggle de IPv6 ahora necesita ambos scripts (deshabilitar y habilitar).
ipv6_script_from_db() {
  local key="$1"
  command -v sqlite3 >/dev/null || return
  [[ -f "$DB_PATH" ]] || return
  sqlite3 "$DB_PATH" "SELECT value FROM settings WHERE key = '$key'" 2>/dev/null
}

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

  # Primero se intenta leer la ruta ya migrada en la base de datos; si no
  # existe todavía (primera instalación, antes de correr `vpn` una vez), se
  # cae a la convención de este proyecto: scripts junto a `../vpns/`.
  local disable_script enable_script
  # `$(...)` en una asignación simple SÍ dispara `set -e` si el comando
  # substituido falla (aunque `ipv6_script_from_db` termine en `return` por
  # DB inexistente, que es el caso normal en toda instalación nueva) — se
  # neutraliza con `|| true` para no tronar el instalador por esto.
  disable_script="$(ipv6_script_from_db ipv6_disable_script || true)"
  enable_script="$(ipv6_script_from_db ipv6_enable_script || true)"
  [[ -n "$disable_script" ]] || disable_script="$(cd "$HERE/.." && pwd)/vpns/ipv6_deshabilitar.sh"
  [[ -n "$enable_script" ]] || enable_script="$(cd "$HERE/.." && pwd)/vpns/habilitar_ipv6.sh"

  local ipv6_rules=""
  if [[ -f "$disable_script" ]]; then
    ipv6_rules+="$TARGET_USER ALL=(ALL) NOPASSWD: $disable_script"$'\n'
  fi
  if [[ -f "$enable_script" ]]; then
    ipv6_rules+="$TARGET_USER ALL=(ALL) NOPASSWD: $enable_script"$'\n'
  fi

  local tmp
  tmp="$(mktemp)"
  cat > "$tmp" <<EOF
# Generado por vpn-tui/install.sh
$TARGET_USER ALL=(ALL) NOPASSWD: $forti
$TARGET_USER ALL=(ALL) NOPASSWD: $pkill_bin -INT -x openfortivpn
$TARGET_USER ALL=(ALL) NOPASSWD: $pkill_bin -TERM -x openfortivpn
$TARGET_USER ALL=(ALL) NOPASSWD: $pkill_bin -KILL -x openfortivpn
${ipv6_rules}
EOF

  visudo -cqf "$tmp" || { rm -f "$tmp"; die "sudoers inválido, no instalé nada"; }
  install -o root -g root -m 440 "$tmp" "$SUDOERS_FILE"
  rm -f "$tmp"
  ok "reglas sudo en $SUDOERS_FILE"
}

main() {
  info "vpn → usuario $TARGET_USER"
  install_system_deps
  build
  install_bin
  disable_old_alias
  install_sudoers
  echo
  ok "listo — ejecuta: vpn"
}

main "$@"
