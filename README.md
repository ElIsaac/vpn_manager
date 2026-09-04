# vpn

Un solo gestor en la terminal para tus tres tipos de VPN: **OpenVPN3**,
**OpenFortiVPN** y **Tailscale**. Conecta, desconecta y ve el estado real, el
tráfico y el log de cada túnel sin salir de la terminal ni recordar tres
sintaxis distintas.

```
╭──────────────────────────────────────────────────────────────────────────────────────────────────╮
│ vpn   0/4 activas   IPv6 off                                                                     │
╰──────────────────────────────────────────────────────────────────────────────────────── 16:02:02 ╯
╭ VPNS ─────────────────────────╮╭ detalle ────────────────────────────╮╭ tráfico ─────────────────╮
│ ○ OpenVPN3 (trabajo)          ││ tipo  OpenVPN3                      ││↓ rx 0 B/s                │
│       desconectado            ││ destino                             ││                          │
│ ○ OpenVPN3 (casa)             ││ /home/user/vpns/trabajo.ovpn        ││                          │
│       desconectado            ││ estado  desconectado                ││                          │
│ ○ OpenFortiVPN                ││                                     ││                          │
│       desconectado            ││                                     ││↑ tx 0 B/s                │
│ ○ Tailscale                   ││                                     ││                          │
│       desconectado            ││                                     ││                          │
│                               ││                                     ││                          │
│                               │╰─────────────────────────────────────╯╰──────────────────────────╯
│                               │╭ registro  1 ────────────────────────────────────────────────────╮
│                               ││16:01:59 base de datos: ~/.local/share/vpn-tui/vpn-tui.db        │
│                               ││                                                                 │
╰───────────────────────────────╯│                                                                 │
╭───────────────────────────────╮│                                                                 │
│       ⚙  configuración        ││                                                                 │
╰───────────────────────────────╯╰─────────────────────────────────────────────────────────────────╯
 ↑↓  mover   ←→  panel   1/2/3  saltar a tipo   enter  conectar/cortar   a  todas   x  cortar todas
```

## Por qué

Cada cliente hace lo mismo a su manera: `openvpn3` habla por D-Bus y sobrevive
a que cierres la terminal, `openfortivpn` es un proceso que necesita root y se
mata con señales, y `tailscale` tiene su propio demonio y su propio modelo de
permisos. Esta TUI los pone bajo una sola interfaz y, sobre todo, **nunca te
pide contraseña a media conexión**: los permisos mínimos se conceden una vez al
instalar.

## Qué hace

- **Los tres tipos en una sola lista**, con el estado real y no el que la app
  crea recordar: se reconcilia cada 2 s contra `openvpn3 sessions-list`,
  `pgrep openfortivpn`, `tailscale status` y `/sys/class/net`.
- **Adopta sesiones que ya estaban abiertas** —levantadas a mano, por otra
  herramienta o por una ejecución anterior— y no las cierra al salir.
- **Log por VPN**: al moverte entre perfiles ves solo el registro de esa
  conexión, no todo revuelto.
- **Tráfico en vivo**: totales y velocidad instantánea por perfil.
- **Matador de sesiones OpenVPN3**: lista *todas* las sesiones vivas del sistema
  (incluidas las huérfanas que se quedaron reconectando solas) y las corta una
  por una o todas de golpe.
- **Alta de perfiles guiada**: arrastra el `.ovpn` a la terminal, ábrelo con el
  explorador gráfico del sistema, o navégalo dentro de la TUI.
- **IPv6**: interruptor para deshabilitarlo o habilitarlo, útil con VPNs que
  dejan escapar tráfico fuera del túnel.
- **Tailscale completo**: estado, up/down, elegir exit-node y anunciar rutas.

## Requisitos

- Linux con systemd, y Rust (`cargo`) para compilar. Si no lo tienes, el
  instalador se ofrece a ponerlo con rustup.
- Los clientes que vayas a usar (`openvpn3`, `openfortivpn`, `tailscale`): el
  instalador los instala por ti, y solo necesitas los que uses.

Probado de punta a punta en **Fedora, Debian, Ubuntu y Kali**.

> **Sobre `openvpn3` en Debian/Ubuntu**: el repo oficial del proyecto OpenVPN3
> Linux solo publica paquetes para codenames antiguos (`bullseye`, `jammy`,
> `focal`), no para `bookworm` ni `noble`. En Kali y en Debian sid/testing sí
> existe el paquete nativo `openvpn3-client`, y el instalador lo prefiere. Si tu
> versión no tiene ninguno de los dos, el instalador avisa y sigue: el resto
> (FortiVPN, Tailscale) funciona igual.

## Instalación

```bash
git clone https://github.com/ElIsaac/vpn_manager.git
cd vpn_manager
sudo ./install.sh
```

Layout estándar del sistema:

| Qué | Dónde |
| --- | --- |
| binario | `/usr/local/bin/vpn` |
| scripts privilegiados de IPv6 | `/usr/local/libexec/vpn/ipv6-*.sh` (root:root) |
| lanzador de escritorio | `/usr/local/libexec/vpn/vpn-launcher.sh` |
| entrada del menú de apps | `/usr/share/applications/vpn.desktop` |
| ícono | `/usr/share/icons/hicolor/512x512/apps/vpn.png` |
| tus datos | `~/.local/share/vpn-tui/vpn-tui.db` |

También queda en el lanzador de aplicaciones de tu escritorio: al abrirlo desde
ahí arranca en tu terminal (respeta `$TERMINAL`; si no, prefiere kitty, ghostty,
alacritty, foot, wezterm, ptyxis…).

## Uso

```bash
vpn            # abre la TUI
vpn 1          # entra directo a OpenVPN3, y conecta si hay un solo perfil
vpn 2          # lo mismo para OpenFortiVPN
vpn 3          # lo mismo para Tailscale
vpn --status   # una línea por perfil, sin TUI (para barras de estado y scripts)
```

### Atajos

| Tecla | Acción |
| --- | --- |
| `↑` `↓` / `k` `j` | mover la selección (o desplazar el registro si tiene el foco) |
| `←` `→` | cambiar de panel (VPNS → registro → ⚙ configuración) |
| `1` `2` `3` | saltar a la primera VPN de ese tipo |
| `enter` / `space` | conectar o cortar lo seleccionado |
| `a` / `x` | conectar todas / cortar todas |
| `6` | interruptor de IPv6 |
| `esc` | cerrar el modal actual, o salir |
| `?` | ayuda |
| `q` / `ctrl-c` | salir |

Al salir se cierran solo las conexiones que abrió la TUI; las que ya estaban
levantadas se quedan intactas.

### Agregar un perfil

Desde `⚙ configuración → perfiles`, o con `enter` sobre una fila
`… No Configurada`. El alta de OpenVPN3 va en dos pasos:

1. **El archivo**: arrastra el `.ovpn` sobre la ventana de la terminal (la
   terminal pega la ruta y la app la interpreta, incluyendo `file://`, comillas
   y espacios escapados), `F2` para el explorador gráfico del sistema, `F3` para
   el explorador dentro de la TUI, o teclea la ruta.
2. **El nombre**: opcional. Sin nombre la fila se ve como `OpenVPN3`; con
   nombre, `OpenVPN3 (trabajo)`. Solo es obligatorio si ya tienes otro perfil
   del mismo tipo.

## Dónde viven tus datos

En SQLite, en `~/.local/share/vpn-tui/vpn-tui.db` (`chmod 600`): ahí van los
perfiles y los ajustes.

Para FortiVPN puedes guardar la contraseña en la base de datos o —mejor— dejar
solo el **nombre de una variable de entorno** y exportarla en tu sesión. Cuando
sí se usa, nunca viaja por la línea de comandos: se escribe en un archivo
temporal `chmod 600` dentro de `$XDG_RUNTIME_DIR` que se borra al terminar, para
que no aparezca en `ps`.

## Permisos: qué se concede y por qué

La TUI usa `sudo -n` (nunca interactivo), así que lo privilegiado se autoriza al
instalar. Esto queda en `/etc/sudoers.d/vpn-tui`:

```
<usuario> ALL=(ALL) NOPASSWD: /usr/bin/openfortivpn
<usuario> ALL=(ALL) NOPASSWD: /usr/bin/pkill -INT  -x openfortivpn
<usuario> ALL=(ALL) NOPASSWD: /usr/bin/pkill -TERM -x openfortivpn
<usuario> ALL=(ALL) NOPASSWD: /usr/bin/pkill -KILL -x openfortivpn
<usuario> ALL=(ALL) NOPASSWD: /usr/local/libexec/vpn/ipv6-disable.sh
<usuario> ALL=(ALL) NOPASSWD: /usr/local/libexec/vpn/ipv6-enable.sh
```

Los `pkill` están acotados a una señal y a un nombre exacto de proceso. Los
scripts de IPv6 se instalan **root:root en `/usr/local/libexec`** precisamente
porque llevan `NOPASSWD`: si vivieran en tu home, cualquiera con tu cuenta
podría reescribirlos y ejecutar código como root sin contraseña.

Para Tailscale el instalador configura el
[modo operator](https://tailscale.com/kb/1080/cli)
(`tailscale set --operator=<usuario>`), que autoriza a tu usuario a hacer
`up`/`down`/`set` sin `sudo` y no concede nada fuera de Tailscale.

`openvpn3` no necesita permisos especiales: habla por D-Bus con su propio
servicio del sistema.

## Desinstalar

```bash
sudo rm -f /usr/local/bin/vpn \
           /usr/share/applications/vpn.desktop \
           /usr/share/icons/hicolor/512x512/apps/vpn.png \
           /etc/sudoers.d/vpn-tui
sudo rm -rf /usr/local/libexec/vpn
rm -rf ~/.local/share/vpn-tui       # tus perfiles (opcional)
sudo tailscale set --operator=""    # revertir el modo operator (opcional)
```

## Desarrollo

```bash
cargo test                                        # 48 tests
cargo build --release
VPN_TUI_DB=/tmp/prueba.db ./target/release/vpn    # con una base de datos aparte
```

```
src/
├── main.rs        CLI, arranque de la terminal y mapeo de teclas
├── app.rs         estado, transiciones y reconciliación con el sistema
├── db.rs          SQLite: perfiles y ajustes
├── event.rs       canal de mensajes: teclado, latido, sondeo y pegado
├── filedialog.rs  explorador gráfico del sistema (zenity/kdialog/yad)
├── ui.rs          dibujado (ratatui)
└── vpn/
    ├── mod.rs        ejecución con timeout, interfaces, formatos
    ├── openvpn3.rs   session-start / -manage / sessions-list / session-stats
    ├── fortivpn.rs   credenciales en archivo temporal, señales, pgrep
    └── tailscale.rs  status --json, up/down, exit-node, rutas
```

## Licencia

MIT — ver [LICENSE](LICENSE).
