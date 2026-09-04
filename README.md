# vpn-tui

TUI en Rust para gestionar túneles **OpenVPN3** y **OpenFortiVPN** desde una sola
pantalla: estado en vivo, tráfico por interfaz y el log del túnel sin salir de la
terminal. Equivalente al `vpn_manager.py` de `../vpns`, pero sin menús anidados
ni bloquear la terminal mientras hay una conexión abierta.

```
╭ vpn-tui ──────────────────────────────────────────────────────────────────────╮
│ VPN   1/2 activas   IPv6 off   tun1 100.96.1.66/28              14:42:52      │
╰───────────────────────────────────────────────────────────────────────────────╯
╭ perfiles ───────────╮╭ detalle ─────────────────────────╮╭ tráfico ──────────╮
│ ● OVPN  thinkpad    ││ tipo  OpenVPN3                   ││↓ rx 24.1 KiB/s    │
│       conectado 2:14││ destino  …/thinkpad.ovpn         ││   ▂▃▅▇▆▃▂         │
│ ○ FORTI corporativa ││ estado  conectado · 02:14        ││↑ tx 3.2 KiB/s     │
│       desconectado  ││ interfaz  tun1  100.96.1.66/28   ││   ▁▂▂▁▃▂▁         │
╰─────────────────────╯╰──────────────────────────────────╯╰───────────────────╯
```

## Características

- **Ambas VPNs a la vez o por separado**, sin bloquear la interfaz: cada conexión
  corre en su propio hilo y reporta por un canal.
- **Adopción de sesiones existentes**: si ya había un túnel levantado (por esta
  TUI, por el script de Python o a mano), lo detecta y lo muestra. Esas
  conexiones se marcan como externas y **no** se cierran al salir.
- **Reconciliación continua**: cada 2 s consulta `openvpn3 sessions-list`,
  `pgrep openfortivpn` y `/sys/class/net`, así que el estado que ves es el real,
  no el que la TUI cree recordar.
- **Log del túnel en vivo** con el stdout/stderr de `openfortivpn` coloreado por
  severidad, desplazable y con seguimiento automático del final.
- **Tráfico**: totales y velocidad instantánea con gráficas por perfil
  (`openvpn3 session-stats` para OpenVPN3, contadores de interfaz para Forti).
- **Credenciales fuera de `ps`**: `openfortivpn` recibe un archivo de
  configuración temporal `chmod 600` en `$XDG_RUNTIME_DIR`, que se borra al
  terminar el proceso, en vez de un `-p contraseña` visible en la lista de
  procesos.
- **Nunca pide contraseña**: todo lo que necesita root va por `sudo -n`, así que
  falla con un mensaje claro en lugar de dejar la TUI colgada.

## Instalación

```bash
cd /home/isaac/apps/vpn-tui
./install.sh          # compila e instala en ~/.local/bin
sudo ./install.sh     # además escribe /etc/sudoers.d/vpn-tui
```

O a mano:

```bash
cargo build --release
./target/release/vpn-tui
```

Requiere Rust 1.88+, y `openvpn3` / `openfortivpn` según los perfiles que uses.

## Configuración

`config.toml`, buscado en este orden:

1. `$VPN_TUI_CONFIG`
2. el directorio del binario y la raíz del proyecto
3. `./config.toml`
4. `~/.config/vpn-tui/config.toml`

`vpn-tui --init` escribe una plantilla en la ruta de usuario. El formato completo
está documentado en [`config.example.toml`](config.example.toml):

```toml
[general]
ipv6_script = "/home/isaac/apps/vpns/ipv6_deshabilitar.sh"
poll_interval_ms = 2000
ipv6_on_connect = false          # ejecutar el script tras cada conexión

[[openvpn]]
name = "thinkpad"
config = "/home/isaac/apps/vpns/thinkpad.ovpn"

[[fortivpn]]
name = "corporativa"
host = "203.0.113.10:10443"
username = "usuario"
password_env = "VPN_TUI_FORTI_PASSWORD"   # gana sobre `password` si está definida
trusted_cert = "1316d1…"
```

Las rutas relativas se resuelven contra la carpeta del `config.toml` y `~` se
expande. El archivo puede contener credenciales: se mantiene en `chmod 600` y
está en `.gitignore`.

## Atajos

| Tecla | Acción |
| --- | --- |
| `↑` `↓` / `k` `j` | mover selección (o desplazar el log si tiene el foco) |
| `enter` / `space` | conectar o desconectar el perfil seleccionado |
| `c` / `d` | conectar / desconectar el perfil |
| `a` / `x` | conectar / desconectar todos |
| `6` | ejecutar el script de IPv6 |
| `tab` | alternar foco entre perfiles y registro |
| `pgup` `pgdn`, `g`, `G` | desplazar el registro / ir al inicio / seguir el final |
| `r` | limpiar el registro |
| `?` | ayuda |
| `q` / `esc` / `ctrl-c` | salir |

Al salir se cierran las conexiones que abrió la TUI; las adoptadas se dejan
intactas.

## Modos no interactivos

```bash
vpn-tui --status      # estado de cada perfil e interfaces, una línea por entrada
vpn-tui --init        # plantilla de configuración
vpn-tui --help
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
isaac ALL=(ALL) NOPASSWD: /home/isaac/apps/vpns/ipv6_deshabilitar.sh
```

Las reglas de `pkill` están acotadas a la señal y al nombre exacto del proceso,
así que no sirven para tocar nada más. La contrapartida es que afectan a *todos*
los `openfortivpn` del sistema: si algún día corres varios túneles Forti a la
vez, añade una regla de `kill` por PID — la TUI ya lo intenta como segunda
opción.

`openvpn3` no necesita sudo: habla por D-Bus con el servicio del sistema.

## Estructura

```
src/
├── main.rs          CLI, arranque del terminal y mapeo de teclas
├── app.rs           estado, transiciones y reconciliación con el sistema
├── config.rs        carga y resolución del config.toml
├── event.rs         canal de mensajes: teclado, latido y sondeo
├── ui.rs            dibujado (ratatui)
└── vpn/
    ├── mod.rs       ejecución de comandos con timeout, interfaces, formatos
    ├── openvpn3.rs  session-start / -manage / sessions-list / session-stats
    └── fortivpn.rs  spawn con credenciales en archivo temporal, señales, pgrep
```

`cargo test` cubre el parseo de la salida de `openvpn3`, la clasificación de las
líneas de `openfortivpn` y el emparejamiento perfil ↔ sesión.

## Diferencias con `vpn_manager.py`

| | `vpn_manager.py` | `vpn-tui` |
| --- | --- | --- |
| Interfaz | menú, una acción a la vez | pantalla completa, estado permanente |
| Mientras hay conexión | terminal bloqueada en `input()` | interfaz viva, log en tiempo real |
| Credenciales | `-p contraseña` visible en `ps` | archivo temporal `chmod 600` |
| Estado | lo que el script recuerda | se reconcilia con el sistema cada 2 s |
| Sesiones previas | invisibles | adoptadas y mostradas |
| Perfiles | uno de cada tipo, en el código | los que quieras, en `config.toml` |
| Contraseña de sudo | podía quedarse esperando | `sudo -n`, falla con mensaje |
