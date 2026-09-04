# vpn

TUI en Rust para gestionar **OpenVPN3**, **OpenFortiVPN** y **Tailscale** desde
una sola pantalla, con estado persistido en SQLite. Sucesor de `vpn-tui`
(que a su vez reemplazaba al `vpn_manager.py` de `../vpns`).

```
╭ vpn ─────────────────────────────────────────────────╮
│                                                       │
│                    1) OpenVPN3                        │
│                                                       │
│                    2) OpenFortiVPN                     │
│                                                       │
│                    3) Tailscale                        │
│                                                       │
╰───────────────────────────────────────────────────────╯
 c  configuración   q  salir
```

`1`/`2` entran a un dashboard con estado en vivo, tráfico y log por perfil
(igual que `vpn-tui` original); `3` entra al estado de Tailscale. Todo lo que
no es "conectar/desconectar el día a día" — agregar o editar perfiles, ajustes
generales, el toggle de IPv6, ver y matar sesiones OpenVPN3 huérfanas — vive
en el menú de **configuración** (`c`), no en el menú principal.

## Características

- **Perfiles en SQLite**, no en un `config.toml` estático: se agregan, editan
  y borran desde la propia TUI (menú de config → Perfiles), con un formulario
  y un explorador de archivos (`F3`) para elegir el `.ovpn` sin teclear la
  ruta a mano.
- **Migración automática, una sola vez**: si ya tenías un `config.toml` de la
  versión anterior, sus perfiles (incluidas las credenciales de FortiVPN) se
  importan a la base de datos la primera vez que corres `vpn`. El
  `config.toml` no se borra ni se vuelve a tocar después de eso.
- **Tailscale integrado**: estado (`backend`, IPs, exit-node, rutas
  anunciadas), conectar/desconectar, elegir exit-node de una lista de peers
  candidatos, y anunciar subnet routes (recordando que el admin del tailnet
  debe aprobarlas aparte).
- **Matador de sesiones OpenVPN3**: el menú de config muestra *todas* las
  sesiones vivas que reporta `openvpn3 sessions-list` — incluidas las que
  nadie cerró (reconexiones automáticas del core, sesiones de ayer que
  siguen vivas) — y permite cerrar una o todas.
- **Toggle de IPv6 real**: lee `/proc/sys/net/ipv6/conf/all/disable_ipv6` en
  vez de asumir un estado, y alterna entre los dos scripts instalados en
  `/usr/local/libexec/vpn/` (`ipv6-disable.sh` / `ipv6-enable.sh`).
- **Ambas VPNs a la vez o por separado**, sin bloquear la interfaz: cada
  conexión corre en su propio hilo y reporta por un canal.
- **Adopción de sesiones existentes**: si ya había un túnel levantado (por
  esta TUI, por el script de Python o a mano), lo detecta y lo muestra. Esas
  conexiones se marcan como externas y **no** se cierran al salir.
- **Reconciliación continua**: cada `poll_interval_ms` (2 s por omisión)
  consulta `openvpn3 sessions-list`, `pgrep openfortivpn`, `tailscale status
  --json` y `/sys/class/net`, así que el estado que ves es el real.
- **Credenciales fuera de `ps`**: `openfortivpn` recibe un archivo de
  configuración temporal `chmod 600` en `$XDG_RUNTIME_DIR`, que se borra al
  terminar el proceso.
- **Nunca pide contraseña**: todo lo que necesita root va por `sudo -n`
  (openfortivpn, los scripts de IPv6) o directo por D-Bus/CLI sin privilegios
  (openvpn3, tailscale — con reintento por `sudo -n` si el equipo no tiene
  configurado `tailscale set --operator`).

## Instalación

```bash
cd /home/isaac/apps/vpn-tui
sudo ./install.sh
```

Instala en el layout estándar del sistema (por eso necesita `sudo`):

| Qué | Dónde |
| --- | --- |
| binario | `/usr/local/bin/vpn` (root:root 755) |
| scripts de IPv6 | `/usr/local/libexec/vpn/ipv6-{disable,enable}.sh` (root:root 755) |
| entrada del lanzador | `/usr/share/applications/vpn.desktop` |
| reglas sudo | `/etc/sudoers.d/vpn-tui` |
| datos del usuario | `~/.local/share/vpn-tui/vpn-tui.db` (no se toca al reinstalar) |

La compilación corre como tu usuario (no como root); solo la instalación de
archivos es privilegiada. No hace falta tocar el `PATH`: `/usr/local/bin` ya
está en el de cualquier shell. Con la entrada `.desktop` (`Terminal=true`)
`vpn` aparece en el lanzador de aplicaciones y se abre en tu terminal
predeterminada.

El instalador reconoce Fedora/RHEL-like (`dnf`) y Debian/Ubuntu-like (`apt`):

- `openfortivpn`: paquete oficial en ambos.
- `openvpn3`: en Fedora viene del COPR de terceros `dsommers/openvpn3`
  (`dnf copr enable` + `dnf install`); en Debian/Ubuntu, del repo APT oficial
  del proyecto (`swupdate.openvpn.net`) si publica lista para tu codename —
  hoy no cubre bookworm/noble; en Kali se usa el paquete nativo
  `openvpn3-client`. Ver la
  [wiki de OpenVPN3Linux](https://community.openvpn.net/openvpn/wiki/OpenVPN3Linux).
- `tailscale`: instalador oficial (`curl -fsSL https://tailscale.com/install.sh | sh`),
  que ya cubre ambas familias de distro.

Si detecta el alias viejo `alias vpn="python3 .../vpn_manager.py"` en tu
`~/.zshrc`, lo comenta (no lo borra) y avisa.

O a mano:

```bash
cargo build --release
./target/release/vpn
```

Requiere Rust 1.88+, y `openvpn3` / `openfortivpn` / `tailscale` según los
perfiles y funciones que uses.

## Base de datos

SQLite, buscada en:

1. `$VPN_TUI_DB`
2. `~/.local/share/vpn-tui/vpn-tui.db` (por omisión)

Se crea sola (con `chmod 600`) la primera vez que corres `vpn`, migrando
perfiles desde un `config.toml` viejo si lo encuentra (misma búsqueda que
usaba la versión anterior: `$VPN_TUI_CONFIG`, el directorio del binario,
`./config.toml`, `~/.config/vpn-tui/config.toml`). El formato de
`config.toml` sigue documentado en [`config.example.toml`](config.example.toml)
solo como referencia de qué se migra — ya no es la fuente de verdad en
ejecución.

## Atajos

### Menú principal

| Tecla | Acción |
| --- | --- |
| `1` / `2` / `3` | entrar a OpenVPN3 / OpenFortiVPN / Tailscale |
| `c` | menú de configuración |
| `q` / `esc` / `ctrl-c` | salir |

### Dashboard (OpenVPN3 / OpenFortiVPN)

| Tecla | Acción |
| --- | --- |
| `↑` `↓` / `k` `j` | mover selección (o desplazar el log si tiene el foco) |
| `enter` / `space` | conectar o desconectar el perfil seleccionado |
| `c` / `d` | conectar / desconectar el perfil |
| `a` / `x` | conectar / desconectar todos los de este tipo |
| `tab` | alternar foco entre perfiles y registro |
| `pgup` `pgdn`, `g`, `G` | desplazar el registro / ir al inicio / seguir el final |
| `r` | limpiar el registro |
| `?` | ayuda |
| `q` / `esc` | volver al menú principal (cierra lo que abrió esta TUI) |

### Tailscale

| Tecla | Acción |
| --- | --- |
| `enter` / `space` | conectar / desconectar |
| `e` | elegir exit-node (lista de peers candidatos) |
| `r` | editar rutas anunciadas |
| `esc` / `q` | volver al menú principal |

### Config (`c` desde el menú principal)

| Tecla | Acción |
| --- | --- |
| `↑` `↓` | moverse en el menú o en una lista |
| `enter` | entrar / confirmar |
| `n` / `e` / `x` | nuevo / editar / borrar perfil (en una lista de perfiles) |
| `F3` | buscar archivo (campos de ruta, en el formulario o en General) |
| `tab` / `shift-tab` | siguiente / anterior campo |
| `t` | alternar IPv6 (en la pantalla de estado de IPv6) |
| `d` / `a` / `esc`/`n` | matar sesión seleccionada / matar todas / cancelar (sesiones OpenVPN3) |
| `esc` / `q` | atrás |

## Modos no interactivos

```bash
vpn --status      # estado de cada perfil, interfaces y Tailscale, una línea por entrada
vpn --help
vpn --version
```

## Permisos sudo

`openfortivpn` necesita root y la TUI usa `sudo -n`, así que las reglas tienen
que estar en sudoers o la conexión falla al instante. `install.sh` escribe
`/etc/sudoers.d/vpn-tui` con lo mínimo:

```
isaac ALL=(ALL) NOPASSWD: /usr/bin/openfortivpn
isaac ALL=(ALL) NOPASSWD: /usr/bin/pkill -INT -x openfortivpn
isaac ALL=(ALL) NOPASSWD: /usr/bin/pkill -TERM -x openfortivpn
isaac ALL=(ALL) NOPASSWD: /usr/bin/pkill -KILL -x openfortivpn
isaac ALL=(ALL) NOPASSWD: /usr/local/libexec/vpn/ipv6-disable.sh
isaac ALL=(ALL) NOPASSWD: /usr/local/libexec/vpn/ipv6-enable.sh
```

Los scripts de IPv6 se instalan en `/usr/local/libexec/vpn/` y son `root:root`
justamente porque llevan `NOPASSWD`: una regla así apuntando a un archivo
dentro del home (editable por el usuario) sería una escalada de privilegios
trivial. Por lo mismo, las rutas de sudoers son fijas y nunca se leen de la
base de datos.

Las reglas de `pkill` están acotadas a la señal y al nombre exacto del proceso,
así que no sirven para tocar nada más. La contrapartida es que afectan a *todos*
los `openfortivpn` del sistema: si algún día corres varios túneles Forti a la
vez, añade una regla de `kill` por PID — la TUI ya lo intenta como segunda
opción.

`openvpn3` no necesita sudo: habla por D-Bus con el servicio del sistema.
`tailscale` tampoco, salvo que este equipo no tenga configurado
`tailscale set --operator=$USER`, en cuyo caso la TUI reintenta con `sudo -n`.

## Estructura

```
src/
├── main.rs          CLI, arranque del terminal, enrutamiento de pantallas y teclas
├── app.rs            estado, transiciones, formularios, reconciliación con el sistema
├── db.rs             persistencia SQLite (perfiles, ajustes) + migración desde config.toml
├── event.rs          canal de mensajes: teclado, latido y sondeo (incluye Tailscale)
├── ui.rs             dibujado (ratatui): menú, dashboard, config, tailscale
└── vpn/
    ├── mod.rs        ejecución de comandos con timeout, interfaces, formatos
    ├── openvpn3.rs   session-start / -manage / sessions-list / session-stats / matador de sesiones
    ├── fortivpn.rs   spawn con credenciales en archivo temporal, señales, pgrep
    └── tailscale.rs  status --json + debug prefs, up/down, set exit-node/rutas
```

`cargo test` cubre el parseo de `openvpn3` (incluidas dos sesiones
simultáneas), la clasificación de líneas de `openfortivpn`, el parseo de
`tailscale status --json` + `debug prefs`, y la persistencia/migración en
`db.rs` (perfiles, CRUD, detección del script hermano de habilitar IPv6).

## Diferencias con la versión anterior (`vpn-tui` / `vpn_manager.py`)

| | `vpn_manager.py` | `vpn-tui` (v1) | `vpn` (esta versión) |
| --- | --- | --- | --- |
| Interfaz | menú, una acción a la vez | pantalla completa | menú 1/2/3 + config aparte |
| Perfiles | uno de cada tipo, en el código | en `config.toml` | en SQLite, editables desde la TUI |
| Tailscale | no | no | estado, up/down, exit-node, rutas |
| Sesiones OpenVPN3 huérfanas | invisibles | invisibles | listadas y se pueden matar |
| IPv6 | solo deshabilitar | solo deshabilitar | alterna en ambas direcciones |
| Comando | `vpn_manager.py` / alias `vpn` | `vpn-tui` | `vpn` (`vpn 1`/`2`/`3` saltan el menú) |
