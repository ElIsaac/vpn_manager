#!/usr/bin/env bash
# Instalador de vpn (antes vpn-tui): instala TODO lo necesario partiendo de un
# sistema recién instalado — herramientas de compilación (compilador de C,
# curl, gpg), la toolchain de Rust (rustup) si falta o está vieja, las
# dependencias de VPN (openvpn3, openfortivpn, tailscale) — compila e instala
# en el layout estándar del sistema, y configura sudoers.
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

# apt en una máquina limpia abre diálogos (configuración de paquetes, reinicio
# de servicios) que cuelgan un instalador que corre sin supervisión.
export DEBIAN_FRONTEND=noninteractive

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

# Versión mínima de Rust que compila este proyecto. No es un número inventado:
# es el MSRV real de las dependencias de Cargo.lock (ratatui 0.30 pide 1.88 y
# enum-ordinalize 4.4 pide 1.89). El cargo de los repos de Ubuntu se queda
# corto en las versiones vigentes (24.04 → 1.75, 25.10 → 1.85), así que cuando
# no alcanza se instala rustup en vez de fallar.
MIN_RUST="1.89.0"

# Se rellenan solos al detectar la toolchain (ver ensure_cargo):
#   CARGO_TOOLCHAIN  "+stable" si hay que forzar una toolchain concreta de rustup
#   CARGO_VIA_LOGIN  1 si el cargo del usuario solo aparece en su perfil de login
CARGO_TOOLCHAIN=""
CARGO_VIA_LOGIN=0

info()  { printf '\033[36m→\033[0m %s\n' "$*"; }
ok()    { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[33m!\033[0m %s\n' "$*"; }
die()   { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

as_user() {
  # sudo -u arranca un shell no interactivo/no login: no lee .zshenv/.profile,
  # así que ~/.cargo/bin (rustup) no queda en PATH aunque el usuario sí lo
  # tenga en su sesión normal. Se agrega explícito para no depender de eso.
  # HOME también va explícito: rustup y cargo escriben en ~/.cargo y ~/.rustup,
  # y no queremos que acaben en /root si sudo no reescribe HOME.
  if [[ $EUID -eq 0 && "$TARGET_USER" != root ]]; then
    sudo -u "$TARGET_USER" env "HOME=$TARGET_HOME" "PATH=$TARGET_HOME/.cargo/bin:$PATH" "$@"
  else
    HOME="$TARGET_HOME" PATH="$TARGET_HOME/.cargo/bin:$PATH" "$@"
  fi
}

# apt-get siempre con las mismas garantías: sin diálogos (DEBIAN_FRONTEND, ya
# exportado) y sin que dpkg tome el control del pty. Dpkg::Use-Pty=0 y el
# stdin cerrado importan cuando el instalador se lanza desde una terminal sin
# stdin (ssh no interactivo, un contenedor con TTY, un lanzador gráfico): ahí
# cualquier intento de leer del terminal le manda SIGTTIN al proceso y apt se
# queda detenido para siempre en vez de fallar. Visto en pruebas con Docker.
# Los timeouts evitan el otro modo de fallo de un mirror roto: no el 404, sino
# la conexión que nunca contesta y deja el instalador esperando sin límite
# (visto en pruebas: 50 minutos en un `apt-get update`).
apt_get() {
  apt-get -o Dpkg::Use-Pty=0 \
          -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 \
          -o Acquire::Retries=2 "$@" </dev/null
}

# curl para comprobaciones y descargas chicas: nunca debe quedarse esperando.
curl_q() { curl --connect-timeout 10 --max-time 60 "$@"; }

# `command`, `cd` y compañía son builtins del shell, no ejecutables: pasarlos
# por `env` (como hace as_user cuando corre con sudo) falla con
# "env: 'command': No such file or directory". Todo lo que sea builtin o
# necesite tuberías va por aquí.
as_user_sh() { as_user bash -c "$1"; }

# --- distro --------------------------------------------------------------
PKG_MANAGER=""
DISTRO_CODENAME=""
if [[ -r /etc/os-release ]]; then
  # shellcheck source=/dev/null
  . /etc/os-release
  # UBUNTU_CODENAME primero: en las derivadas de Ubuntu (Mint, Pop!_OS, Zorin,
  # elementary) VERSION_CODENAME es el nombre de LA DERIVADA ('xia', 'wilma'…),
  # que ningún repo de terceros publica, mientras UBUNTU_CODENAME dice la base
  # real ('noble'). Sin esto se omitía openvpn3 en todas esas distros.
  DISTRO_CODENAME="${UBUNTU_CODENAME:-${VERSION_CODENAME:-}}"
  # Debian viejo (wheezy, jessie) no trae VERSION_CODENAME: el codename solo
  # aparece entre paréntesis en VERSION/PRETTY_NAME.
  if [[ -z "$DISTRO_CODENAME" ]]; then
    _cn="${VERSION:-${PRETTY_NAME:-}}"
    _cn="${_cn##*\(}"; _cn="${_cn%%\)*}"
    # `if` y no `[[ … ]] && …`: con `set -e`, un test que no hace match como
    # último comando del bloque aborta el instalador.
    if [[ "$_cn" =~ ^[a-z]+$ ]]; then DISTRO_CODENAME="$_cn"; fi
    unset _cn
  fi
  case "${ID:-}:${ID_LIKE:-}" in
    fedora:*|*:*fedora*|rhel:*|*:*rhel*) PKG_MANAGER="dnf" ;;
    debian:*|ubuntu:*|*:*debian*) PKG_MANAGER="apt" ;;
  esac
fi
[[ -n "$PKG_MANAGER" ]] || warn "no reconozco el gestor de paquetes de esta distro; omito instalación de dependencias del sistema"

# --- herramientas base ------------------------------------------------------
# Una Ubuntu/Fedora recién instalada (y cualquier imagen mínima) no trae ni
# curl, ni gpg, ni compilador de C. Los tres son obligatorios aquí:
#   * curl/gpg  → los usan los repos de openvpn3 y el instalador de tailscale.
#   * cc + make → rusqlite se compila con SQLite embebido (feature `bundled`),
#                 así que cargo necesita un compilador de C, y rustc necesita
#                 `cc` como linker. Sin esto la compilación truena aunque
#                 cargo esté instalado.
#   * pkill/ip  → los ejecuta la TUI en caliente (matar openfortivpn, leer
#                 interfaces), y sudoers necesita la ruta real de pkill.
install_prereqs() {
  local -a want=() pkgs=()
  command -v curl       >/dev/null || want+=(curl)
  command -v gpg        >/dev/null || want+=(gpg)
  command -v cc         >/dev/null || want+=(cc)
  command -v make       >/dev/null || want+=(make)
  command -v pkg-config >/dev/null || want+=(pkg-config)
  command -v pkill      >/dev/null || want+=(pkill)
  command -v ip         >/dev/null || want+=(ip)
  # distro-info-data: el mapa versión→codename que usa older_series para las
  # releases intermedias. Viene de serie en instalaciones normales (prioridad
  # 'important'), pero no en imágenes mínimas.
  if [[ "$PKG_MANAGER" == apt && ! -r /usr/share/distro-info/ubuntu.csv ]]; then
    want+=(distro-info)
  fi

  if ((${#want[@]} == 0)); then
    ok "herramientas base ya presentes (curl, gpg, compilador de C, pkill, ip)"
    return 0
  fi

  local tool
  for tool in "${want[@]}"; do
    case "$PKG_MANAGER:$tool" in
      apt:curl)       pkgs+=(curl ca-certificates) ;;
      apt:gpg)        pkgs+=(gnupg) ;;
      apt:cc)         pkgs+=(build-essential) ;;
      apt:make)       pkgs+=(make) ;;
      apt:pkg-config) pkgs+=(pkg-config) ;;
      apt:pkill)      pkgs+=(procps) ;;
      apt:ip)         pkgs+=(iproute2) ;;
      apt:distro-info) pkgs+=(distro-info-data) ;;
      dnf:curl)       pkgs+=(curl ca-certificates) ;;
      dnf:gpg)        pkgs+=(gnupg2) ;;
      dnf:cc)         pkgs+=(gcc glibc-devel) ;;
      dnf:make)       pkgs+=(make) ;;
      dnf:pkg-config) pkgs+=(pkgconf-pkg-config) ;;
      dnf:pkill)      pkgs+=(procps-ng) ;;
      dnf:ip)         pkgs+=(iproute) ;;
      *) warn "falta '$tool' y no sé instalarlo en esta distro; instálalo a mano" ;;
    esac
  done
  ((${#pkgs[@]})) || return 0

  info "instalando herramientas base: ${pkgs[*]}…"
  case "$PKG_MANAGER" in
    apt) apt_get update -qq && apt_get install -y -qq "${pkgs[@]}" ;;
    dnf) dnf install -y "${pkgs[@]}" ;;
  esac || warn "falló la instalación de alguna herramienta base; sigo y verifico"

  # Sin compilador de C no hay nada que hacer: es mejor decirlo aquí que
  # después de tres minutos de descarga de crates.
  command -v cc >/dev/null || command -v gcc >/dev/null \
    || die "no pude instalar un compilador de C (cc/gcc) y rusqlite lo necesita para compilar SQLite"
  command -v curl >/dev/null || die "no pude instalar curl, lo necesitan los repos de openvpn3/tailscale"
  ok "herramientas base listas"
}

# --- openvpn3 / openfortivpn ----------------------------------------------
# openfortivpn es paquete oficial en Fedora y Debian/Ubuntu. openvpn3 no: en
# Fedora hay que ir al COPR de terceros `dsommers/openvpn3`, y en Debian/Ubuntu
# depende de la versión (Ubuntu lo empaqueta desde 25.10; antes de eso, repo
# APT del proyecto). Por eso cada distro tiene su función.
# Ubuntu saca una release cada 6 meses, pero OpenVPN solo empaqueta algunas
# (comprobado: noble, plucky y resolute sí; oracular y questing no). Para una
# release intermedia sirve el paquete de la release ANTERIOR más cercana que el
# repo sí publique: probado en 24.10 (oracular), donde el .deb de noble instala
# sin dependencias rotas y `openvpn3 version` responde v27.1. Solo hacia atrás:
# un paquete de una release POSTERIOR pediría librerías que esta máquina no
# tiene todavía.
#
# El mapa versión→codename se lee de distro-info-data (paquete de prioridad
# 'important' en Debian y Ubuntu) para no dejar una tabla de codenames dentro
# de este script, que quedaría vieja en la siguiente release.
older_series() {
  local csv="/usr/share/distro-info/${ID:-ubuntu}.csv"
  [[ -r "$csv" ]] || csv="/usr/share/distro-info/ubuntu.csv"
  [[ -r "$csv" ]] || return 0
  # columnas: version,codename,series,… — 'series' es el codename corto.
  awk -F, -v v="${VERSION_ID:-0}" \
    'NR>1 && ($1+0) < (v+0) && $3 != "" { printf "%s %s\n", ($1+0), $3 }' "$csv" \
    | sort -k1,1 -rn | awk '{print $2}' | head -3
}

# Primer codename que un repo publique: el de esta máquina si está, y si no el
# de la release anterior más cercana. @CN@ es el hueco del codename en la URL.
repo_codename() {
  local url_fmt="$1" cn code
  for cn in "$DISTRO_CODENAME" $(older_series); do
    [[ -n "$cn" ]] || continue
    code="$(curl_q -fsS -o /dev/null -w '%{http_code}' "${url_fmt//@CN@/$cn}" 2>/dev/null || true)"
    if [[ "$code" == "200" ]]; then printf '%s' "$cn"; return 0; fi
  done
  return 1
}

install_openvpn3_fedora() {
  command -v openvpn3 >/dev/null && { ok "openvpn3 ya está instalado"; return 0; }

  # `dnf copr` es un plugin, no un subcomando de serie: vive en dnf-plugins-core
  # (dnf4) o dnf5-plugins (dnf5) y en una instalación mínima no está, así que sin
  # esto el COPR fallaba con "No such command: copr". El nombre virtual
  # 'dnf-command(copr)' lo resuelve en ambas generaciones de dnf.
  if ! dnf copr --help >/dev/null 2>&1; then
    info "instalando el plugin copr de dnf…"
    dnf install -y 'dnf-command(copr)' >/dev/null 2>&1 \
      || dnf install -y dnf-plugins-core >/dev/null 2>&1 \
      || warn "no pude instalar el plugin copr de dnf"
  fi

  info "habilitando COPR dsommers/openvpn3…"
  # El COPR solo publica para las versiones de Fedora que su dueño mantiene
  # (comprobado: hay chroot para 43 y 44, no para 42 y anteriores), y ahí
  # responde "Chroot not found in the given Copr project".
  if ! dnf -y copr enable dsommers/openvpn3; then
    warn "el COPR dsommers/openvpn3 no publica paquetes para esta versión de Fedora"
    return 1
  fi
  dnf install -y openvpn3 || true
  # Se verifica el binario: igual que en la rama de Debian, estas funciones se
  # llaman con `|| warn` y sin esto un dnf fallido llegaba igual al `ok`.
  command -v openvpn3 >/dev/null || { warn "el COPR quedó habilitado pero no pude instalar openvpn3"; return 1; }
  ok "openvpn3 instalado (COPR dsommers/openvpn3)"
}

install_openvpn3_debian() {
  command -v openvpn3 >/dev/null && { ok "openvpn3 ya está instalado"; return 0; }

  # 1) Paquete nativo de la distro — siempre preferible al repo del vendor.
  #    Ubuntu lo empaqueta en universe desde 25.10 (openvpn3-client 25) y
  #    26.04 trae la 27; también está en Kali `main` y en Debian sid/testing.
  apt_get update -qq || true
  if apt_get install -y -qq openvpn3-client >/dev/null 2>&1 && command -v openvpn3 >/dev/null; then
    ok "openvpn3 instalado (paquete nativo de la distro: openvpn3-client)"
    return 0
  fi

  if [[ -z "$DISTRO_CODENAME" ]]; then
    warn "no pude determinar VERSION_CODENAME de /etc/os-release; omito openvpn3"
    return 1
  fi

  # 2) Repo APT oficial vigente del proyecto (packages.openvpn.net). Reemplazó
  #    al viejo swupdate.openvpn.net, que se quedó sin listas para los
  #    codenames nuevos. Comprobado con pruebas reales: publica noble, plucky y
  #    resolute; para lo que no publica (oracular, questing) responde 301 a una
  #    página de error, así que la comprobación NO sigue redirecciones. Cuando
  #    el codename de la máquina no está, repo_codename cae a la release
  #    anterior más cercana que sí esté (ver su comentario).
  local base="https://packages.openvpn.net/openvpn3/debian"
  local keyring="/etc/apt/keyrings/openvpn.asc"
  local cn
  if cn="$(repo_codename "$base/dists/@CN@/Release")"; then
    if [[ "$cn" != "$DISTRO_CODENAME" ]]; then
      warn "el repo de OpenVPN3 no publica para '$DISTRO_CODENAME'; uso el paquete de '$cn' (release anterior compatible)"
    fi
    info "agregando el repo APT oficial de OpenVPN3 ($cn)…"
    install -d -o root -g root -m 755 /etc/apt/keyrings
    # La llave viene en ASCII (armored), así que va tal cual como .asc y no
    # hace falta pasarla por `gpg --dearmor`.
    if curl_q -fsSL "https://packages.openvpn.net/packages-repo.gpg" -o "$keyring"; then
      chmod 644 "$keyring"
      printf 'deb [signed-by=%s] %s %s main\n' "$keyring" "$base" "$cn" \
        > /etc/apt/sources.list.d/openvpn3.list
      apt_get update -qq || true
      if apt_get install -y -qq openvpn3 >/dev/null && command -v openvpn3 >/dev/null; then
        ok "openvpn3 instalado (repo oficial packages.openvpn.net, $cn)"
        return 0
      fi
      warn "el repo packages.openvpn.net se agregó pero la instalación falló"
    else
      warn "no pude descargar la llave del repo de OpenVPN3"
    fi
  fi

  # 3) Repo legacy (swupdate.openvpn.net): hoy solo publica lista para los
  #    codenames viejos (jammy, focal, bullseye…), pero si la máquina es una de
  #    esas, sirve.
  local list_url
  if cn="$(repo_codename "https://swupdate.openvpn.net/community/openvpn3/repos/openvpn3-@CN@.list")"; then
    list_url="https://swupdate.openvpn.net/community/openvpn3/repos/openvpn3-${cn}.list"
    if [[ "$cn" != "$DISTRO_CODENAME" ]]; then
      warn "el repo legacy no publica para '$DISTRO_CODENAME'; uso la lista de '$cn'"
    fi
    info "agregando el repo APT legacy de OpenVPN3 Linux ($cn)…"
    if curl_q -fsSL https://swupdate.openvpn.net/repos/openvpn-repo-pkg-key.pub \
         | gpg --dearmor -o /etc/apt/trusted.gpg.d/openvpn-repo-pkg-keyring.gpg \
       && curl_q -fsSL -o /etc/apt/sources.list.d/openvpn3.list "$list_url"; then
      apt_get update -qq || true
      if apt_get install -y -qq openvpn3 >/dev/null && command -v openvpn3 >/dev/null; then
        ok "openvpn3 instalado (repo legacy swupdate.openvpn.net, $cn)"
        return 0
      fi
    fi
    warn "el repo legacy de openvpn3 se agregó pero la instalación falló"
  fi

  warn "no hay paquete de openvpn3 para '$DISTRO_CODENAME' en la distro ni en los repos oficiales de OpenVPN"
  warn "omito openvpn3 — el resto de la instalación sigue; instálalo a mano desde https://openvpn.net/cloud-docs/owner/connectors/connector-user-guides/openvpn-3-client-for-linux.html"
  return 1
}

install_openfortivpn() {
  command -v openfortivpn >/dev/null && { ok "openfortivpn ya está instalado"; return 0; }
  info "instalando openfortivpn…"
  case "$PKG_MANAGER" in
    dnf) dnf install -y openfortivpn || true ;;
    apt) apt_get update -qq; apt_get install -y -qq openfortivpn >/dev/null || true ;;
  esac
  # Se verifica el binario en vez de confiar en el código de salida: estas
  # funciones se llaman con `|| warn`, lo que apaga `set -e` dentro de ellas y
  # haría que un apt fallido igual llegara al `ok`.
  command -v openfortivpn >/dev/null || { warn "no pude instalar openfortivpn"; return 1; }
  ok "openfortivpn instalado"
}

# --- tailscale -------------------------------------------------------------
# El instalador oficial de Tailscale (https://tailscale.com/docs/install/linux)
# ya detecta la distro y cubre tanto Fedora/dnf como Debian/Ubuntu/apt, así
# que no hace falta duplicar la lógica de repos por distro aquí.
install_tailscale() {
  command -v tailscale >/dev/null && { ok "tailscale ya está instalado"; return 0; }
  command -v curl >/dev/null || { warn "falta curl: omito tailscale"; return 1; }
  info "instalando tailscale (instalador oficial tailscale.com/install.sh)…"
  # `timeout` porque este script es de terceros y hace su propio apt-get: si su
  # repo no contesta (típico en distros viejas), sin límite se cuelga para
  # siempre y el usuario se queda mirando una instalación que nunca avanza.
  curl_q -fsSL https://tailscale.com/install.sh | timeout -k 30 600 sh || true
  command -v tailscale >/dev/null || { warn "el instalador de tailscale terminó pero no encuentro el binario"; return 1; }
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
# La toolchain de Rust se instala sola: este instalador no debe pedirle al
# usuario que vaya a rustup.rs antes de poder usarlo. Se prefiere el cargo que
# ya tenga la máquina si cumple el MSRV; si no, se instala/actualiza rustup
# para $TARGET_USER (nunca para root: el binario se compila como el usuario).

# Devuelve la versión de cargo del usuario, o nada si no hay cargo. El `return
# 0` no es adorno: con `set -e`, una asignación cuyo comando falla (cargo
# ausente → 127) mata el instalador sin decir nada.
cargo_version() {
  local out
  out="$(as_user_sh "cargo $CARGO_TOOLCHAIN --version 2>/dev/null" 2>/dev/null)" || out=""
  if [[ -z "$out" ]]; then
    # Segundo intento con un shell de LOGIN, que sí lee el perfil del usuario:
    # así se reutiliza un cargo instalado fuera de ~/.cargo/bin (nix, asdf,
    # mise, CARGO_HOME movido) en vez de instalarle rustup al lado. El primer
    # intento va con el PATH explícito porque es el caso normal (rustup) y no
    # depende de que el perfil del usuario esté sano.
    out="$(as_user bash -lc "cargo $CARGO_TOOLCHAIN --version 2>/dev/null" 2>/dev/null | grep -m1 '^cargo ')" || out=""
  fi
  [[ -n "$out" ]] || return 0
  printf '%s\n' "$out" | awk '{print $2}'
}

# Compara versiones con `sort -V` (orden de versión), no como texto: "1.100"
# es mayor que "1.89" aunque alfabéticamente no lo parezca.
rust_version_ok() {
  local v="${1:-}"
  [[ -n "$v" ]] || return 1
  [[ "$(printf '%s\n%s\n' "$MIN_RUST" "$v" | sort -V | head -1)" == "$MIN_RUST" ]]
}

install_rustup() {
  command -v curl >/dev/null || die "falta curl, no puedo instalar rustup"
  info "instalando Rust (rustup, toolchain stable) para $TARGET_USER — puede tardar un par de minutos…"
  # --profile minimal: solo rustc/cargo/std, sin docs ni clippy (no hacen falta
  # para compilar esto y bajan bastante el tiempo de instalación).
  # RUSTUP_INIT_SKIP_PATH_CHECK: si la máquina ya tiene un rustc de la distro,
  # rustup-init imprime un 'error: cannot install while Rust is installed' que
  # con -y es solo ignorable — pero un 'error:' en pantalla parece que el
  # instalador tronó. Se salta el chequeo: rustup convive con el rustc del
  # sistema, y como ~/.cargo/bin va primero en el PATH, se compila con el par
  # cargo+rustc de rustup (nunca mezclando uno de cada lado).
  as_user_sh 'curl --proto "=https" --tlsv1.2 -sSf --connect-timeout 10 --max-time 120 https://sh.rustup.rs | RUSTUP_INIT_SKIP_PATH_CHECK=yes timeout -k 30 900 sh -s -- -y --profile minimal --default-toolchain stable' \
    || die "no pude instalar rustup; instálalo a mano: https://rustup.rs"
}

ensure_cargo() {
  local v
  # Esta detección va aquí y no en cargo_version porque a esa se la llama dentro
  # de $( ) —una subshell—, y una variable que se fije ahí no sobrevive.
  if ! as_user_sh 'command -v cargo >/dev/null 2>&1' \
     && as_user bash -lc 'command -v cargo >/dev/null 2>&1'; then
    CARGO_VIA_LOGIN=1
  fi
  v="$(cargo_version)"
  if rust_version_ok "$v"; then
    ok "cargo $v (>= $MIN_RUST) para $TARGET_USER"
    return 0
  fi

  if [[ -z "$v" ]]; then
    warn "cargo no está instalado para $TARGET_USER: lo instalo con rustup"
  else
    warn "cargo $v es más viejo que $MIN_RUST (lo exigen ratatui 0.30 y sus dependencias)"
  fi

  # Si ya hay rustup, instalar su toolchain stable es más rápido y menos
  # invasivo que reinstalar rustup entero. NO se toca `rustup default`: la
  # toolchain por omisión del usuario afecta a todos sus proyectos, y un
  # instalador de una app no tiene por qué cambiársela. Para compilar esto
  # basta con `cargo +stable`.
  if as_user_sh 'command -v rustup >/dev/null 2>&1'; then
    info "instalando la toolchain stable con el rustup que ya tienes…"
    if as_user_sh 'rustup toolchain install stable --profile minimal'; then
      CARGO_TOOLCHAIN="+stable"
      v="$(cargo_version)"
    else
      warn "no pude instalar la toolchain con rustup; reinstalo rustup"
    fi
  fi

  if ! rust_version_ok "$v"; then
    install_rustup
    CARGO_TOOLCHAIN=""   # rustup recién instalado: su stable ya es el default
    v="$(cargo_version)"
  fi

  # Causa típica cuando rustup "instala" pero no deja un cargo usable: la libc
  # de la distro es más vieja que lo que Rust soporta (glibc 2.17, es decir
  # Debian 8 / RHEL 7 en adelante). Se dice en el mensaje para no dejar al
  # usuario adivinando.
  rust_version_ok "$v" \
    || die "instalé Rust pero cargo ('${v:-no encontrado}') sigue por debajo de $MIN_RUST. Si esta distro es muy antigua (glibc $(ldd --version 2>/dev/null | sed -n '1s/.*[^0-9]\([0-9]\+\.[0-9]\+\)$/\1/p')), Rust ya no publica toolchain para ella; revisa https://rustup.rs"
  ok "cargo $v listo para $TARGET_USER${CARGO_TOOLCHAIN:+ (compilando con $CARGO_TOOLCHAIN)}"
}

build() {
  ensure_cargo
  info "compilando en modo release (la primera vez descarga y compila ~200 crates)…"
  # `cd` en vez de `env -C`: esa opción de env llegó en coreutils 8.28 (2017) y
  # en distros más viejas (Debian 8, Ubuntu 16.04, CentOS 7) falla con un
  # 'invalid option' y salida 125, sin pista de qué pasó.
  #
  # -lc solo si el cargo del usuario vive fuera de ~/.cargo/bin: ahí hace falta
  # su perfil para encontrarlo. En el caso normal se evita, para no depender de
  # lo que el perfil del usuario haga con el PATH.
  if [[ "$CARGO_VIA_LOGIN" == 1 ]]; then
    as_user bash -lc "cd \"\$1\" && cargo $CARGO_TOOLCHAIN build --release" _ "$HERE"
  else
    as_user bash -c "cd \"\$1\" && cargo $CARGO_TOOLCHAIN build --release" _ "$HERE"
  fi
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
# Este proyecto nació reemplazando un script `vpn_manager.py` invocado por un
# alias `vpn`. Si ese alias sigue vivo, le gana al binario recién instalado y el
# usuario creería que la instalación no sirvió, así que se comenta (no se borra).
disable_old_alias() {
  local rc
  for rc in "$TARGET_HOME/.zshrc" "$TARGET_HOME/.bashrc"; do
    [[ -f "$rc" ]] || continue
    grep -qE '^[[:space:]]*alias[[:space:]]+vpn=.*vpn_manager\.py' "$rc" || continue
    as_user sed -i -E 's|^([[:space:]]*alias[[:space:]]+vpn=.*vpn_manager\.py.*)$|# reemplazado por el instalador de vpn — \1|' "$rc"
    warn "comenté el alias viejo de 'vpn' (vpn_manager.py) en $rc; abre una terminal nueva"
  done
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

# --- tailscale sin sudo -------------------------------------------------------
# `tailscale up/down/set` escriben las prefs de tailscaled y por defecto exigen
# root ("Access denied: prefs write access denied"). El modo operator delega ese
# permiso a un usuario concreto, que es justo lo que necesita la TUI para no
# pedir contraseña. Como el instalador ya corre como root, se configura aquí en
# vez de dejarle el comando suelto al usuario.
configure_tailscale_operator() {
  command -v tailscale >/dev/null || return 0
  local current
  current="$(tailscale debug prefs 2>/dev/null | sed -n 's/.*"OperatorUser": *"\([^"]*\)".*/\1/p' | head -1 || true)"
  if [[ "$current" == "$TARGET_USER" ]]; then
    ok "tailscale ya opera sin sudo para $TARGET_USER"
    return 0
  fi
  if tailscale set --operator="$TARGET_USER" 2>/dev/null; then
    ok "tailscale configurado para operar sin sudo como $TARGET_USER"
  else
    warn "no pude configurar el modo operator de tailscale (¿tailscaled detenido?)"
    warn "hazlo después con: sudo tailscale set --operator=$TARGET_USER"
  fi
}

main() {
  [[ $EUID -eq 0 ]] || die "este instalador escribe en $PREFIX y /etc/sudoers.d: ejecútalo con 'sudo ./install.sh'"
  info "vpn → usuario $TARGET_USER"
  install_prereqs
  install_system_deps
  build
  install_bin
  install_scripts
  install_icon
  install_desktop
  disable_old_alias
  install_sudoers
  configure_tailscale_operator
  echo
  ok "listo — ejecuta: vpn"
}

main "$@"
