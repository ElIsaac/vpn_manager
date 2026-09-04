//! vpn-tui — interfaz de terminal para OpenVPN3, OpenFortiVPN y Tailscale.

mod app;
mod db;
mod event;
mod ui;
mod vpn;

use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};

use app::{App, ConfigScreen, FileTarget, Focus, Kind, Screen, TailscaleScreen};
use db::Db;
use event::{Events, Level, Msg};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
vpn — gestor TUI de OpenVPN3, OpenFortiVPN y Tailscale

USO:
    vpn [1|2|3] [OPCIONES]

    (sin argumentos)   menú principal: 1) OpenVPN3  2) OpenFortiVPN  3) Tailscale
    vpn 1              salta el menú, entra directo a OpenVPN3 (conecta si hay 1 perfil)
    vpn 2              salta el menú, entra directo a OpenFortiVPN (conecta si hay 1 perfil)
    vpn 3              salta el menú, entra directo a Tailscale (conecta si no está arriba)

OPCIONES:
        --status      Imprime el estado y sale (sin TUI)
    -h, --help        Muestra esta ayuda
    -V, --version     Muestra la versión

BASE DE DATOS:
    SQLite en $VPN_TUI_DB, o en el directorio de datos del usuario
    (~/.local/share/vpn-tui/vpn-tui.db). Se crea sola la primera vez, migrando
    perfiles desde un config.toml viejo si lo encuentra.
";

/// A qué pantalla saltar al arrancar (`vpn 1`/`vpn 2`/`vpn 3`), saltando el
/// menú principal por completo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupTarget {
    MainMenu,
    OpenVpn,
    Forti,
    Tailscale,
}

enum Mode {
    Tui(StartupTarget),
    Status,
}

fn main() -> Result<()> {
    let mut mode = Mode::Tui(StartupTarget::MainMenu);

    let args = std::env::args().skip(1);
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("vpn {VERSION}");
                return Ok(());
            }
            "--status" => mode = Mode::Status,
            "1" => mode = Mode::Tui(StartupTarget::OpenVpn),
            "2" => mode = Mode::Tui(StartupTarget::Forti),
            "3" => mode = Mode::Tui(StartupTarget::Tailscale),
            other => {
                eprintln!("argumento desconocido: {other}\n");
                print!("{USAGE}");
                std::process::exit(2);
            }
        }
    }

    let db = Db::open()?;

    match mode {
        Mode::Status => print_status(&db),
        Mode::Tui(target) => run_tui(db, target),
    }
}

fn print_status(db: &Db) -> Result<()> {
    let sessions = vpn::openvpn3::sessions().unwrap_or_default();
    let forti = vpn::fortivpn::running_pids();

    for profile in db.openvpn_profiles().unwrap_or_default() {
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
    for (i, profile) in db.forti_profiles().unwrap_or_default().iter().enumerate() {
        match forti.get(i) {
            Some(pid) => println!("forti    {:<16} conectado   pid {pid}", profile.name),
            None => println!("forti    {:<16} desconectado", profile.name),
        }
    }
    if vpn::tailscale::is_available() {
        match vpn::tailscale::status() {
            Ok(status) => println!(
                "tailscale        {:<11} {}",
                status.backend_state,
                status.self_ips.join(", ")
            ),
            Err(err) => println!("tailscale        error: {err}"),
        }
    }
    for iface in vpn::tunnel_interfaces() {
        println!("iface    {:<16} {}", iface.name, iface.addr);
    }
    Ok(())
}

fn run_tui(db: Db, target: StartupTarget) -> Result<()> {
    let general = db.general().unwrap_or_default();
    let poll_interval = Duration::from_millis(general.poll_interval_ms);
    let events = Events::start(poll_interval);
    let mut app = App::new(db, events.tx.clone());

    // `vpn 1`/`vpn 2`/`vpn 3` saltan el menú principal directo a la pantalla
    // correspondiente, y disparan la conexión de inmediato cuando es
    // inequívoco (un solo perfil de ese tipo, o Tailscale si no está arriba).
    match target {
        StartupTarget::MainMenu => {}
        StartupTarget::OpenVpn => {
            app.enter_dashboard(Kind::OpenVpn);
            app.autoconnect_single(Kind::OpenVpn);
        }
        StartupTarget::Forti => {
            app.enter_dashboard(Kind::Forti);
            app.autoconnect_single(Kind::Forti);
        }
        StartupTarget::Tailscale => {
            app.enter_tailscale();
            app.tailscale_autoconnect();
        }
    }

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
        match key.code {
            KeyCode::Char('q') => app.should_quit = true,
            _ => app.show_help = false,
        }
        return;
    }

    match app.screen {
        Screen::MainMenu => on_key_main_menu(app, key),
        Screen::Dashboard(kind) => on_key_dashboard(app, key, kind),
        Screen::Tailscale(screen) => on_key_tailscale(app, key, screen),
        Screen::Config(screen) => on_key_config(app, key, screen),
    }
}

fn on_key_main_menu(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('1') => app.enter_dashboard(Kind::OpenVpn),
        KeyCode::Char('2') => app.enter_dashboard(Kind::Forti),
        KeyCode::Char('3') => app.enter_tailscale(),
        KeyCode::Char('c') => app.enter_config_menu(),
        KeyCode::Char('?') | KeyCode::F(1) => app.show_help = true,
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        _ => {}
    }
}

fn on_key_dashboard(app: &mut App, key: KeyEvent, kind: Kind) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.back_to_main_menu(),
        KeyCode::Char('?') | KeyCode::F(1) => app.show_help = true,
        KeyCode::Tab | KeyCode::BackTab => app.toggle_focus(),

        KeyCode::Up | KeyCode::Char('k') => match app.focus {
            Focus::Profiles => app.select_prev(),
            Focus::Logs => app.scroll_logs(-1),
        },
        KeyCode::Down | KeyCode::Char('j') => match app.focus {
            Focus::Profiles => app.select_next(),
            Focus::Logs => app.scroll_logs(1),
        },

        KeyCode::Enter | KeyCode::Char(' ') => {
            if let Some(idx) = app.selected_absolute() {
                app.toggle(idx);
            }
        }
        KeyCode::Char('c') => {
            if let Some(idx) = app.selected_absolute() {
                app.connect(idx);
            }
        }
        KeyCode::Char('d') => {
            if let Some(idx) = app.selected_absolute() {
                app.disconnect(idx);
            }
        }
        KeyCode::Char('a') => app.connect_all(Some(kind)),
        KeyCode::Char('x') => app.disconnect_all(Some(kind)),

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

fn on_key_tailscale(app: &mut App, key: KeyEvent, screen: TailscaleScreen) {
    match screen {
        TailscaleScreen::Status => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => app.back_to_main_menu(),
            KeyCode::Enter | KeyCode::Char(' ') => app.tailscale_toggle(),
            KeyCode::Char('e') => {
                app.exit_node_selected = 0;
                app.screen = Screen::Tailscale(TailscaleScreen::ExitNodePicker);
            }
            KeyCode::Char('r') => {
                let current = app
                    .tailscale_status
                    .as_ref()
                    .map(|s| s.advertise_routes.join(","))
                    .unwrap_or_default();
                app.routes_input = tui_input::Input::new(current);
                app.screen = Screen::Tailscale(TailscaleScreen::RoutesForm);
            }
            _ => {}
        },
        TailscaleScreen::ExitNodePicker => {
            let n = app
                .tailscale_status
                .as_ref()
                .map(|s| s.exit_node_candidates.len())
                .unwrap_or(0);
            match key.code {
                KeyCode::Esc => app.screen = Screen::Tailscale(TailscaleScreen::Status),
                KeyCode::Up | KeyCode::Char('k')
                    if n > 0 => {
                        app.exit_node_selected = (app.exit_node_selected + n - 1) % n;
                    }
                KeyCode::Down | KeyCode::Char('j')
                    if n > 0 => {
                        app.exit_node_selected = (app.exit_node_selected + 1) % n;
                    }
                KeyCode::Char('c') => {
                    app.tailscale_apply_exit_node(None);
                    app.screen = Screen::Tailscale(TailscaleScreen::Status);
                }
                KeyCode::Enter => {
                    let ip = app
                        .tailscale_status
                        .as_ref()
                        .and_then(|s| s.exit_node_candidates.get(app.exit_node_selected))
                        .and_then(|c| c.tailscale_ip.clone());
                    if let Some(ip) = ip {
                        app.tailscale_apply_exit_node(Some(ip));
                    }
                    app.screen = Screen::Tailscale(TailscaleScreen::Status);
                }
                _ => {}
            }
        }
        TailscaleScreen::RoutesForm => match key.code {
            KeyCode::Esc => app.screen = Screen::Tailscale(TailscaleScreen::Status),
            KeyCode::Enter => app.tailscale_apply_routes(),
            _ => {
                let event = TermEvent::Key(key);
                if let Some(req) = tui_input::backend::crossterm::to_input_request(&event) {
                    app.routes_input.handle(req);
                }
            }
        },
    }
}

fn on_key_config(app: &mut App, key: KeyEvent, screen: ConfigScreen) {
    match screen {
        ConfigScreen::Menu => on_key_config_menu(app, key),
        ConfigScreen::ProfileList(kind) => on_key_profile_list(app, key, kind),
        ConfigScreen::ProfileForm => on_key_profile_form(app, key),
        ConfigScreen::FilePicker => on_key_file_picker(app, key),
        ConfigScreen::General => on_key_general(app, key),
        ConfigScreen::Ipv6Status => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => app.enter_config_menu(),
            KeyCode::Char('t') => app.toggle_ipv6(),
            _ => {}
        },
        ConfigScreen::OpenVpnSessions => on_key_sessions(app, key),
    }
}

const CONFIG_MENU_LEN: usize = 5;

fn on_key_config_menu(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.back_to_main_menu(),
        KeyCode::Up | KeyCode::Char('k') => {
            app.config_menu_selected = (app.config_menu_selected + CONFIG_MENU_LEN - 1) % CONFIG_MENU_LEN;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.config_menu_selected = (app.config_menu_selected + 1) % CONFIG_MENU_LEN;
        }
        KeyCode::Enter => match app.config_menu_selected {
            0 => {
                app.profile_list_selected = 0;
                app.screen = Screen::Config(ConfigScreen::ProfileList(Kind::OpenVpn));
            }
            1 => {
                app.profile_list_selected = 0;
                app.screen = Screen::Config(ConfigScreen::ProfileList(Kind::Forti));
            }
            2 => app.open_general_form(),
            3 => app.screen = Screen::Config(ConfigScreen::Ipv6Status),
            4 => app.screen = Screen::Config(ConfigScreen::OpenVpnSessions),
            _ => {}
        },
        _ => {}
    }
}

fn on_key_profile_list(app: &mut App, key: KeyEvent, kind: Kind) {
    let len = match kind {
        Kind::OpenVpn => app.openvpn_profiles.len(),
        Kind::Forti => app.forti_profiles.len(),
    };
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.enter_config_menu(),
        KeyCode::Up | KeyCode::Char('k')
            if len > 0 => {
                app.profile_list_selected = (app.profile_list_selected + len - 1) % len;
            }
        KeyCode::Down | KeyCode::Char('j')
            if len > 0 => {
                app.profile_list_selected = (app.profile_list_selected + 1) % len;
            }
        KeyCode::Char('n') => app.open_profile_form_new(kind),
        KeyCode::Char('e') if len > 0 => app.open_profile_form_edit(kind, app.profile_list_selected),
        KeyCode::Char('x') if len > 0 => {
            app.delete_profile(kind, app.profile_list_selected);
            app.profile_list_selected = 0;
        }
        _ => {}
    }
}

fn on_key_profile_form(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            let kind = app.profile_form.take().map(|f| f.kind).unwrap_or(Kind::OpenVpn);
            app.screen = Screen::Config(ConfigScreen::ProfileList(kind));
        }
        KeyCode::Tab => {
            if let Some(form) = app.profile_form.as_mut() {
                form.next_field();
            }
        }
        KeyCode::BackTab => {
            if let Some(form) = app.profile_form.as_mut() {
                form.prev_field();
            }
        }
        KeyCode::F(3) => {
            let Some(form) = app.profile_form.as_ref() else { return };
            let idx = form.focus;
            if form.fields.get(idx).map(|f| f.pickable).unwrap_or(false) {
                let start = std::path::PathBuf::from(form.fields[idx].input.value());
                let start = start.parent().map(std::path::Path::to_path_buf);
                app.open_file_picker(FileTarget::Profile(idx), start.as_deref());
            }
        }
        KeyCode::Enter => {
            let kind_before = app.profile_form.as_ref().map(|f| f.kind);
            if app.save_profile_form() {
                if let Some(kind) = kind_before {
                    app.screen = Screen::Config(ConfigScreen::ProfileList(kind));
                }
            }
        }
        _ => {
            let event = TermEvent::Key(key);
            if let Some(form) = app.profile_form.as_mut() {
                if let Some(field) = form.fields.get_mut(form.focus) {
                    if let Some(req) = tui_input::backend::crossterm::to_input_request(&event) {
                        field.input.handle(req);
                    }
                }
            }
        }
    }
}

fn on_key_file_picker(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Esc {
        app.cancel_file_picker();
        return;
    }
    let is_file_confirm = key.code == KeyCode::Enter
        && app
            .file_explorer
            .as_ref()
            .map(|e| e.current().is_file())
            .unwrap_or(false);
    if is_file_confirm {
        if let Some(path) = app.file_explorer.as_ref().map(|e| e.current().path.clone()) {
            app.apply_picked_file(path);
        }
        return;
    }
    if let Some(explorer) = app.file_explorer.as_mut() {
        let _ = explorer.handle(&TermEvent::Key(key));
    }
}

fn on_key_general(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.general_form = None;
            app.enter_config_menu();
        }
        KeyCode::Tab => {
            if let Some(form) = app.general_form.as_mut() {
                form.next_field();
            }
        }
        KeyCode::BackTab => {
            if let Some(form) = app.general_form.as_mut() {
                form.prev_field();
            }
        }
        KeyCode::F(3) => {
            let Some(form) = app.general_form.as_ref() else { return };
            match form.focus {
                2 => {
                    let start = std::path::PathBuf::from(form.ipv6_disable_script.value())
                        .parent()
                        .map(std::path::Path::to_path_buf);
                    app.open_file_picker(FileTarget::General(0), start.as_deref());
                }
                3 => {
                    let start = std::path::PathBuf::from(form.ipv6_enable_script.value())
                        .parent()
                        .map(std::path::Path::to_path_buf);
                    app.open_file_picker(FileTarget::General(1), start.as_deref());
                }
                _ => {}
            }
        }
        KeyCode::Char(' ') if app.general_form.as_ref().map(|f| f.focus) == Some(4) => {
            if let Some(form) = app.general_form.as_mut() {
                form.ipv6_on_connect = !form.ipv6_on_connect;
            }
        }
        KeyCode::Enter => app.save_general_form(),
        _ => {
            let focus = app.general_form.as_ref().map(|f| f.focus).unwrap_or(0);
            if focus == 4 {
                return; // el toggle no es un campo de texto
            }
            let event = TermEvent::Key(key);
            if let Some(req) = tui_input::backend::crossterm::to_input_request(&event) {
                if let Some(form) = app.general_form.as_mut() {
                    match focus {
                        0 => form.poll_interval_ms.handle(req),
                        1 => form.log_capacity.handle(req),
                        2 => form.ipv6_disable_script.handle(req),
                        3 => form.ipv6_enable_script.handle(req),
                        _ => None,
                    };
                }
            }
        }
    }
}

fn on_key_sessions(app: &mut App, key: KeyEvent) {
    let n = app.all_openvpn_sessions.len();
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('n') => app.enter_config_menu(),
        KeyCode::Up | KeyCode::Char('k')
            if n > 0 => {
                app.sessions_selected = (app.sessions_selected + n - 1) % n;
            }
        KeyCode::Down | KeyCode::Char('j')
            if n > 0 => {
                app.sessions_selected = (app.sessions_selected + 1) % n;
            }
        KeyCode::Char('d') => app.kill_selected_session(),
        KeyCode::Char('a') => app.kill_all_sessions(),
        _ => {}
    }
}
