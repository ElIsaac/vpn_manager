//! Estado de la aplicación y transiciones.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui_explorer::FileExplorer;
use tui_input::Input;

use crate::db::{Db, FortiProfile, General, OpenVpnProfile};
use crate::event::{Level, Msg, Snapshot};
use crate::vpn::{self, fortivpn, human_bytes, openvpn3, tailscale, Iface, State, Stats};

/// Muestras de velocidad que se conservan para las gráficas.
const RATE_HISTORY: usize = 80;
/// Cuánto se queda visible un aviso en la barra de estado.
const TOAST_TTL: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    OpenVpn,
    Forti,
    Tailscale,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::OpenVpn => "OpenVPN3",
            Kind::Forti => "OpenFortiVPN",
            Kind::Tailscale => "Tailscale",
        }
    }
}

/// Pantalla visible. `Dashboard` es la entrada: un solo panel "VPNS" con los
/// tres tipos mezclados (perfiles OpenVPN3 + FortiVPN + la fila única de
/// Tailscale) — todo lo demás vive en `Config`, o en `Tailscale` para las dos
/// subpantallas propias de exit-node/rutas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Tailscale(TailscaleScreen),
    Config(ConfigScreen),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailscaleScreen {
    ExitNodePicker,
    RoutesForm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScreen {
    Menu,
    ProfileList(Kind),
    ProfileForm,
    FilePicker,
    General,
    Ipv6Status,
    OpenVpnSessions,
}

/// A qué campo vuelve la ruta elegida en el `FilePicker`.
#[derive(Debug, Clone, Copy)]
pub enum FileTarget {
    /// Índice de campo dentro de `ProfileForm::fields`.
    Profile(usize),
    /// Índice de campo dentro de `GeneralForm` (0 = script deshabilitar IPv6,
    /// 1 = script habilitar IPv6).
    General(usize),
}

/// Un campo de texto editable en un formulario.
pub struct FormField {
    pub label: &'static str,
    pub input: Input,
    pub mask: bool,
    /// Si `true`, este campo tiene una acción de "buscar archivo" (F3).
    pub pickable: bool,
}

impl FormField {
    fn new(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            input: Input::new(value.into()),
            mask: false,
            pickable: false,
        }
    }

    fn masked(mut self) -> Self {
        self.mask = true;
        self
    }

    fn with_picker(mut self) -> Self {
        self.pickable = true;
        self
    }
}

/// Formulario de alta/edición de un perfil (OpenVPN3 o FortiVPN).
pub struct ProfileForm {
    pub kind: Kind,
    pub editing_id: Option<i64>,
    pub fields: Vec<FormField>,
    pub focus: usize,
    pub error: Option<String>,
}

impl ProfileForm {
    pub fn new_openvpn(existing: Option<&OpenVpnProfile>) -> Self {
        let fields = vec![
            FormField::new("nombre", existing.map(|p| p.name.clone()).unwrap_or_default()),
            FormField::new(
                "ruta .ovpn (F3 buscar)",
                existing
                    .map(|p| p.config.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
            .with_picker(),
        ];
        Self {
            kind: Kind::OpenVpn,
            editing_id: existing.map(|p| p.id),
            fields,
            focus: 0,
            error: None,
        }
    }

    pub fn new_forti(existing: Option<&FortiProfile>) -> Self {
        let fields = vec![
            FormField::new("nombre", existing.map(|p| p.name.clone()).unwrap_or_default()),
            FormField::new("host:puerto", existing.map(|p| p.host.clone()).unwrap_or_default()),
            FormField::new("usuario", existing.map(|p| p.username.clone()).unwrap_or_default()),
            FormField::new(
                "password",
                existing.and_then(|p| p.password.clone()).unwrap_or_default(),
            )
            .masked(),
            FormField::new(
                "password_env (opcional)",
                existing.and_then(|p| p.password_env.clone()).unwrap_or_default(),
            ),
            FormField::new(
                "trusted_cert (opcional)",
                existing.and_then(|p| p.trusted_cert.clone()).unwrap_or_default(),
            ),
            FormField::new(
                "extra_args (coma)",
                existing.map(|p| p.extra_args.join(",")).unwrap_or_default(),
            ),
        ];
        Self {
            kind: Kind::Forti,
            editing_id: existing.map(|p| p.id),
            fields,
            focus: 0,
            error: None,
        }
    }

    pub fn next_field(&mut self) {
        if !self.fields.is_empty() {
            self.focus = (self.focus + 1) % self.fields.len();
        }
    }

    pub fn prev_field(&mut self) {
        if !self.fields.is_empty() {
            self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
        }
    }
}

/// Ajustes generales, editados como formulario dentro de `Config::General`.
pub struct GeneralForm {
    pub poll_interval_ms: Input,
    pub log_capacity: Input,
    pub ipv6_disable_script: Input,
    pub ipv6_enable_script: Input,
    pub ipv6_on_connect: bool,
    pub focus: usize,
}

impl GeneralForm {
    pub fn from(general: &General) -> Self {
        Self {
            poll_interval_ms: Input::new(general.poll_interval_ms.to_string()),
            log_capacity: Input::new(general.log_capacity.to_string()),
            ipv6_disable_script: Input::new(
                general
                    .ipv6_disable_script
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            ipv6_enable_script: Input::new(
                general
                    .ipv6_enable_script
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            ipv6_on_connect: general.ipv6_on_connect,
            focus: 0,
        }
    }

    /// Índices de campo: 0 poll_interval_ms, 1 log_capacity,
    /// 2 ipv6_disable_script, 3 ipv6_enable_script, 4 toggle ipv6_on_connect.
    pub const FIELD_COUNT: usize = 5;

    pub fn next_field(&mut self) {
        self.focus = (self.focus + 1) % Self::FIELD_COUNT;
    }

    pub fn prev_field(&mut self) {
        self.focus = (self.focus + Self::FIELD_COUNT - 1) % Self::FIELD_COUNT;
    }
}

/// Un perfil de VPN tal y como lo ve la interfaz.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub kind: Kind,
    /// Índice dentro de `openvpn_profiles` o `forti_profiles`.
    pub cfg_index: usize,
    /// Ruta del `.ovpn` o `host:puerto`.
    pub target: String,
    pub state: State,
    pub since: Option<Instant>,
    pub session_path: Option<String>,
    pub pid: Option<u32>,
    pub device: Option<String>,
    pub addr: Option<String>,
    pub stats: Stats,
    pub rate_in: VecDeque<u64>,
    pub rate_out: VecDeque<u64>,
    /// La conexión ya existía cuando arrancó la TUI (o la creó otro proceso).
    pub adopted: bool,
    pub last_error: Option<String>,
    prev_sample: Option<(Stats, Instant)>,
}

impl Profile {
    fn new(name: String, kind: Kind, cfg_index: usize, target: String) -> Self {
        Self {
            name,
            kind,
            cfg_index,
            target,
            state: State::Disconnected,
            since: None,
            session_path: None,
            pid: None,
            device: None,
            addr: None,
            stats: Stats::default(),
            rate_in: VecDeque::new(),
            rate_out: VecDeque::new(),
            adopted: false,
            last_error: None,
            prev_sample: None,
        }
    }

    /// Tiempo transcurrido en el estado actual.
    pub fn elapsed(&self) -> Option<Duration> {
        self.since.map(|t| t.elapsed())
    }

    pub fn rate_summary(&self) -> String {
        let last = |q: &VecDeque<u64>| q.back().copied().unwrap_or(0);
        format!(
            "↓ {}/s   ↑ {}/s",
            human_bytes(last(&self.rate_in)),
            human_bytes(last(&self.rate_out))
        )
    }

    fn enter(&mut self, state: State) {
        if self.state != state {
            self.since = Some(Instant::now());
        }
        self.state = state;
    }

    fn reset_link(&mut self) {
        self.session_path = None;
        self.pid = None;
        self.device = None;
        self.addr = None;
        self.stats = Stats::default();
        self.prev_sample = None;
        self.rate_in.clear();
        self.rate_out.clear();
        self.adopted = false;
    }

    /// Registra una muestra de tráfico y actualiza las series de velocidad.
    fn sample(&mut self, stats: Stats) {
        let now = Instant::now();
        if let Some((prev, at)) = self.prev_sample {
            let secs = now.duration_since(at).as_secs_f64().max(0.001);
            let rate = |new: u64, old: u64| ((new.saturating_sub(old)) as f64 / secs) as u64;
            push_capped(&mut self.rate_in, rate(stats.bytes_in, prev.bytes_in));
            push_capped(&mut self.rate_out, rate(stats.bytes_out, prev.bytes_out));
        }
        self.prev_sample = Some((stats, now));
        self.stats = stats;
    }
}

fn push_capped(q: &mut VecDeque<u64>, value: u64) {
    if q.len() >= RATE_HISTORY {
        q.pop_front();
    }
    q.push_back(value);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Profiles,
    Logs,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: chrono::DateTime<chrono::Local>,
    pub level: Level,
    pub profile: Option<String>,
    pub text: String,
}

pub struct App {
    pub db: Db,
    pub general: General,
    pub openvpn_profiles: Vec<OpenVpnProfile>,
    pub forti_profiles: Vec<FortiProfile>,

    pub screen: Screen,
    pub profiles: Vec<Profile>,
    pub selected: usize,
    pub logs: VecDeque<LogEntry>,
    /// `None` = seguir el final del log; `Some(n)` = desplazamiento fijado.
    pub log_scroll: Option<usize>,
    /// Alto útil del panel de registro en el último dibujado.
    pub log_viewport: usize,
    pub focus: Focus,
    pub show_help: bool,
    pub should_quit: bool,
    pub ticks: u64,
    pub toast: Option<(Level, String, Instant)>,
    pub interfaces: Vec<Iface>,
    pub ipv6_disabled: Option<bool>,
    pub openvpn_available: bool,
    pub fortivpn_available: bool,

    // --- config: menú y subpantallas -----------------------------------
    pub config_menu_selected: usize,
    pub profile_list_selected: usize,
    pub profile_form: Option<ProfileForm>,
    pub general_form: Option<GeneralForm>,
    pub file_explorer: Option<FileExplorer>,
    pub file_target: Option<FileTarget>,
    pub all_openvpn_sessions: Vec<openvpn3::Session>,
    pub sessions_selected: usize,

    // --- tailscale --------------------------------------------------------
    pub tailscale_available: bool,
    pub tailscale_status: Option<tailscale::Status>,
    pub exit_node_selected: usize,
    pub routes_input: Input,

    tx: Sender<Msg>,
}

impl App {
    pub fn new(db: Db, tx: Sender<Msg>) -> Self {
        let general = db.general().unwrap_or_default();
        let openvpn_profiles = db.openvpn_profiles().unwrap_or_default();
        let forti_profiles = db.forti_profiles().unwrap_or_default();
        let profiles = build_profiles(&openvpn_profiles, &forti_profiles);

        let mut app = Self {
            db,
            general,
            openvpn_profiles,
            forti_profiles,
            screen: Screen::Dashboard,
            profiles,
            selected: 0,
            logs: VecDeque::new(),
            log_scroll: None,
            log_viewport: 10,
            focus: Focus::Profiles,
            show_help: false,
            should_quit: false,
            ticks: 0,
            toast: None,
            interfaces: Vec::new(),
            ipv6_disabled: None,
            openvpn_available: true,
            fortivpn_available: true,
            config_menu_selected: 0,
            profile_list_selected: 0,
            profile_form: None,
            general_form: None,
            file_explorer: None,
            file_target: None,
            all_openvpn_sessions: Vec::new(),
            sessions_selected: 0,
            tailscale_available: tailscale::is_available(),
            tailscale_status: None,
            exit_node_selected: 0,
            routes_input: Input::default(),
            tx,
        };
        let source = app.db.source.display().to_string();
        app.log(None, Level::Info, format!("base de datos: {source}"));
        app
    }

    // ---------------------------------------------------------------- logging

    pub fn log(&mut self, idx: Option<usize>, level: Level, text: impl Into<String>) {
        let profile = idx
            .and_then(|i| self.profiles.get(i))
            .map(|p| p.name.clone());
        if self.logs.len() >= self.general.log_capacity {
            self.logs.pop_front();
        }
        self.logs.push_back(LogEntry {
            at: chrono::Local::now(),
            level,
            profile,
            text: text.into(),
        });
    }

    pub fn toast(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        self.log(None, level, text.clone());
        self.toast = Some((level, text, Instant::now()));
    }

    /// Aviso visible actualmente, si no ha caducado.
    pub fn active_toast(&self) -> Option<(Level, &str)> {
        self.toast
            .as_ref()
            .filter(|(_, _, at)| at.elapsed() < TOAST_TTL)
            .map(|(lvl, text, _)| (*lvl, text.as_str()))
    }

    // ------------------------------------------------------------- navegación

    /// Índices de `self.profiles` visibles — el panel "VPNS" siempre muestra
    /// los tres tipos mezclados, así que esto es simplemente todos.
    pub fn visible_profile_indices(&self) -> Vec<usize> {
        (0..self.profiles.len()).collect()
    }

    /// Mueve la selección al primer perfil de `kind` (o a la fila de
    /// Tailscale). Usado por los atajos de teclado `1`/`2`/`3` y por
    /// `vpn 1`/`vpn 2`/`vpn 3` para saltar directo a un tipo sin tocar la
    /// navegación normal de flechas.
    pub fn select_kind(&mut self, kind: Kind) {
        if let Some(pos) = self.profiles.iter().position(|p| p.kind == kind) {
            self.selected = pos;
        }
    }

    pub fn select_next(&mut self) {
        let n = self.visible_profile_indices().len();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
        }
    }

    pub fn select_prev(&mut self) {
        let n = self.visible_profile_indices().len();
        if n > 0 {
            self.selected = (self.selected + n - 1) % n;
        }
    }

    /// Índice absoluto (en `self.profiles`) del elemento seleccionado.
    pub fn selected_absolute(&self) -> Option<usize> {
        self.visible_profile_indices().get(self.selected).copied()
    }

    pub fn selected_profile(&self) -> Option<&Profile> {
        self.selected_absolute().and_then(|i| self.profiles.get(i))
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Profiles => Focus::Logs,
            Focus::Logs => Focus::Profiles,
        };
    }

    pub fn scroll_logs(&mut self, delta: i32) {
        let viewport = self.log_viewport.max(1);
        let max = self.logs.len().saturating_sub(viewport);
        let current = self.log_scroll.unwrap_or(max) as i64;
        let next = (current + delta as i64).clamp(0, max as i64) as usize;
        self.log_scroll = if next >= max { None } else { Some(next) };
    }

    pub fn follow_logs(&mut self) {
        self.log_scroll = None;
    }

    pub fn log_top(&mut self) {
        self.log_scroll = Some(0);
    }

    // ----------------------------------------------------------------- menús

    pub fn enter_config_menu(&mut self) {
        self.screen = Screen::Config(ConfigScreen::Menu);
        self.config_menu_selected = 0;
    }

    pub fn back_to_dashboard(&mut self) {
        self.screen = Screen::Dashboard;
        self.profile_form = None;
        self.general_form = None;
        self.file_explorer = None;
        self.file_target = None;
    }

    /// Abre el selector de exit-node de Tailscale (tecla `e` con la fila de
    /// Tailscale seleccionada en el dashboard).
    pub fn enter_exit_node_picker(&mut self) {
        self.exit_node_selected = 0;
        self.screen = Screen::Tailscale(TailscaleScreen::ExitNodePicker);
    }

    /// Abre el formulario de rutas anunciadas de Tailscale (tecla `R` con la
    /// fila de Tailscale seleccionada en el dashboard).
    pub fn enter_routes_form(&mut self) {
        let current = self
            .tailscale_status
            .as_ref()
            .map(|s| s.advertise_routes.join(","))
            .unwrap_or_default();
        self.routes_input = Input::new(current);
        self.screen = Screen::Tailscale(TailscaleScreen::RoutesForm);
    }

    /// Si hay exactamente un perfil de `kind`, lo conecta de inmediato — usado
    /// por `vpn 1` / `vpn 2` para saltar el menú principal.
    pub fn autoconnect_single(&mut self, kind: Kind) {
        let matches: Vec<usize> = self
            .profiles
            .iter()
            .enumerate()
            .filter(|(_, p)| p.kind == kind)
            .map(|(i, _)| i)
            .collect();
        if let [idx] = matches[..] {
            self.connect(idx);
        }
    }

    // ------------------------------------------------------------- perfiles

    /// Reconstruye `self.profiles` desde la base de datos tras un alta/edición
    /// /borrado. El estado de conexión en curso se pierde para el perfil
    /// tocado, pero el siguiente sondeo (a lo más `poll_interval_ms`) vuelve a
    /// adoptarlo si de verdad sigue conectado.
    pub fn reload_profiles(&mut self) {
        self.openvpn_profiles = self.db.openvpn_profiles().unwrap_or_default();
        self.forti_profiles = self.db.forti_profiles().unwrap_or_default();
        self.profiles = build_profiles(&self.openvpn_profiles, &self.forti_profiles);
        self.selected = 0;
    }

    pub fn open_profile_form_new(&mut self, kind: Kind) {
        self.profile_form = Some(match kind {
            Kind::OpenVpn => ProfileForm::new_openvpn(None),
            Kind::Forti => ProfileForm::new_forti(None),
            Kind::Tailscale => unreachable!("Tailscale no tiene ProfileForm"),
        });
        self.screen = Screen::Config(ConfigScreen::ProfileForm);
    }

    pub fn open_profile_form_edit(&mut self, kind: Kind, list_idx: usize) {
        let form = match kind {
            Kind::OpenVpn => self
                .openvpn_profiles
                .get(list_idx)
                .map(|p| ProfileForm::new_openvpn(Some(p))),
            Kind::Forti => self
                .forti_profiles
                .get(list_idx)
                .map(|p| ProfileForm::new_forti(Some(p))),
            // Tailscale no tiene "perfiles": el menú de config nunca llama
            // aquí con este `Kind`.
            Kind::Tailscale => None,
        };
        if let Some(form) = form {
            self.profile_form = Some(form);
            self.screen = Screen::Config(ConfigScreen::ProfileForm);
        }
    }

    pub fn delete_profile(&mut self, kind: Kind, list_idx: usize) {
        let result = match kind {
            Kind::OpenVpn => self
                .openvpn_profiles
                .get(list_idx)
                .map(|p| self.db.delete_openvpn_profile(p.id)),
            Kind::Forti => self
                .forti_profiles
                .get(list_idx)
                .map(|p| self.db.delete_forti_profile(p.id)),
            Kind::Tailscale => None,
        };
        match result {
            Some(Ok(())) => {
                self.toast(Level::Ok, "perfil borrado");
                self.reload_profiles();
            }
            Some(Err(err)) => self.toast(Level::Error, format!("no pude borrar: {err}")),
            None => {}
        }
    }

    /// Guarda el `ProfileForm` abierto. `Ok(true)` si guardó y debe cerrarse.
    pub fn save_profile_form(&mut self) -> bool {
        let Some(form) = self.profile_form.as_mut() else {
            return true;
        };
        let value = |i: usize| form.fields[i].input.value().trim().to_string();

        let result = match form.kind {
            Kind::OpenVpn => {
                let name = value(0);
                let path = value(1);
                if name.is_empty() || path.is_empty() {
                    form.error = Some("nombre y ruta son obligatorios".into());
                    return false;
                }
                let config = PathBuf::from(&path);
                if !config.is_file() {
                    form.error = Some(format!("no encuentro el archivo {path}"));
                    return false;
                }
                match form.editing_id {
                    Some(id) => self.db.update_openvpn_profile(id, &name, &config),
                    None => self.db.insert_openvpn_profile(&name, &config).map(|_| ()),
                }
            }
            Kind::Forti => {
                let name = value(0);
                let host = value(1);
                let username = value(2);
                let password = value(3);
                let password_env = value(4);
                let trusted_cert = value(5);
                let extra_args: Vec<String> = value(6)
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                if name.is_empty() || host.is_empty() || username.is_empty() {
                    form.error = Some("nombre, host y usuario son obligatorios".into());
                    return false;
                }
                if password.is_empty() && password_env.is_empty() {
                    form.error = Some("define `password` o `password_env`".into());
                    return false;
                }
                let password = (!password.is_empty()).then_some(password.as_str());
                let password_env = (!password_env.is_empty()).then_some(password_env.as_str());
                let trusted_cert = (!trusted_cert.is_empty()).then_some(trusted_cert.as_str());
                match form.editing_id {
                    Some(id) => self.db.update_forti_profile(
                        id,
                        &name,
                        &host,
                        &username,
                        password,
                        password_env,
                        trusted_cert,
                        &extra_args,
                    ),
                    None => self
                        .db
                        .insert_forti_profile(
                            &name,
                            &host,
                            &username,
                            password,
                            password_env,
                            trusted_cert,
                            &extra_args,
                        )
                        .map(|_| ()),
                }
            }
            // El formulario de perfiles solo se abre para OpenVpn/Forti.
            Kind::Tailscale => unreachable!("Tailscale no tiene ProfileForm"),
        };

        match result {
            Ok(()) => {
                self.reload_profiles();
                self.profile_form = None;
                self.toast(Level::Ok, "perfil guardado");
                true
            }
            Err(err) => {
                if let Some(form) = self.profile_form.as_mut() {
                    form.error = Some(err.to_string());
                }
                false
            }
        }
    }

    /// Abre el explorador de archivos apuntando a `start` (o `~`), recordando
    /// a qué campo debe volver la ruta elegida.
    pub fn open_file_picker(&mut self, target: FileTarget, start: Option<&std::path::Path>) {
        let mut explorer = match FileExplorer::new() {
            Ok(e) => e,
            Err(err) => {
                self.toast(Level::Error, format!("no pude abrir el explorador: {err}"));
                return;
            }
        };
        if let Some(start) = start.filter(|p| p.is_dir()) {
            let _ = explorer.set_cwd(start);
        }
        self.file_explorer = Some(explorer);
        self.file_target = Some(target);
        self.screen = Screen::Config(ConfigScreen::FilePicker);
    }

    /// Aplica la ruta elegida en el `FilePicker` al campo pendiente y regresa.
    pub fn apply_picked_file(&mut self, path: PathBuf) {
        match self.file_target {
            Some(FileTarget::Profile(idx)) => {
                if let Some(form) = self.profile_form.as_mut() {
                    if let Some(field) = form.fields.get_mut(idx) {
                        field.input = Input::new(path.to_string_lossy().into_owned());
                    }
                }
                self.screen = Screen::Config(ConfigScreen::ProfileForm);
            }
            Some(FileTarget::General(idx)) => {
                if let Some(form) = self.general_form.as_mut() {
                    let value = path.to_string_lossy().into_owned();
                    match idx {
                        0 => form.ipv6_disable_script = Input::new(value),
                        1 => form.ipv6_enable_script = Input::new(value),
                        _ => {}
                    }
                }
                self.screen = Screen::Config(ConfigScreen::General);
            }
            None => {}
        }
        self.file_explorer = None;
        self.file_target = None;
    }

    pub fn cancel_file_picker(&mut self) {
        self.file_explorer = None;
        let back = match self.file_target.take() {
            Some(FileTarget::Profile(_)) => ConfigScreen::ProfileForm,
            Some(FileTarget::General(_)) | None => ConfigScreen::General,
        };
        self.screen = Screen::Config(back);
    }

    // ------------------------------------------------------- general (config)

    pub fn open_general_form(&mut self) {
        self.general_form = Some(GeneralForm::from(&self.general));
        self.screen = Screen::Config(ConfigScreen::General);
    }

    pub fn save_general_form(&mut self) {
        let Some(form) = self.general_form.take() else {
            return;
        };
        let poll = form
            .poll_interval_ms
            .value()
            .trim()
            .parse::<u64>()
            .unwrap_or(self.general.poll_interval_ms)
            .max(250);
        let cap = form
            .log_capacity
            .value()
            .trim()
            .parse::<usize>()
            .unwrap_or(self.general.log_capacity)
            .max(100);
        let disable_script = form.ipv6_disable_script.value().trim().to_string();
        let enable_script = form.ipv6_enable_script.value().trim().to_string();

        let _ = self.db.set_setting("poll_interval_ms", &poll.to_string());
        let _ = self.db.set_setting("log_capacity", &cap.to_string());
        let _ = self.db.set_setting("ipv6_disable_script", &disable_script);
        let _ = self.db.set_setting("ipv6_enable_script", &enable_script);
        let _ = self
            .db
            .set_setting("ipv6_on_connect", &form.ipv6_on_connect.to_string());

        self.general = self.db.general().unwrap_or(self.general.clone());
        self.toast(Level::Ok, "configuración guardada");
        self.screen = Screen::Config(ConfigScreen::Menu);
    }

    // ----------------------------------------------------------------- acciones

    /// Conecta o desconecta el perfil seleccionado según su estado.
    pub fn toggle(&mut self, idx: usize) {
        let Some(profile) = self.profiles.get(idx) else {
            return;
        };
        match profile.state {
            State::Connected => self.disconnect(idx),
            State::Disconnected | State::Failed(_) => self.connect(idx),
            _ => self.toast(
                Level::Warn,
                format!("'{}' está {}", profile.name, profile.state.label()),
            ),
        }
    }

    pub fn connect(&mut self, idx: usize) {
        let Some(profile) = self.profiles.get(idx) else {
            return;
        };
        if profile.state.is_busy() {
            return;
        }
        if profile.state.is_up() {
            let name = profile.name.clone();
            self.toast(Level::Info, format!("'{name}' ya está conectado"));
            return;
        }

        let kind = profile.kind;
        let cfg_index = profile.cfg_index;
        let name = profile.name.clone();
        self.profiles[idx].last_error = None;
        self.profiles[idx].enter(State::Connecting);
        self.log(Some(idx), Level::Info, format!("conectando '{name}'…"));

        let tx = self.tx.clone();
        match kind {
            Kind::OpenVpn => {
                let Some(config) = self.openvpn_profiles.get(cfg_index).map(|p| p.config.clone())
                else {
                    return;
                };
                std::thread::spawn(move || match openvpn3::connect(&config) {
                    Ok(session) => {
                        let _ = tx.send(Msg::OvpnConnected { idx, session });
                    }
                    Err(err) => {
                        let _ = tx.send(Msg::OvpnFailed {
                            idx,
                            err: err.to_string(),
                        });
                    }
                });
            }
            Kind::Forti => {
                let Some(profile_cfg) = self.forti_profiles.get(cfg_index).cloned() else {
                    return;
                };
                std::thread::spawn(
                    move || match fortivpn::spawn(idx, &profile_cfg, tx.clone()) {
                        Ok(pid) => {
                            let _ = tx.send(Msg::FortiSpawned { idx, pid });
                        }
                        Err(err) => {
                            let _ = tx.send(Msg::FortiFailed {
                                idx,
                                err: err.to_string(),
                            });
                        }
                    },
                );
            }
            Kind::Tailscale => {
                // No hay un mensaje de "conectado" dedicado: `reconcile()`
                // sincroniza el estado real de Tailscale en cada sondeo, así
                // que basta con reportar aquí si el comando en sí falló.
                std::thread::spawn(move || {
                    if let Err(err) = tailscale::up() {
                        let _ = tx.send(Msg::Log(Some(idx), Level::Error, format!("tailscale: {err}")));
                    }
                });
            }
        }
    }

    pub fn disconnect(&mut self, idx: usize) {
        let Some(profile) = self.profiles.get(idx) else {
            return;
        };
        if profile.state.is_busy() {
            return;
        }
        if !profile.state.is_up() {
            let name = profile.name.clone();
            self.toast(Level::Info, format!("'{name}' no está conectado"));
            return;
        }

        let name = profile.name.clone();
        let kind = profile.kind;
        let session = profile.session_path.clone();
        let pid = profile.pid;
        self.profiles[idx].enter(State::Disconnecting);
        self.log(Some(idx), Level::Info, format!("desconectando '{name}'…"));

        let tx = self.tx.clone();
        match kind {
            Kind::OpenVpn => {
                let Some(session) = session else {
                    self.profiles[idx].enter(State::Disconnected);
                    self.profiles[idx].reset_link();
                    return;
                };
                std::thread::spawn(move || match openvpn3::disconnect(&session) {
                    Ok(()) => {
                        let _ = tx.send(Msg::OvpnDisconnected { idx });
                    }
                    Err(err) => {
                        let _ = tx.send(Msg::OvpnFailed {
                            idx,
                            err: err.to_string(),
                        });
                    }
                });
            }
            Kind::Forti => {
                let Some(pid) = pid else {
                    self.profiles[idx].enter(State::Disconnected);
                    self.profiles[idx].reset_link();
                    return;
                };
                std::thread::spawn(move || {
                    // SIGINT es la salida ordenada de openfortivpn; si no cede,
                    // se escala tras unos segundos.
                    match fortivpn::signal(pid, "INT") {
                        Ok(how) => {
                            let _ = tx.send(Msg::Log(
                                Some(idx),
                                Level::Info,
                                format!("SIGINT enviado ({how})"),
                            ));
                        }
                        Err(err) => {
                            let _ = tx.send(Msg::FortiFailed {
                                idx,
                                err: err.to_string(),
                            });
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_secs(10));
                    if fortivpn::running_pids().is_empty() {
                        return;
                    }
                    if let Ok(how) = fortivpn::signal(pid, "TERM") {
                        let _ = tx.send(Msg::Log(
                            Some(idx),
                            Level::Warn,
                            format!("no cedió a SIGINT; SIGTERM enviado ({how})"),
                        ));
                    }
                });
            }
            Kind::Tailscale => {
                std::thread::spawn(move || {
                    if let Err(err) = tailscale::down() {
                        let _ = tx.send(Msg::Log(Some(idx), Level::Error, format!("tailscale: {err}")));
                    }
                });
            }
        }
    }

    /// Conecta todos los perfiles (incluida la fila de Tailscale).
    pub fn connect_all(&mut self) {
        let targets: Vec<usize> = (0..self.profiles.len())
            .filter(|i| !self.profiles[*i].state.is_up() && !self.profiles[*i].state.is_busy())
            .collect();
        if targets.is_empty() {
            self.toast(Level::Info, "no hay nada que conectar");
            return;
        }
        self.toast(
            Level::Info,
            format!("conectando {} perfil(es)…", targets.len()),
        );
        for idx in targets {
            self.connect(idx);
        }
    }

    /// Desconecta todos los perfiles (incluida la fila de Tailscale).
    pub fn disconnect_all(&mut self) {
        let targets: Vec<usize> = (0..self.profiles.len())
            .filter(|i| self.profiles[*i].state.is_up())
            .collect();
        if targets.is_empty() {
            self.toast(Level::Info, "no hay conexiones activas");
            return;
        }
        self.toast(
            Level::Info,
            format!("desconectando {} perfil(es)…", targets.len()),
        );
        for idx in targets {
            self.disconnect(idx);
        }
    }

    /// Cierra todo de forma síncrona antes de salir.
    pub fn shutdown(&mut self) {
        for idx in 0..self.profiles.len() {
            let profile = &self.profiles[idx];
            if !profile.state.is_up() || profile.adopted {
                continue;
            }
            match (profile.kind, profile.session_path.clone(), profile.pid) {
                (Kind::OpenVpn, Some(session), _) => {
                    let _ = openvpn3::disconnect(&session);
                }
                (Kind::Forti, _, Some(pid)) => {
                    let _ = fortivpn::signal(pid, "INT");
                }
                _ => {}
            }
        }
    }

    /// Ejecuta el script de deshabilitar-IPv6 configurado (compatibilidad con
    /// `ipv6_on_connect`). Para alternar en ambas direcciones ver
    /// [`App::toggle_ipv6`].
    pub fn run_ipv6_script(&mut self) {
        self.run_ipv6_action(false);
    }

    /// Alterna IPv6: si está deshabilitado, corre `ipv6_enable_script`; si no,
    /// `ipv6_disable_script`. Ambos scripts ya existen en `../vpns/` y no se
    /// tocan desde aquí.
    pub fn toggle_ipv6(&mut self) {
        let disable = !self.ipv6_disabled.unwrap_or(false);
        self.run_ipv6_action(disable);
    }

    fn run_ipv6_action(&mut self, disable: bool) {
        let script = if disable {
            self.general.ipv6_disable_script.clone()
        } else {
            self.general.ipv6_enable_script.clone()
        };
        let Some(script) = script else {
            self.toast(
                Level::Warn,
                format!(
                    "no hay script para {} IPv6 configurado (menú de config → General)",
                    if disable { "deshabilitar" } else { "habilitar" }
                ),
            );
            return;
        };
        if !script.is_file() {
            self.toast(
                Level::Error,
                format!("no encuentro el script {}", script.display()),
            );
            return;
        }
        self.log(
            None,
            Level::Info,
            format!("ejecutando {} …", script.display()),
        );
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let path = script.to_string_lossy().into_owned();
            let result = vpn::run_checked("sudo", &["-n", &path], Duration::from_secs(20));
            let msg = match result {
                Ok(_) => Msg::Log(None, Level::Ok, "script IPv6 ejecutado".into()),
                Err(err) => Msg::Log(None, Level::Error, format!("script IPv6: {err}")),
            };
            let _ = tx.send(msg);
        });
    }

    // ------------------------------------------------------- sesiones openvpn3

    pub fn kill_selected_session(&mut self) {
        let Some(session) = self.all_openvpn_sessions.get(self.sessions_selected).cloned() else {
            return;
        };
        self.toast(Level::Info, format!("cerrando sesión {}…", short(&session.path)));
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let msg = match openvpn3::disconnect(&session.path) {
                Ok(()) => Msg::Log(None, Level::Ok, "sesión cerrada".into()),
                Err(err) => Msg::Log(None, Level::Error, format!("no pude cerrar la sesión: {err}")),
            };
            let _ = tx.send(msg);
        });
    }

    pub fn kill_all_sessions(&mut self) {
        if self.all_openvpn_sessions.is_empty() {
            self.toast(Level::Info, "no hay sesiones openvpn3 activas");
            return;
        }
        let sessions = self.all_openvpn_sessions.clone();
        self.toast(Level::Info, format!("cerrando {} sesión(es)…", sessions.len()));
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let msg = match openvpn3::disconnect_all(&sessions) {
                Ok(()) => Msg::Log(None, Level::Ok, "todas las sesiones cerradas".into()),
                Err(err) => Msg::Log(None, Level::Error, format!("alguna sesión no cerró: {err}")),
            };
            let _ = tx.send(msg);
        });
    }

    // ------------------------------------------------------------- tailscale

    pub fn tailscale_toggle(&mut self) {
        let running = self
            .tailscale_status
            .as_ref()
            .map(tailscale::Status::is_running)
            .unwrap_or(false);
        self.toast(
            Level::Info,
            if running { "desconectando tailscale…" } else { "conectando tailscale…" },
        );
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = if running { tailscale::down() } else { tailscale::up() };
            let msg = match result {
                Ok(()) => Msg::Log(None, Level::Ok, "tailscale: hecho".into()),
                Err(err) => Msg::Log(None, Level::Error, format!("tailscale: {err}")),
            };
            let _ = tx.send(msg);
        });
    }

    /// Dispara `tailscale up` solo si el backend no está ya `Running` — usado
    /// por `vpn 3` para saltar el menú sin reconectar de más.
    ///
    /// Al arrancar, `self.tailscale_status` todavía puede estar en `None`
    /// (el primer sondeo periódico aún no llegó), así que aquí se consulta
    /// el estado de una vez de forma síncrona en vez de asumir "no
    /// conectado" — de lo contrario se dispararía un `up` innecesario cada
    /// vez que ya estaba arriba.
    pub fn tailscale_autoconnect(&mut self) {
        let running = match &self.tailscale_status {
            Some(status) => status.is_running(),
            None => tailscale::status().map(|s| s.is_running()).unwrap_or(false),
        };
        if !running {
            self.tailscale_toggle();
        }
    }

    pub fn tailscale_apply_exit_node(&mut self, node_ip: Option<String>) {
        self.toast(Level::Info, "aplicando exit-node…");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let msg = match tailscale::set_exit_node(node_ip.as_deref()) {
                Ok(()) => Msg::Log(None, Level::Ok, "exit-node actualizado".into()),
                Err(err) => Msg::Log(None, Level::Error, format!("exit-node: {err}")),
            };
            let _ = tx.send(msg);
        });
    }

    pub fn tailscale_apply_routes(&mut self) {
        let routes: Vec<String> = self
            .routes_input
            .value()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        self.toast(Level::Info, "aplicando rutas anunciadas…");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let msg = match tailscale::set_advertise_routes(&routes) {
                Ok(()) => Msg::Log(
                    None,
                    Level::Ok,
                    "rutas actualizadas (falta aprobación del admin del tailnet)".into(),
                ),
                Err(err) => Msg::Log(None, Level::Error, format!("rutas: {err}")),
            };
            let _ = tx.send(msg);
        });
        self.screen = Screen::Dashboard;
    }

    // ------------------------------------------------------------- mensajería

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Input(_) => {}
            Msg::Tick => {
                self.ticks = self.ticks.wrapping_add(1);
            }
            Msg::Log(idx, level, text) => self.log(idx, level, text),

            Msg::OvpnConnected { idx, session } => {
                if let Some(p) = self.profiles.get_mut(idx) {
                    p.session_path = Some(session.clone());
                    p.adopted = false;
                    p.enter(State::Connected);
                    let name = p.name.clone();
                    self.log(
                        Some(idx),
                        Level::Ok,
                        format!("'{name}' conectado — sesión {session}"),
                    );
                    self.after_connect();
                }
            }
            Msg::OvpnDisconnected { idx } => {
                if let Some(p) = self.profiles.get_mut(idx) {
                    p.enter(State::Disconnected);
                    p.reset_link();
                    let name = p.name.clone();
                    self.log(Some(idx), Level::Ok, format!("'{name}' desconectado"));
                }
            }
            Msg::OvpnFailed { idx, err } => self.fail(idx, err),

            Msg::FortiSpawned { idx, pid } => {
                if let Some(p) = self.profiles.get_mut(idx) {
                    p.pid = Some(pid);
                    p.adopted = false;
                    let name = p.name.clone();
                    self.log(
                        Some(idx),
                        Level::Info,
                        format!("'{name}': openfortivpn arrancado (PID {pid})"),
                    );
                }
            }
            Msg::FortiUp { idx } => {
                if let Some(p) = self.profiles.get_mut(idx) {
                    p.enter(State::Connected);
                    let name = p.name.clone();
                    self.log(Some(idx), Level::Ok, format!("'{name}': túnel arriba"));
                    self.after_connect();
                }
            }
            Msg::FortiExited { idx, code } => {
                let Some(p) = self.profiles.get_mut(idx) else {
                    return;
                };
                let was_up = p.state.is_up();
                let name = p.name.clone();
                p.reset_link();
                if was_up || matches!(p.state, State::Disconnecting) {
                    p.enter(State::Disconnected);
                    self.log(Some(idx), Level::Ok, format!("'{name}' desconectado"));
                } else {
                    let reason = fortivpn::exit_reason(code);
                    p.enter(State::Failed(reason.clone()));
                    p.last_error = Some(reason.clone());
                    self.toast(Level::Error, format!("'{name}': {reason}"));
                }
            }
            Msg::FortiFailed { idx, err } => self.fail(idx, err),

            Msg::Poll(snapshot) => self.reconcile(*snapshot),
        }
    }

    fn fail(&mut self, idx: usize, err: String) {
        let Some(p) = self.profiles.get_mut(idx) else {
            return;
        };
        let name = p.name.clone();
        p.reset_link();
        p.last_error = Some(err.clone());
        p.enter(State::Failed(err.clone()));
        self.toast(Level::Error, format!("'{name}': {err}"));
    }

    fn after_connect(&mut self) {
        if self.general.ipv6_on_connect {
            self.run_ipv6_script();
        }
    }

    /// Ajusta el estado interno a lo que dice el sistema.
    ///
    /// Los perfiles en transición se dejan en paz: la verdad la traerá el
    /// mensaje del hilo que está haciendo el trabajo.
    fn reconcile(&mut self, snap: Snapshot) {
        self.openvpn_available = snap.openvpn_available;
        self.fortivpn_available = snap.fortivpn_available;
        self.interfaces = snap.interfaces;
        self.ipv6_disabled = snap.ipv6_disabled;
        self.all_openvpn_sessions = snap.sessions.clone();
        self.tailscale_status = snap.tailscale;

        // --- OpenVPN3 --------------------------------------------------------
        for idx in 0..self.profiles.len() {
            if self.profiles[idx].kind != Kind::OpenVpn || self.profiles[idx].state.is_busy() {
                continue;
            }
            let Some(config) = self
                .openvpn_profiles
                .get(self.profiles[idx].cfg_index)
                .map(|p| p.config.clone())
            else {
                continue;
            };
            let known = self.profiles[idx].session_path.clone();

            let session = snap.sessions.iter().find(|s| {
                known.as_deref() == Some(s.path.as_str()) || openvpn3::matches_config(s, &config)
            });

            match session {
                Some(session) if session.is_connected() => {
                    let adopted = known.is_none();
                    let p = &mut self.profiles[idx];
                    p.session_path = Some(session.path.clone());
                    p.device = session.device.clone();
                    if adopted {
                        p.adopted = true;
                        p.enter(State::Connected);
                        let name = p.name.clone();
                        self.log(
                            Some(idx),
                            Level::Info,
                            format!("'{name}': sesión existente adoptada"),
                        );
                    } else if !p.state.is_up() {
                        p.enter(State::Connected);
                    }
                    if let Some((_, stats)) =
                        snap.stats.iter().find(|(path, _)| path == &session.path)
                    {
                        self.profiles[idx].sample(*stats);
                    }
                    let addr = self.addr_for(self.profiles[idx].device.as_deref());
                    self.profiles[idx].addr = addr;
                }
                _ if known.is_some() => {
                    let p = &mut self.profiles[idx];
                    p.reset_link();
                    p.enter(State::Disconnected);
                    let name = p.name.clone();
                    self.log(
                        Some(idx),
                        Level::Warn,
                        format!("'{name}': la sesión ya no existe"),
                    );
                }
                _ => {}
            }
        }

        // --- OpenFortiVPN ----------------------------------------------------
        // Los procesos que gestionamos se controlan por su hilo vigilante; aquí
        // sólo se adoptan (o se olvidan) los externos.
        let managed: Vec<u32> = self
            .profiles
            .iter()
            .filter(|p| p.kind == Kind::Forti && !p.adopted)
            .filter_map(|p| p.pid)
            .collect();
        let mut unclaimed: Vec<u32> = snap
            .forti_pids
            .iter()
            .copied()
            .filter(|pid| !managed.contains(pid))
            .collect();

        for idx in 0..self.profiles.len() {
            if self.profiles[idx].kind != Kind::Forti || self.profiles[idx].state.is_busy() {
                continue;
            }
            let adopted = self.profiles[idx].adopted;
            let pid = self.profiles[idx].pid;

            if adopted {
                match pid.filter(|pid| snap.forti_pids.contains(pid)) {
                    Some(_) => unclaimed.retain(|p| Some(*p) != pid),
                    None => {
                        let p = &mut self.profiles[idx];
                        p.reset_link();
                        p.enter(State::Disconnected);
                        let name = p.name.clone();
                        self.log(
                            Some(idx),
                            Level::Warn,
                            format!("'{name}': el proceso externo terminó"),
                        );
                        continue;
                    }
                }
            } else if pid.is_none() && !unclaimed.is_empty() {
                let pid = unclaimed.remove(0);
                let p = &mut self.profiles[idx];
                p.pid = Some(pid);
                p.adopted = true;
                p.enter(State::Connected);
                let name = p.name.clone();
                self.log(
                    Some(idx),
                    Level::Info,
                    format!("'{name}': proceso externo adoptado (PID {pid})"),
                );
            }

            if self.profiles[idx].state.is_up() {
                let device = self.profiles[idx]
                    .device
                    .clone()
                    .or_else(|| self.free_ppp_device());
                if let Some(iface) = device
                    .as_deref()
                    .and_then(|name| self.interfaces.iter().find(|i| i.name == name))
                {
                    let stats = Stats {
                        bytes_in: iface.rx,
                        bytes_out: iface.tx,
                    };
                    let addr = iface.addr.clone();
                    self.profiles[idx].device = device;
                    self.profiles[idx].addr = Some(addr);
                    self.profiles[idx].sample(stats);
                }
            }
        }

        // --- Tailscale ---------------------------------------------------
        // No hay un flujo de "Connecting"/"Disconnecting" propio: cada sondeo
        // simplemente refleja el `backend_state` real, así que un `up`/`down`
        // en curso se resuelve solo en el siguiente `poll_interval_ms` sin
        // quedar nunca atorado en un estado intermedio.
        if let Some(idx) = self.profiles.iter().position(|p| p.kind == Kind::Tailscale) {
            if let Some(status) = self.tailscale_status.clone() {
                let target_state = if status.is_running() {
                    State::Connected
                } else {
                    State::Disconnected
                };
                self.profiles[idx].enter(target_state);
                self.profiles[idx].target = status
                    .self_ips
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "tailscale".to_string());
            }
        }
    }

    fn addr_for(&self, device: Option<&str>) -> Option<String> {
        let device = device?;
        self.interfaces
            .iter()
            .find(|i| i.name == device)
            .map(|i| i.addr.clone())
    }

    /// Primera interfaz `ppp*` que ningún perfil reclama todavía.
    fn free_ppp_device(&self) -> Option<String> {
        let taken: Vec<&str> = self
            .profiles
            .iter()
            .filter_map(|p| p.device.as_deref())
            .collect();
        self.interfaces
            .iter()
            .find(|i| i.name.starts_with("ppp") && !taken.contains(&i.name.as_str()))
            .map(|i| i.name.clone())
    }

    /// Perfiles conectados / total.
    pub fn summary(&self) -> (usize, usize) {
        (
            self.profiles.iter().filter(|p| p.state.is_up()).count(),
            self.profiles.len(),
        )
    }
}

fn build_profiles(openvpn: &[OpenVpnProfile], forti: &[FortiProfile]) -> Vec<Profile> {
    let mut profiles = Vec::new();
    for (i, p) in openvpn.iter().enumerate() {
        profiles.push(Profile::new(
            p.name.clone(),
            Kind::OpenVpn,
            i,
            p.config.to_string_lossy().into_owned(),
        ));
    }
    for (i, p) in forti.iter().enumerate() {
        profiles.push(Profile::new(p.name.clone(), Kind::Forti, i, p.host.clone()));
    }
    // Tailscale no es una lista de perfiles — es una sola conexión de este
    // equipo — pero vive en el mismo panel "VPNS", así que se representa
    // como una única fila sintética; `cfg_index` no se usa para este `Kind`.
    profiles.push(Profile::new(
        "Tailscale".to_string(),
        Kind::Tailscale,
        0,
        "tailscale".to_string(),
    ));
    profiles
}

/// `/net/openvpn/v3/sessions/abc…` → `…/abc…`, para que quepa en pantalla.
fn short(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((_, id)) if id.len() > 12 => format!("…{}", &id[id.len() - 12..]),
        Some((_, id)) => id.to_string(),
        None => path.to_string(),
    }
}
