//! vpn-tui — interfaz de terminal para OpenVPN3 y OpenFortiVPN.

mod app;
mod config;
mod event;
mod ui;
mod vpn;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};

use app::App;
use config::Config;
use event::{Events, Level, Msg};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
vpn-tui — gestor TUI de OpenVPN3 y OpenFortiVPN

USO:
    vpn-tui [OPCIONES]

OPCIONES:
    -c, --config <RUTA>   Usa este archivo de configuración
        --init            Escribe una plantilla en ~/.config/vpn-tui/config.toml
        --status          Imprime el estado y sale (sin TUI)
    -h, --help            Muestra esta ayuda
    -V, --version         Muestra la versión

CONFIGURACIÓN:
    Se busca en $VPN_TUI_CONFIG, el directorio del binario, ./config.toml y
    ~/.config/vpn-tui/config.toml (en ese orden).
";

enum Mode {
    Tui,
    Status,
    Init,
}

fn main() -> Result<()> {
    let mut mode = Mode::Tui;
    let mut explicit_config: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("vpn-tui {VERSION}");
                return Ok(());
            }
            "-c" | "--config" => {
                let value = args.next().context("--config necesita una ruta")?;
                explicit_config = Some(PathBuf::from(value));
            }
            "--status" => mode = Mode::Status,
            "--init" => mode = Mode::Init,
            other => {
                eprintln!("argumento desconocido: {other}\n");
                print!("{USAGE}");
                std::process::exit(2);
            }
        }
    }

    if matches!(mode, Mode::Init) {
        return init_config();
    }

    let cfg = match explicit_config {
        Some(path) => Config::from_path(&path)?,
        None => Config::load()?,
    };

    match mode {
        Mode::Status => print_status(&cfg),
        Mode::Init => unreachable!(),
        Mode::Tui => run_tui(cfg),
    }
}

fn init_config() -> Result<()> {
    let target = Config::user_path();
    if target.exists() {
        println!("ya existe: {}", target.display());
        return Ok(());
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&target, config::TEMPLATE)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("plantilla escrita en {}", target.display());
    Ok(())
}

/// Salida no interactiva, útil para scripts y barras de estado.
fn print_status(cfg: &Config) -> Result<()> {
    let sessions = vpn::openvpn3::sessions().unwrap_or_default();
    let forti = vpn::fortivpn::running_pids();

    for profile in &cfg.openvpn {
        let session = sessions
            .iter()
            .find(|s| vpn::openvpn3::matches_config(s, &profile.config));
        match session {
            Some(s) if s.is_connected() => {
                println!("openvpn  {:<16} conectado   {}", profile.name, s.path)
            }
            _ => println!("openvpn  {:<16} desconectado", profile.name),
        }
    }
    for (i, profile) in cfg.fortivpn.iter().enumerate() {
        match forti.get(i) {
            Some(pid) => println!("forti    {:<16} conectado   pid {pid}", profile.name),
            None => println!("forti    {:<16} desconectado", profile.name),
        }
    }
    for iface in vpn::tunnel_interfaces() {
        println!("iface    {:<16} {}", iface.name, iface.addr);
    }
    Ok(())
}

fn run_tui(cfg: Config) -> Result<()> {
    let poll_interval = Duration::from_millis(cfg.general.poll_interval_ms);
    let events = Events::start(poll_interval);
    let mut app = App::new(cfg, events.tx.clone());

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &events);
    ratatui::restore();

    // Cierra lo que abrió la TUI, ya fuera de la pantalla alternativa.
    let pending: Vec<String> = app
        .profiles
        .iter()
        .filter(|p| p.state.is_up() && !p.adopted)
        .map(|p| p.name.clone())
        .collect();
    if !pending.is_empty() {
        println!("cerrando conexiones: {}", pending.join(", "));
        app.shutdown();
        println!("listo");
    }

    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    events: &Events,
) -> Result<()> {
    terminal.draw(|frame| ui::draw(frame, app))?;

    while let Ok(msg) = events.rx.recv() {
        dispatch(app, msg);
        // Agrupa lo que ya esté en la cola para no redibujar de más.
        while let Ok(msg) = events.rx.try_recv() {
            dispatch(app, msg);
        }
        if app.should_quit {
            break;
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
    }
    Ok(())
}

fn dispatch(app: &mut App, msg: Msg) {
    match msg {
        Msg::Input(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => on_key(app, key),
        Msg::Input(_) => {}
        other => app.handle(other),
    }
}

fn on_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return;
    }

    if app.show_help {
        // Con la ayuda abierta, cualquier tecla la cierra.
        match key.code {
            KeyCode::Char('q') => app.should_quit = true,
            _ => app.show_help = false,
        }
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('?') | KeyCode::F(1) => app.show_help = true,
        KeyCode::Tab | KeyCode::BackTab => app.toggle_focus(),

        KeyCode::Up | KeyCode::Char('k') => match app.focus {
            app::Focus::Profiles => app.select_prev(),
            app::Focus::Logs => app.scroll_logs(-1),
        },
        KeyCode::Down | KeyCode::Char('j') => match app.focus {
            app::Focus::Profiles => app.select_next(),
            app::Focus::Logs => app.scroll_logs(1),
        },

        KeyCode::Enter | KeyCode::Char(' ') => app.toggle(app.selected),
        KeyCode::Char('c') => app.connect(app.selected),
        KeyCode::Char('d') => app.disconnect(app.selected),
        KeyCode::Char('a') => app.connect_all(),
        KeyCode::Char('x') => app.disconnect_all(),
        KeyCode::Char('6') => app.run_ipv6_script(),

        KeyCode::Char('r') => {
            app.logs.clear();
            app.follow_logs();
            app.toast(Level::Info, "registro limpiado");
        }
        KeyCode::PageUp => app.scroll_logs(-(app.log_viewport as i32)),
        KeyCode::PageDown => app.scroll_logs(app.log_viewport as i32),
        KeyCode::Char('g') | KeyCode::Home => app.log_top(),
        KeyCode::Char('G') | KeyCode::End => app.follow_logs(),
        _ => {}
    }
}
