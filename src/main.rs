//! vpn-tui — interfaz de terminal para OpenVPN3, OpenFortiVPN y Tailscale.

mod app;
mod db;
mod event;
mod filedialog;
mod ui;
mod vpn;

use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;

use app::{
    App, ConfigScreen, FileTarget, Focus, FormOrigin, FormStep, Kind, Screen, TailscaleScreen,
};
use db::Db;
use event::{Events, Level, Msg};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
vpn — gestor TUI de OpenVPN3, OpenFortiVPN y Tailscale

USO:
    vpn [1|2|3] [OPCIONES]

    (sin argumentos)   panel VPNS con los tres tipos
    vpn 1              selecciona OpenVPN3 (conecta si hay 1 perfil)
    vpn 2              selecciona OpenFortiVPN (conecta si hay 1 perfil)
    vpn 3              selecciona Tailscale (conecta si no está arriba)

OPCIONES:
        --status      Imprime el estado y sale (sin TUI)
    -h, --help        Muestra esta ayuda
    -V, --version     Muestra la versión

BASE DE DATOS:
    SQLite en $VPN_TUI_DB, o en el directorio de datos del usuario
    (~/.local/share/vpn-tui/vpn-tui.db). Se crea sola la primera vez, migrando
    perfiles desde un config.toml viejo si lo encuentra.
";

/// Selección inicial (`vpn 1`/`vpn 2`/`vpn 3`) dentro del panel VPNS único.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupTarget {
    Default,
    OpenVpn,
    Forti,
    Tailscale,
}

enum Mode {
    Tui(StartupTarget),
    Status,
}

fn main() -> Result<()> {
    let mut mode = Mode::Tui(StartupTarget::Default);

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
        let name = app::display_name(Kind::OpenVpn, profile.name.as_deref());
        let session = sessions
            .iter()
            .find(|s| vpn::openvpn3::matches_config(s, &profile.config));
        match session {
            Some(s) if s.is_connected() => {
                println!("openvpn  {name:<24} conectado   {}", s.path)
            }
            _ => println!("openvpn  {name:<24} desconectado"),
        }
    }
    for (i, profile) in db.forti_profiles().unwrap_or_default().iter().enumerate() {
        let name = app::display_name(Kind::Forti, profile.name.as_deref());
        match forti.get(i) {
            Some(pid) => println!("forti    {name:<24} conectado   pid {pid}"),
            None => println!("forti    {name:<24} desconectado"),
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

    // `vpn 1`/`vpn 2`/`vpn 3` seleccionan de inmediato esa fila en el panel
    // VPNS único y disparan la conexión cuando es inequívoco (un solo perfil
    // de ese tipo, o Tailscale si no está arriba).
    match target {
        StartupTarget::Default => {}
        StartupTarget::OpenVpn => {
            app.select_kind(Kind::OpenVpn);
            app.autoconnect_single(Kind::OpenVpn);
        }
        StartupTarget::Forti => {
            app.select_kind(Kind::Forti);
            app.autoconnect_single(Kind::Forti);
        }
        StartupTarget::Tailscale => {
            app.select_kind(Kind::Tailscale);
            app.tailscale_autoconnect();
        }
    }

    let mut terminal = ratatui::init();
    enable_bracketed_paste();
    let result = event_loop(&mut terminal, &mut app, &events);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
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

/// Sin bracketed paste, soltar un archivo sobre la terminal (que el emulador
/// traduce a pegar la ruta) llegaría como una ráfaga de teclas sueltas, con su
/// `enter` final enviando el formulario a medias. Con él, llega entero como un
/// solo `Event::Paste`.
///
/// El modo se apaga también si el programa revienta: `ratatui::init()` deja un
/// hook de pánico que restaura la terminal, y aquí se encadena el nuestro
/// delante para que corra antes de esa restauración.
fn enable_bracketed_paste() {
    if execute!(std::io::stdout(), EnableBracketedPaste).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        previous(info);
    }));
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
        Msg::Input(TermEvent::Paste(text)) => app.on_paste(text),
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
        Screen::Dashboard => on_key_dashboard(app, key),
        Screen::Tailscale(screen) => on_key_tailscale(app, key, screen),
        Screen::Config(screen) => on_key_config(app, key, screen),
        Screen::Setup(_) => on_key_setup(app, key),
    }
}

/// Asistente de primera configuración: carrusel con `←`/`→`, `enter` entra a
/// configurar ese tipo, `esc` sale al dashboard.
fn on_key_setup(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.back_to_dashboard(),
        KeyCode::Right | KeyCode::Char('l') => app.setup_slide(true),
        KeyCode::Left | KeyCode::Char('h') => app.setup_slide(false),
        KeyCode::Char('1') => app.enter_setup(Kind::OpenVpn),
        KeyCode::Char('2') => app.enter_setup(Kind::Forti),
        KeyCode::Char('3') => app.enter_setup(Kind::Tailscale),
        KeyCode::Enter | KeyCode::Char(' ') => app.setup_confirm(),
        KeyCode::Char('?') | KeyCode::F(1) => app.show_help = true,
        _ => {}
    }
}

fn on_key_dashboard(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('?') | KeyCode::F(1) => app.show_help = true,

        // Los paneles se recorren con ←/→ (incluido el botón ⚙); `tab` ya no
        // mueve el foco aquí — sigue siendo "siguiente campo" en los
        // formularios, que es otro contexto.
        KeyCode::Right => app.focus_next(),
        KeyCode::Left => app.focus_prev(),

        KeyCode::Char('1') => app.select_kind(Kind::OpenVpn),
        KeyCode::Char('2') => app.select_kind(Kind::Forti),
        KeyCode::Char('3') => app.select_kind(Kind::Tailscale),

        KeyCode::Up | KeyCode::Char('k') => match app.focus {
            Focus::Profiles | Focus::ConfigButton => app.select_prev(),
            Focus::Logs => app.scroll_logs(-1),
        },
        KeyCode::Down | KeyCode::Char('j') => match app.focus {
            Focus::Profiles | Focus::ConfigButton => app.select_next(),
            Focus::Logs => app.scroll_logs(1),
        },

        KeyCode::Enter | KeyCode::Char(' ') => {
            if app.focus == Focus::ConfigButton {
                app.enter_config_menu();
            } else if let Some(idx) = app.selected_absolute() {
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
        KeyCode::Char('a') => app.connect_all(),
        KeyCode::Char('x') => app.disconnect_all(),

        KeyCode::Char('e') => match app.selected_profile().map(|p| p.kind) {
            Some(Kind::Tailscale) => app.enter_exit_node_picker(),
            _ => app.toast(Level::Warn, "'e' solo aplica con Tailscale seleccionado"),
        },
        KeyCode::Char('R') => match app.selected_profile().map(|p| p.kind) {
            Some(Kind::Tailscale) => app.enter_routes_form(),
            _ => app.toast(Level::Warn, "'R' solo aplica con Tailscale seleccionado"),
        },

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
        TailscaleScreen::ExitNodePicker => {
            let n = app
                .tailscale_status
                .as_ref()
                .map(|s| s.exit_node_candidates.len())
                .unwrap_or(0);
            match key.code {
                KeyCode::Esc => app.screen = Screen::Dashboard,
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
                    app.screen = Screen::Dashboard;
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
                    app.screen = Screen::Dashboard;
                }
                _ => {}
            }
        }
        TailscaleScreen::RoutesForm => match key.code {
            KeyCode::Esc => app.screen = Screen::Dashboard,
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
        ConfigScreen::ConfirmDelete { kind, idx } => match key.code {
            KeyCode::Char('s') | KeyCode::Char('y') | KeyCode::Enter => {
                app.delete_profile(kind, idx);
                app.profile_list_selected = 0;
                app.screen = Screen::Config(ConfigScreen::ProfileList(kind));
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                app.screen = Screen::Config(ConfigScreen::ProfileList(kind));
            }
            _ => {}
        },
    }
}

const CONFIG_MENU_LEN: usize = 5;

fn on_key_config_menu(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.back_to_dashboard(),
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
        Kind::Tailscale => 0,
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
            // Borrar tira configuración (y credenciales, en Forti): se
            // confirma antes, nunca directo.
            app.screen = Screen::Config(ConfigScreen::ConfirmDelete {
                kind,
                idx: app.profile_list_selected,
            });
        }
        _ => {}
    }
}

/// A dónde vuelve el formulario de perfil al guardar o cancelar: a la lista de
/// config si vino de ahí, o al dashboard si lo abrió el asistente.
fn profile_form_exit(kind: Kind, origin: FormOrigin) -> Screen {
    match origin {
        FormOrigin::ConfigList => Screen::Config(ConfigScreen::ProfileList(kind)),
        FormOrigin::Setup => Screen::Dashboard,
    }
}

fn on_key_profile_form(app: &mut App, key: KeyEvent) {
    // Con el diálogo del sistema abierto la pantalla está esperando: aceptar
    // teclas aquí solo serviría para dejar el formulario en un estado raro
    // cuando llegue la respuesta.
    if app.gui_picker_busy {
        return;
    }
    let step = app.profile_form.as_ref().map(|f| f.step).unwrap_or(FormStep::All);

    match key.code {
        // En el paso del nombre, `esc` es "volver al archivo", no "cancelar
        // todo": ya se hizo el trabajo de elegirlo.
        KeyCode::Esc if step == FormStep::Name => app.back_profile_step(),
        KeyCode::Esc => {
            let (kind, origin) = app
                .profile_form
                .take()
                .map(|f| (f.kind, f.origin))
                .unwrap_or((Kind::OpenVpn, FormOrigin::ConfigList));
            app.screen = profile_form_exit(kind, origin);
        }
        // Explorador de archivos del escritorio (el de siempre), frente al F3
        // que abre el que vive dentro de la terminal.
        KeyCode::F(2) => app.open_gui_file_picker(),
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
        // Paso 1 del asistente: `enter` no guarda todavía, avanza a preguntar
        // el nombre.
        KeyCode::Enter if step == FormStep::PickFile => {
            app.advance_profile_step();
        }
        KeyCode::Enter => {
            let before = app.profile_form.as_ref().map(|f| (f.kind, f.origin));
            if app.save_profile_form() {
                if let Some((kind, origin)) = before {
                    app.screen = profile_form_exit(kind, origin);
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
    // Con un cierre en curso solo se permite mirar: nada de disparar otro
    // `disconnect` sobre la misma sesión ni salir a media faena.
    if app.sessions_busy() && !matches!(key.code, KeyCode::Up | KeyCode::Down) {
        return;
    }
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
