#!/usr/bin/env bash
# Instalador de vpn (antes vpn-tui): instala las dependencias del sistema
# (openvpn3, openfortivpn, tailscale), compila e instala en el layout estándar
# del sistema, y configura sudoers.
#
#   sudo ./install.sh
#
# Layout (estándar FHS/XDG para un programa instalado a mano):
#   /usr/local/bin/vpn                     binario
#   /usr/local/libexec/vpn/ipv6-*.sh       scripts privilegiados (root:root)
#   /usr/local/libexec/vpn/vpn-launcher.sh abre la TUI en la terminal del usuario
#   /usr/share/applications/vpn.desktop    entrada del lanzador
#   /usr/share/icons/hicolor/512x512/apps/vpn.png   ícono de la app
#   ~/.local/share/vpn-tui/vpn-tui.db      datos por usuario (no se toca)
#
# Soporta Fedora/RHEL-like (dnf) y Debian/Ubuntu-like (apt). El resto del
# sistema (systemd, sudoers) es igual en ambos.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_USER="${SUDO_USER:-$USER}"
TARGET_HOME="$(getent passwd "$TARGET_USER" | cut -d: -f6)"
PREFIX="/usr/local"
BIN_DIR="$PREFIX/bin"
LIBEXEC_DIR="$PREFIX/libexec/vpn"
DESKTOP_DIR="/usr/share/applications"
ICON_THEME_DIR="/usr/share/icons/hicolor"
ICON_DIR="$ICON_THEME_DIR/512x512/apps"
SUDOERS_FILE="/etc/sudoers.d/vpn-tui"
IPV6_DISABLE="$LIBEXEC_DIR/ipv6-disable.sh"
IPV6_ENABLE="$LIBEXEC_DIR/ipv6-enable.sh"
LAUNCHER="$LIBEXEC_DIR/vpn-launcher.sh"

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
  # Cada dependencia es independiente: si una falla (repo caído, systemd
  # ausente en un contenedor, red intermitente, etc.) se avisa y se sigue con
  # las demás y con compilar/instalar el binario — no tiene sentido tronar
  # todo el instalador por un tipo de VPN que el usuario puede no necesitar.
  case "$PKG_MANAGER" in
    dnf) install_openvpn3_fedora || warn "openvpn3 no se pudo instalar; sigo sin él" ;;
    apt) install_openvpn3_debian || warn "openvpn3 no se pudo instalar; sigo sin él" ;;
    *) warn "omito openvpn3: distro no reconocida" ;;
  esac
  install_openfortivpn || warn "openfortivpn no se pudo instalar; sigo sin él"
  install_tailscale || warn "tailscale no se pudo instalar; sigo sin él"
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

# --- instalación ---------------------------------------------------------
# Todo lo que se instala va como root al prefijo del sistema. No hace falta
# tocar el PATH de ningún shell: /usr/local/bin ya está en el PATH por defecto
# en Fedora, Debian, Ubuntu y Kali.
install_bin() {
  install -d -o root -g root -m 755 "$BIN_DIR"
  install -o root -g root -m 755 "$HERE/target/release/vpn" "$BIN_DIR/vpn"
  ok "binario en $BIN_DIR/vpn"
}

# Los scripts de IPv6 quedan root:root en /usr/local/libexec/vpn/ justamente
# porque sudoers les da NOPASSWD: si vivieran en el home del usuario, cualquiera
# con esa cuenta podría reescribirlos y ejecutarse como root sin contraseña.
install_scripts() {
  install -d -o root -g root -m 755 "$LIBEXEC_DIR"
  install -o root -g root -m 755 "$HERE/scripts/ipv6-disable.sh" "$IPV6_DISABLE"
  install -o root -g root -m 755 "$HERE/scripts/ipv6-enable.sh" "$IPV6_ENABLE"
  ok "scripts de IPv6 en $LIBEXEC_DIR (root:root)"
  # Lanzador para el .desktop: elige la terminal del usuario (kitty y demás) en
  # vez de dejarlo en manos de `Terminal=true`, que en sesiones sin
  # xdg-terminal-exec abre cualquier cosa o falla.
  install -o root -g root -m 755 "$HERE/scripts/vpn-launcher.sh" "$LAUNCHER"
  ok "lanzador en $LAUNCHER"
}

install_icon() {
  local src="$HERE/assets/vpn.png"
  [[ -f "$src" ]] || { warn "no encuentro assets/vpn.png; el lanzador usará el ícono por defecto del tema"; return 0; }
  install -d -o root -g root -m 755 "$ICON_DIR"
  install -o root -g root -m 644 "$src" "$ICON_DIR/vpn.png"
  # Refresca el caché de íconos si la herramienta existe (no es crítica).
  command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f -t "$ICON_THEME_DIR" 2>/dev/null || true
  ok "ícono en $ICON_DIR/vpn.png"
}

install_desktop() {
  local src="$HERE/vpn.desktop"
  [[ -f "$src" ]] || { warn "no encuentro vpn.desktop; omito la entrada del lanzador"; return 0; }
  install -d -o root -g root -m 755 "$DESKTOP_DIR"
  install -o root -g root -m 644 "$src" "$DESKTOP_DIR/vpn.desktop"
  # Refresca el caché del lanzador si la herramienta existe (no es crítica).
  command -v update-desktop-database >/dev/null && update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
  ok "entrada del lanzador en $DESKTOP_DIR/vpn.desktop"
}

# --- alias viejo -------------------------------------------------------------
disable_old_alias() {
  local rc="$TARGET_HOME/.zshrc"
  local old='alias vpn="python3 /home/isaac/apps/vpns/vpn_manager.py"'
  [[ -f "$rc" ]] || return 0
  grep -qF "$old" "$rc" || return 0
  as_user sed -i "s|^${old}$|# reemplazado por vpn-tui/install.sh — # ${old}|" "$rc"
  warn "comenté el alias viejo de 'vpn' (vpn_manager.py) en $rc; abre una terminal nueva"
}

# --- sudoers ------------------------------------------------------------------
# openfortivpn necesita root, y la TUI no puede pedir contraseña (usa `sudo -n`).
# Las reglas de pkill están acotadas a la señal y al nombre exacto del proceso.
# Las rutas de los scripts de IPv6 son FIJAS y root:root a propósito: nunca se
# leen de la base de datos ni de ningún lugar escribible por el usuario, porque
# una regla NOPASSWD apuntando a un archivo que el usuario puede editar es una
# escalada de privilegios directa.
install_sudoers() {
  local forti pkill_bin
  forti="$(command -v openfortivpn || true)"
  pkill_bin="$(command -v pkill || true)"
  [[ -n "$forti" ]] || { warn "openfortivpn no encontrado: omito sudoers"; return 0; }
  [[ -n "$pkill_bin" ]] || die "no encuentro pkill"

  local ipv6_rules=""
  if [[ -f "$IPV6_DISABLE" ]]; then
    ipv6_rules+="$TARGET_USER ALL=(ALL) NOPASSWD: $IPV6_DISABLE"$'\n'
  fi
  if [[ -f "$IPV6_ENABLE" ]]; then
    ipv6_rules+="$TARGET_USER ALL=(ALL) NOPASSWD: $IPV6_ENABLE"$'\n'
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
  [[ $EUID -eq 0 ]] || die "este instalador escribe en $PREFIX y /etc/sudoers.d: ejecútalo con 'sudo ./install.sh'"
  info "vpn → usuario $TARGET_USER"
  install_system_deps
  build
  install_bin
  install_scripts
  install_icon
  install_desktop
  disable_old_alias
  install_sudoers
  echo
  ok "listo — ejecuta: vpn"
}

main "$@"
