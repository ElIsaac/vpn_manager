//! Estado de la aplicación y transiciones.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui_explorer::FileExplorer;
use tui_input::Input;

use crate::db::{Db, FortiProfile, General, OpenVpnProfile};
use crate::filedialog::{self, GuiPicker};
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

    /// Los tres tipos, en el orden en que se muestran (panel VPNS y slider).
    pub const ALL: [Kind; 3] = [Kind::OpenVpn, Kind::Forti, Kind::Tailscale];
}

/// Cómo se nombra una fila del panel VPNS: el tipo manda, y el nombre del
/// perfil solo aparece entre paréntesis cuando el usuario puso uno.
pub fn display_name(kind: Kind, name: Option<&str>) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => format!("{} ({name})", kind.label()),
        None => kind.label().to_string(),
    }
}

/// Texto de la fila cuando ese tipo de VPN no tiene ningún perfil todavía.
pub fn unconfigured_name(kind: Kind) -> String {
    format!("{} No Configurada", kind.label())
}

/// Nombre que se sugiere para un perfil recién importado: el del archivo, sin
/// extensión (`…/thinkpad.ovpn` → `thinkpad`). Solo es una sugerencia: el
/// nombre sigue siendo opcional.
pub fn suggested_profile_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy().trim().to_string();
    (!stem.is_empty()).then_some(stem)
}

/// Regla de nombres: el primer perfil de un tipo puede ir sin nombre (basta el
/// tipo para identificarlo), pero en cuanto hay dos del mismo tipo el nombre
/// es obligatorio o serían indistinguibles en la lista.
///
/// `others` = cuántos perfiles de ese mismo tipo quedarían aparte de éste.
pub fn validate_profile_name(others: usize, name: &str) -> Result<Option<String>, String> {
    let name = name.trim();
    if !name.is_empty() {
        return Ok(Some(name.to_string()));
    }
    if others == 0 {
        Ok(None)
    } else {
        Err("ya hay otro perfil de este tipo: ponle un nombre para distinguirlos".into())
    }
}

/// Interpreta el texto que llega al soltar un archivo sobre la terminal.
///
/// En una TUI no hay drag & drop de verdad: lo que hace el emulador de terminal
/// al soltar un archivo es *pegar la ruta como texto*, y cada uno la escribe a
/// su manera — cruda, entre comillas, con los espacios escapados al estilo
/// shell, o como URI `file://` percent-encoded, y casi siempre con un salto de
/// línea al final. Esto lo deja en una ruta utilizable, o `None` si lo pegado
/// no parece una ruta.
pub fn normalize_dropped_path(text: &str) -> Option<PathBuf> {
    // Soltar varios archivos pega varias líneas: nos quedamos con la primera.
    let mut candidate = text.lines().map(str::trim).find(|l| !l.is_empty())?.to_string();

    // Comillas: dentro de comillas simples el shell no interpreta escapes, así
    // que ahí no hay backslashes que deshacer.
    let mut unescape = true;
    for quote in ['\'', '"'] {
        if candidate.len() >= 2 && candidate.starts_with(quote) && candidate.ends_with(quote) {
            candidate = candidate[1..candidate.len() - 1].to_string();
            unescape = quote != '\'';
            break;
        }
    }

    if let Some(rest) = candidate.strip_prefix("file://") {
        // `file:///ruta` y `file://host/ruta`: en ambos la ruta empieza en la
        // primera `/`. El percent-decoding solo aplica aquí — en una ruta
        // normal un `%` es un `%` y decodificarlo la rompería.
        let path = rest.find('/').map(|i| &rest[i..])?;
        candidate = percent_decode(path);
    } else if unescape {
        candidate = unescape_backslashes(&candidate);
    }

    if candidate.is_empty() {
        return None;
    }
    if let Some(rest) = candidate.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            let home = dirs::home_dir()?;
            return Some(home.join(rest.trim_start_matches('/')));
        }
    }
    // Un archivo soltado siempre trae ruta absoluta; lo demás es texto suelto.
    if candidate.starts_with('/') || candidate.starts_with("./") || candidate.starts_with("../") {
        Some(PathBuf::from(candidate))
    } else {
        None
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Primera línea del texto pegado: un salto de línea dentro de un campo no
/// aporta nada y, sin filtrarlo, el `\n` final de un drop enviaría el
/// formulario.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

fn insert_text(input: &mut Input, text: &str) {
    for c in text.chars() {
        input.handle(tui_input::InputRequest::InsertChar(c));
    }
}

fn unescape_backslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
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
    /// Asistente de primera configuración: un carrusel (`←`/`→`) con un pane
    /// por tipo de VPN. Solo aparece cuando falta configuración — desde una
    /// fila "No Configurada" o al arrancar con la base de datos vacía.
    Setup(Kind),
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
    /// Confirmación antes de borrar el perfil `idx` de la lista de `kind`.
    ConfirmDelete { kind: Kind, idx: usize },
}

/// De dónde se abrió el formulario de perfil, para saber a dónde volver al
/// guardar o cancelar. El mismo formulario se usa desde el menú de config y
/// desde el asistente de primera configuración.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormOrigin {
    ConfigList,
    Setup,
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
    /// Si `true`, este campo tiene una acción de "buscar archivo" (F3) y se
    /// dibuja como zona de importación (dropzone) en vez de como línea suelta.
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

/// En qué paso va el formulario. El alta de OpenVPN3 se hace por pasos —
/// primero el archivo, después el nombre — porque preguntar el nombre antes de
/// saber qué archivo es resulta al revés de como lo piensa quien lo usa.
/// Editar, y FortiVPN entero, siguen mostrando todos los campos juntos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormStep {
    All,
    PickFile,
    Name,
}

/// Formulario de alta/edición de un perfil (OpenVPN3 o FortiVPN).
pub struct ProfileForm {
    pub kind: Kind,
    pub editing_id: Option<i64>,
    pub fields: Vec<FormField>,
    pub focus: usize,
    pub error: Option<String>,
    pub origin: FormOrigin,
    pub step: FormStep,
}

impl ProfileForm {
    /// Índices de campo. El orden es el del recorrido mental: primero lo que
    /// define la conexión, el nombre (opcional) hasta el final.
    pub const OVPN_PATH: usize = 0;
    pub const OVPN_NAME: usize = 1;
    pub const FORTI_NAME: usize = 6;

    pub fn new_openvpn(existing: Option<&OpenVpnProfile>) -> Self {
        let fields = vec![
            FormField::new(
                "archivo .ovpn",
                existing
                    .map(|p| p.config.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
            .with_picker(),
            FormField::new(
                "nombre (opcional)",
                existing.and_then(|p| p.name.clone()).unwrap_or_default(),
            ),
        ];
        // Alta = asistente por pasos; edición = todo a la vista.
        let step = if existing.is_some() {
            FormStep::All
        } else {
            FormStep::PickFile
        };
        Self {
            kind: Kind::OpenVpn,
            editing_id: existing.map(|p| p.id),
            fields,
            focus: Self::OVPN_PATH,
            error: None,
            origin: FormOrigin::ConfigList,
            step,
        }
    }

    pub fn new_forti(existing: Option<&FortiProfile>) -> Self {
        let fields = vec![
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
            FormField::new(
                "nombre (opcional)",
                existing.and_then(|p| p.name.clone()).unwrap_or_default(),
            ),
        ];
        Self {
            kind: Kind::Forti,
            editing_id: existing.map(|p| p.id),
            fields,
            focus: 0,
            error: None,
            origin: FormOrigin::ConfigList,
            step: FormStep::All,
        }
    }

    /// Índice del campo "nombre" según el tipo.
    pub fn name_idx(&self) -> usize {
        match self.kind {
            Kind::Forti => Self::FORTI_NAME,
            _ => Self::OVPN_NAME,
        }
    }

    /// Índice del campo de ruta, si el formulario tiene uno.
    pub fn path_idx(&self) -> Option<usize> {
        self.fields.iter().position(|f| f.pickable)
    }

    /// El nombre del archivo elegido, para sugerirlo como nombre de perfil.
    pub fn suggested_name(&self) -> Option<String> {
        let idx = self.path_idx()?;
        let value = self.fields[idx].input.value();
        (!value.is_empty()).then(|| suggested_profile_name(Path::new(value)))?
    }

    fn opened_from_setup(mut self) -> Self {
        self.origin = FormOrigin::Setup;
        self
    }

    /// Cambia de paso dejando el foco en el único campo visible de ese paso.
    pub fn set_step(&mut self, step: FormStep) {
        self.step = step;
        self.focus = match step {
            FormStep::All => self.focus.min(self.fields.len().saturating_sub(1)),
            FormStep::PickFile => self.path_idx().unwrap_or(0),
            FormStep::Name => self.name_idx(),
        };
        self.error = None;
    }

    /// En los pasos del asistente solo hay un campo visible: `tab` únicamente
    /// tiene sentido con el formulario completo.
    pub fn next_field(&mut self) {
        if self.step == FormStep::All && !self.fields.is_empty() {
            self.focus = (self.focus + 1) % self.fields.len();
        }
    }

    pub fn prev_field(&mut self) {
        if self.step == FormStep::All && !self.fields.is_empty() {
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
    /// Fila "…No Configurada": ese tipo de VPN no tiene ningún perfil todavía,
    /// así que no se puede conectar — `enter` abre el asistente en su lugar.
    pub placeholder: bool,
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
            placeholder: false,
            prev_sample: None,
        }
    }

    fn placeholder(kind: Kind) -> Self {
        let mut p = Self::new(unconfigured_name(kind), kind, 0, String::new());
        p.placeholder = true;
        p
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

/// Paneles del dashboard por los que se mueve el foco con `←`/`→`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Profiles,
    Logs,
    /// El recuadro `⚙ configuración`: `enter` abre el menú de config.
    ConfigButton,
}

impl Focus {
    const ORDER: [Focus; 3] = [Focus::Profiles, Focus::Logs, Focus::ConfigButton];

    pub fn next(self) -> Self {
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        Self::ORDER[(i + 1) % Self::ORDER.len()]
    }

    pub fn prev(self) -> Self {
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        Self::ORDER[(i + Self::ORDER.len() - 1) % Self::ORDER.len()]
    }
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
    /// Explorador gráfico disponible en este equipo (`None` = sin sesión
    /// gráfica o sin zenity/kdialog/yad; se detecta una sola vez al arrancar).
    pub gui_picker: Option<GuiPicker>,
    /// Diálogo gráfico abierto: spinner en pantalla y teclas bloqueadas.
    pub gui_picker_busy: bool,
    pub all_openvpn_sessions: Vec<openvpn3::Session>,
    pub sessions_selected: usize,
    /// Sesiones openvpn3 con un cierre en curso (spinner + teclas bloqueadas).
    pub killing_sessions: HashSet<String>,
    /// "Matar todas" en curso.
    pub killing_all: bool,

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

        // Sin ningún perfil no hay nada que mostrar en el panel: se entra
        // directo al asistente, en el primer pane (OpenVPN3).
        let screen = if openvpn_profiles.is_empty() && forti_profiles.is_empty() {
            Screen::Setup(Kind::OpenVpn)
        } else {
            Screen::Dashboard
        };

        let mut app = Self {
            db,
            general,
            openvpn_profiles,
            forti_profiles,
            screen,
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
            gui_picker: filedialog::detect(),
            gui_picker_busy: false,
            all_openvpn_sessions: Vec::new(),
            sessions_selected: 0,
            killing_sessions: HashSet::new(),
            killing_all: false,
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

    /// `→`: siguiente panel del dashboard.
    pub fn focus_next(&mut self) {
        self.focus = self.focus.next();
    }

    /// `←`: panel anterior.
    pub fn focus_prev(&mut self) {
        self.focus = self.focus.prev();
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

    /// Abre el asistente de primera configuración en el pane de `kind`.
    pub fn enter_setup(&mut self, kind: Kind) {
        self.screen = Screen::Setup(kind);
    }

    /// `←`/`→` dentro del asistente: mueve al pane anterior/siguiente.
    pub fn setup_slide(&mut self, forward: bool) {
        let Screen::Setup(kind) = self.screen else {
            return;
        };
        let i = Kind::ALL.iter().position(|k| *k == kind).unwrap_or(0);
        let n = Kind::ALL.len();
        let next = (if forward { i + 1 } else { i + n - 1 }) % n;
        self.screen = Screen::Setup(Kind::ALL[next]);
    }

    /// `enter` dentro del asistente: entra a configurar el tipo del pane
    /// actual. Es un atajo al MISMO formulario del menú de config; Tailscale
    /// no tiene perfiles, así que ahí lo que procede es levantarlo.
    pub fn setup_confirm(&mut self) {
        let Screen::Setup(kind) = self.screen else {
            return;
        };
        match kind {
            Kind::OpenVpn | Kind::Forti => {
                self.open_profile_form_new(kind);
                if let Some(form) = self.profile_form.take() {
                    self.profile_form = Some(form.opened_from_setup());
                }
            }
            Kind::Tailscale => {
                self.tailscale_toggle();
                self.back_to_dashboard();
            }
        }
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

    /// Texto de la confirmación de borrado: dice exactamente qué se pierde.
    pub fn delete_warning(&self, kind: Kind, list_idx: usize) -> Option<String> {
        match kind {
            Kind::OpenVpn => self.openvpn_profiles.get(list_idx).map(|p| {
                format!(
                    "Se eliminará la configuración guardada de {} y su ruta al archivo {}.\n\
                     El archivo .ovpn en disco NO se borra.",
                    display_name(kind, p.name.as_deref()),
                    p.config.display()
                )
            }),
            Kind::Forti => self.forti_profiles.get(list_idx).map(|p| {
                format!(
                    "Se eliminará la configuración guardada de {} ({}@{}), incluidas sus \
                     credenciales almacenadas.",
                    display_name(kind, p.name.as_deref()),
                    p.username,
                    p.host
                )
            }),
            Kind::Tailscale => None,
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

    /// Cuántos perfiles de `kind` quedarían aparte del que se está editando.
    /// Con al menos uno más, el nombre deja de ser opcional.
    fn others_of_kind(&self, kind: Kind, editing_id: Option<i64>) -> usize {
        match kind {
            Kind::OpenVpn => self
                .openvpn_profiles
                .iter()
                .filter(|p| Some(p.id) != editing_id)
                .count(),
            Kind::Forti => self
                .forti_profiles
                .iter()
                .filter(|p| Some(p.id) != editing_id)
                .count(),
            Kind::Tailscale => 0,
        }
    }

    /// ¿El perfil que se está capturando obliga a poner nombre? La pantalla lo
    /// anuncia en el paso del nombre, en vez de dejar que se descubra al fallar
    /// el guardado.
    pub fn name_required(&self) -> bool {
        self.profile_form
            .as_ref()
            .is_some_and(|f| self.others_of_kind(f.kind, f.editing_id) > 0)
    }

    /// Paso 1 → paso 2 del alta de OpenVPN3. Valida el archivo aquí mismo: no
    /// tiene sentido preguntar el nombre de algo que todavía no existe.
    pub fn advance_profile_step(&mut self) -> bool {
        let Some(form) = self.profile_form.as_mut() else {
            return false;
        };
        if form.step != FormStep::PickFile {
            return false;
        }
        let Some(idx) = form.path_idx() else {
            return false;
        };
        let value = form.fields[idx].input.value().trim().to_string();
        if value.is_empty() {
            form.error = Some("elige el archivo .ovpn para continuar".into());
            return false;
        }
        if !Path::new(&value).is_file() {
            form.error = Some(format!("no encuentro el archivo {value}"));
            return false;
        }
        form.set_step(FormStep::Name);
        true
    }

    /// Paso 2 → paso 1 (`esc`), conservando el archivo ya elegido.
    pub fn back_profile_step(&mut self) {
        if let Some(form) = self.profile_form.as_mut() {
            if form.step == FormStep::Name {
                form.set_step(FormStep::PickFile);
            }
        }
    }

    /// Abre el explorador de archivos del escritorio (zenity/kdialog/yad) en un
    /// hilo, para no bloquear el dibujado mientras el usuario elige.
    pub fn open_gui_file_picker(&mut self) {
        let Some(picker) = self.gui_picker else {
            self.toast(
                Level::Warn,
                format!(
                    "sin explorador del sistema: {} (usa F3)",
                    filedialog::unavailable_reason()
                ),
            );
            return;
        };
        if self.gui_picker_busy {
            return;
        }
        let start = self
            .profile_form
            .as_ref()
            .and_then(|f| f.path_idx().map(|i| f.fields[i].input.value().to_string()))
            .map(PathBuf::from)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir);
        self.gui_picker_busy = true;
        filedialog::open_async(picker, start, self.tx.clone());
    }

    /// Guarda el `ProfileForm` abierto. `Ok(true)` si guardó y debe cerrarse.
    pub fn save_profile_form(&mut self) -> bool {
        let Some(form) = self.profile_form.as_mut() else {
            return true;
        };
        let value = |i: usize| form.fields[i].input.value().trim().to_string();

        // Con al menos un perfil más de este tipo, el nombre deja de ser
        // opcional (si no, quedarían dos filas idénticas en el panel). Se
        // cuenta aquí sobre los campos y no con `others_of_kind`, que toma
        // `&self` entero y chocaría con el préstamo de `self.profile_form`.
        let others = match form.kind {
            Kind::OpenVpn => self
                .openvpn_profiles
                .iter()
                .filter(|p| Some(p.id) != form.editing_id)
                .count(),
            Kind::Forti => self
                .forti_profiles
                .iter()
                .filter(|p| Some(p.id) != form.editing_id)
                .count(),
            Kind::Tailscale => 0,
        };

        let result = match form.kind {
            Kind::OpenVpn => {
                let name = match validate_profile_name(others, &value(ProfileForm::OVPN_NAME)) {
                    Ok(name) => name,
                    Err(err) => {
                        form.error = Some(err);
                        return false;
                    }
                };
                let path = value(ProfileForm::OVPN_PATH);
                if path.is_empty() {
                    form.error = Some("falta el archivo .ovpn".into());
                    return false;
                }
                let config = PathBuf::from(&path);
                if !config.is_file() {
                    form.error = Some(format!("no encuentro el archivo {path}"));
                    return false;
                }
                match form.editing_id {
                    Some(id) => self.db.update_openvpn_profile(id, name.as_deref(), &config),
                    None => self
                        .db
                        .insert_openvpn_profile(name.as_deref(), &config)
                        .map(|_| ()),
                }
            }
            Kind::Forti => {
                let name = match validate_profile_name(others, &value(ProfileForm::FORTI_NAME)) {
                    Ok(name) => name,
                    Err(err) => {
                        form.error = Some(err);
                        return false;
                    }
                };
                let host = value(0);
                let username = value(1);
                let password = value(2);
                let password_env = value(3);
                let trusted_cert = value(4);
                let extra_args: Vec<String> = value(5)
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                if host.is_empty() || username.is_empty() {
                    form.error = Some("host y usuario son obligatorios".into());
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
                        name.as_deref(),
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
                            name.as_deref(),
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

    // -------------------------------------------------------- pegar / soltar

    /// Texto pegado en la terminal — que es también lo que llega al *soltar* un
    /// archivo sobre la ventana, porque el emulador lo traduce a un paste.
    pub fn on_paste(&mut self, text: String) {
        let dropped = normalize_dropped_path(&text);

        match self.screen {
            // Soltar el .ovpn sobre el asistente abre directo el formulario de
            // ese tipo con la ruta ya puesta: es el atajo que espera quien
            // arrastra el archivo a la pantalla que se lo está pidiendo.
            Screen::Setup(kind @ (Kind::OpenVpn | Kind::Forti)) => {
                let Some(path) = dropped else { return };
                self.enter_setup(kind);
                self.setup_confirm();
                self.paste_into_dropzone(path);
            }
            Screen::Config(ConfigScreen::ProfileForm) => self.paste_into_profile_form(text, dropped),
            Screen::Config(ConfigScreen::General) => self.paste_into_general_form(text, dropped),
            Screen::Config(ConfigScreen::FilePicker) => {
                if let Some(path) = dropped.filter(|p| p.is_file()) {
                    self.apply_picked_file(path);
                }
            }
            Screen::Tailscale(TailscaleScreen::RoutesForm) => {
                let line = first_line(&text);
                insert_text(&mut self.routes_input, &line);
            }
            _ => {}
        }
    }

    fn paste_into_profile_form(&mut self, text: String, dropped: Option<PathBuf>) {
        let Some(form) = self.profile_form.as_ref() else { return };
        let focus_is_dropzone = form.fields.get(form.focus).map(|f| f.pickable).unwrap_or(false);
        let has_dropzone = form.fields.iter().any(|f| f.pickable);

        // Con el dropzone enfocado, cualquier ruta va ahí. Con el foco en otro
        // campo, un archivo que existe de verdad también: quien arrastra un
        // .ovpn no quiere la ruta escrita dentro de "nombre".
        let to_dropzone = match &dropped {
            Some(path) => focus_is_dropzone || (has_dropzone && path.is_file()),
            None => false,
        };

        if to_dropzone {
            self.paste_into_dropzone(dropped.unwrap());
            return;
        }
        let line = first_line(&text);
        if let Some(form) = self.profile_form.as_mut() {
            if let Some(field) = form.fields.get_mut(form.focus) {
                insert_text(&mut field.input, &line);
            }
        }
    }

    /// Deja la ruta en la zona de importación y avisa si algo no cuadra, en vez
    /// de aceptarla en silencio.
    fn paste_into_dropzone(&mut self, path: PathBuf) {
        let Some(form) = self.profile_form.as_mut() else { return };
        let Some(idx) = form.fields.iter().position(|f| f.pickable) else { return };
        let value = path.to_string_lossy().into_owned();
        form.fields[idx].input = Input::new(value.clone());
        form.focus = idx;

        let is_ovpn = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("ovpn"))
            .unwrap_or(false);
        form.error = if !path.is_file() {
            Some(format!("no existe: {value}"))
        } else if !is_ovpn {
            Some(format!("ojo: {value} no termina en .ovpn (se usará igual)"))
        } else {
            None
        };
    }

    fn paste_into_general_form(&mut self, text: String, dropped: Option<PathBuf>) {
        let Some(form) = self.general_form.as_mut() else { return };
        // 2 y 3 son las rutas de los scripts de IPv6; el resto, texto normal.
        let is_path_field = matches!(form.focus, 2 | 3);
        if let (true, Some(path)) = (is_path_field, dropped) {
            let value = Input::new(path.to_string_lossy().into_owned());
            match form.focus {
                2 => form.ipv6_disable_script = value,
                3 => form.ipv6_enable_script = value,
                _ => {}
            }
            return;
        }
        let line = first_line(&text);
        match form.focus {
            0 => insert_text(&mut form.poll_interval_ms, &line),
            1 => insert_text(&mut form.log_capacity, &line),
            2 => insert_text(&mut form.ipv6_disable_script, &line),
            3 => insert_text(&mut form.ipv6_enable_script, &line),
            _ => {}
        }
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

    /// Conecta o desconecta el perfil seleccionado según su estado. Sobre una
    /// fila "No Configurada" no hay nada que conectar: abre el asistente.
    pub fn toggle(&mut self, idx: usize) {
        let Some(profile) = self.profiles.get(idx) else {
            return;
        };
        if profile.placeholder {
            let kind = profile.kind;
            self.enter_setup(kind);
            return;
        }
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
        if profile.placeholder {
            let kind = profile.kind;
            self.enter_setup(kind);
            return;
        }
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
        if profile.placeholder || profile.state.is_busy() {
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
            .filter(|i| !self.profiles[*i].placeholder)
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

    /// ¿Hay algún cierre de sesión en curso? Mientras lo haya, la pantalla de
    /// sesiones ignora las teclas de acción para no disparar dos veces.
    pub fn sessions_busy(&self) -> bool {
        self.killing_all || !self.killing_sessions.is_empty()
    }

    pub fn kill_selected_session(&mut self) {
        if self.sessions_busy() {
            return;
        }
        let Some(session) = self.all_openvpn_sessions.get(self.sessions_selected).cloned() else {
            return;
        };
        self.toast(Level::Info, format!("cerrando sesión {}…", short(&session.path)));
        self.killing_sessions.insert(session.path.clone());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (level, text) = match openvpn3::disconnect(&session.path) {
                Ok(()) => (Level::Ok, "sesión cerrada".to_string()),
                Err(err) => (Level::Error, format!("no pude cerrar la sesión: {err}")),
            };
            let _ = tx.send(Msg::SessionKillDone {
                path: Some(session.path),
                level,
                text,
            });
        });
    }

    pub fn kill_all_sessions(&mut self) {
        if self.sessions_busy() {
            return;
        }
        if self.all_openvpn_sessions.is_empty() {
            self.toast(Level::Info, "no hay sesiones openvpn3 activas");
            return;
        }
        let sessions = self.all_openvpn_sessions.clone();
        self.toast(Level::Info, format!("cerrando {} sesión(es)…", sessions.len()));
        self.killing_all = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (level, text) = match openvpn3::disconnect_all(&sessions) {
                Ok(()) => (Level::Ok, "todas las sesiones cerradas".to_string()),
                Err(err) => (Level::Error, format!("alguna sesión no cerró: {err}")),
            };
            let _ = tx.send(Msg::SessionKillDone {
                path: None,
                level,
                text,
            });
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

            Msg::SessionKillDone { path, level, text } => {
                match path {
                    Some(path) => {
                        self.killing_sessions.remove(&path);
                    }
                    None => self.killing_all = false,
                }
                self.toast(level, text);
            }

            // Volvió el explorador gráfico del sistema. Sin ruta = el usuario
            // canceló, y cancelar no merece un error en pantalla.
            Msg::GuiPickerDone { path, err } => {
                self.gui_picker_busy = false;
                match (path, err) {
                    (Some(path), _) => self.paste_into_dropzone(path),
                    (None, Some(err)) => self.toast(Level::Error, err),
                    (None, None) => {}
                }
            }

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
            if self.profiles[idx].kind != Kind::Forti
                || self.profiles[idx].placeholder
                || self.profiles[idx].state.is_busy()
            {
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

    /// Perfiles conectados / total (las filas "No Configurada" no cuentan).
    pub fn summary(&self) -> (usize, usize) {
        let real = self.profiles.iter().filter(|p| !p.placeholder);
        (
            real.clone().filter(|p| p.state.is_up()).count(),
            real.count(),
        )
    }
}

/// Construye las filas del panel VPNS. Un tipo sin perfiles NO desaparece:
/// deja una fila "…No Configurada" que lleva al asistente, para que se pueda
/// descubrir desde el propio panel.
pub fn build_profiles(openvpn: &[OpenVpnProfile], forti: &[FortiProfile]) -> Vec<Profile> {
    let mut profiles = Vec::new();
    if openvpn.is_empty() {
        profiles.push(Profile::placeholder(Kind::OpenVpn));
    }
    for (i, p) in openvpn.iter().enumerate() {
        profiles.push(Profile::new(
            display_name(Kind::OpenVpn, p.name.as_deref()),
            Kind::OpenVpn,
            i,
            p.config.to_string_lossy().into_owned(),
        ));
    }
    if forti.is_empty() {
        profiles.push(Profile::placeholder(Kind::Forti));
    }
    for (i, p) in forti.iter().enumerate() {
        profiles.push(Profile::new(
            display_name(Kind::Forti, p.name.as_deref()),
            Kind::Forti,
            i,
            p.host.clone(),
        ));
    }
    // Tailscale no es una lista de perfiles — es una sola conexión de este
    // equipo — pero vive en el mismo panel "VPNS", así que se representa
    // como una única fila sintética; `cfg_index` no se usa para este `Kind`.
    profiles.push(Profile::new(
        Kind::Tailscale.label().to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ovpn(id: i64, name: Option<&str>) -> OpenVpnProfile {
        OpenVpnProfile {
            id,
            name: name.map(str::to_string),
            config: Path::new("/x/a.ovpn").to_path_buf(),
        }
    }

    fn forti(id: i64, name: Option<&str>) -> FortiProfile {
        FortiProfile {
            id,
            name: name.map(str::to_string),
            host: "h:443".into(),
            username: "u".into(),
            password: None,
            password_env: None,
            trusted_cert: None,
            extra_args: Vec::new(),
        }
    }

    #[test]
    fn nombre_visible_antepone_el_tipo() {
        assert_eq!(display_name(Kind::OpenVpn, Some("thinkpad")), "OpenVPN3 (thinkpad)");
        assert_eq!(display_name(Kind::OpenVpn, None), "OpenVPN3");
        // Un nombre en blanco cuenta como "sin nombre".
        assert_eq!(display_name(Kind::Forti, Some("  ")), "OpenFortiVPN");
        assert_eq!(display_name(Kind::Tailscale, None), "Tailscale");
        assert_eq!(unconfigured_name(Kind::Forti), "OpenFortiVPN No Configurada");
    }

    #[test]
    fn el_nombre_es_obligatorio_a_partir_del_segundo_perfil() {
        // 0 → 1: sin nombre está bien, el tipo alcanza para identificarlo.
        assert_eq!(validate_profile_name(0, ""), Ok(None));
        // 1 → 2 sin nombre: quedarían dos filas idénticas.
        assert!(validate_profile_name(1, "   ").is_err());
        // 1 → 2 con nombre: correcto.
        assert_eq!(
            validate_profile_name(1, " casa "),
            Ok(Some("casa".to_string()))
        );
    }

    #[test]
    fn un_tipo_sin_perfiles_deja_fila_no_configurada() {
        let rows = build_profiles(&[], &[]);
        let names: Vec<&str> = rows.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "OpenVPN3 No Configurada",
                "OpenFortiVPN No Configurada",
                "Tailscale"
            ]
        );
        assert!(rows[0].placeholder && rows[1].placeholder);
        assert!(!rows[2].placeholder, "Tailscale siempre es una fila real");
    }

    #[test]
    fn con_perfiles_desaparece_el_placeholder_de_ese_tipo() {
        let rows = build_profiles(&[ovpn(1, Some("thinkpad")), ovpn(2, None)], &[forti(3, None)]);
        let names: Vec<&str> = rows.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "OpenVPN3 (thinkpad)",
                "OpenVPN3",
                "OpenFortiVPN",
                "Tailscale"
            ]
        );
        assert!(rows.iter().all(|p| !p.placeholder));
    }

    #[test]
    fn el_foco_cicla_entre_los_tres_paneles() {
        assert_eq!(Focus::Profiles.next(), Focus::Logs);
        assert_eq!(Focus::Logs.next(), Focus::ConfigButton);
        assert_eq!(Focus::ConfigButton.next(), Focus::Profiles);
        assert_eq!(Focus::Profiles.prev(), Focus::ConfigButton);
        assert_eq!(Focus::ConfigButton.prev(), Focus::Logs);
    }

    fn test_app() -> App {
        let (tx, rx) = std::sync::mpsc::channel();
        // El receptor se mantiene vivo: si se cierra, los `send` de la app
        // fallarían silenciosamente y el test dejaría de reflejar la realidad.
        std::mem::forget(rx);
        App::new(Db::open_in_memory().unwrap(), tx)
    }

    /// Sin ningún perfil, la app entra directo al asistente en OpenVPN3, y el
    /// carrusel da la vuelta en ambos sentidos.
    #[test]
    fn el_slider_del_asistente_arranca_en_openvpn_y_da_la_vuelta() {
        let mut app = test_app();
        assert_eq!(app.screen, Screen::Setup(Kind::OpenVpn));

        app.setup_slide(true);
        assert_eq!(app.screen, Screen::Setup(Kind::Forti));
        app.setup_slide(true);
        assert_eq!(app.screen, Screen::Setup(Kind::Tailscale));
        app.setup_slide(true);
        assert_eq!(app.screen, Screen::Setup(Kind::OpenVpn), "da la vuelta");
        app.setup_slide(false);
        assert_eq!(app.screen, Screen::Setup(Kind::Tailscale));
    }

    /// `enter` sobre una fila "No Configurada" lleva al asistente en ese tipo,
    /// en vez de intentar conectar algo que no existe.
    #[test]
    fn enter_sobre_fila_no_configurada_abre_el_asistente() {
        let mut app = test_app();
        app.screen = Screen::Dashboard;
        let forti_idx = app
            .profiles
            .iter()
            .position(|p| p.kind == Kind::Forti)
            .unwrap();
        assert!(app.profiles[forti_idx].placeholder);

        app.toggle(forti_idx);
        assert_eq!(app.screen, Screen::Setup(Kind::Forti));
    }

    /// Guardar desde el asistente usa el mismo formulario del menú de config,
    /// solo que recordando que debe volver al dashboard.
    #[test]
    fn el_asistente_reusa_el_formulario_de_config() {
        let mut app = test_app();
        app.enter_setup(Kind::Forti);
        app.setup_confirm();
        assert_eq!(app.screen, Screen::Config(ConfigScreen::ProfileForm));
        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.kind, Kind::Forti);
        assert_eq!(form.origin, FormOrigin::Setup);
    }

    /// Las filas "No Configurada" no cuentan como VPNs en el encabezado.
    #[test]
    fn el_resumen_ignora_los_placeholders() {
        let app = test_app();
        // Solo la fila real de Tailscale cuenta.
        assert_eq!(app.summary(), (0, 1));
    }

    // ------------------------------------------------------------ soltar archivo

    fn dropped(text: &str) -> Option<String> {
        normalize_dropped_path(text).map(|p| p.to_string_lossy().into_owned())
    }

    /// Cada emulador de terminal pega la ruta a su manera cuando sueltas un
    /// archivo sobre la ventana; todas estas formas son reales.
    #[test]
    fn normaliza_la_ruta_de_un_archivo_soltado() {
        let plain = "/home/user/vpns/thinkpad.ovpn";
        assert_eq!(dropped(plain).as_deref(), Some(plain));
        // Casi siempre viene con salto de línea al final (que además enviaría
        // el formulario si se tratara como tecleo normal).
        assert_eq!(dropped("/x/a.ovpn\n").as_deref(), Some("/x/a.ovpn"));
        assert_eq!(dropped("/x/a.ovpn\r\n").as_deref(), Some("/x/a.ovpn"));
        // Comillas cuando la ruta trae espacios.
        assert_eq!(
            dropped("'/home/user/mi carpeta/vpn.ovpn'").as_deref(),
            Some("/home/user/mi carpeta/vpn.ovpn")
        );
        assert_eq!(
            dropped("\"/home/user/mi carpeta/vpn.ovpn\"").as_deref(),
            Some("/home/user/mi carpeta/vpn.ovpn")
        );
        // URI file:// (GNOME/Nautilus y varios más).
        assert_eq!(
            dropped("file:///home/user/vpns/thinkpad.ovpn").as_deref(),
            Some("/home/user/vpns/thinkpad.ovpn")
        );
        assert_eq!(
            dropped("file://localhost/x/a.ovpn").as_deref(),
            Some("/x/a.ovpn")
        );
        // URI con percent-encoding.
        assert_eq!(
            dropped("file:///home/user/mi%20carpeta/vpn.ovpn").as_deref(),
            Some("/home/user/mi carpeta/vpn.ovpn")
        );
        // Espacios escapados al estilo shell.
        assert_eq!(
            dropped("/home/user/mi\\ carpeta/vpn.ovpn").as_deref(),
            Some("/home/user/mi carpeta/vpn.ovpn")
        );
        // Varios archivos soltados a la vez: se toma el primero.
        assert_eq!(
            dropped("\n/x/uno.ovpn\n/x/dos.ovpn\n").as_deref(),
            Some("/x/uno.ovpn")
        );
        // `~` se expande, porque también se pega a mano.
        let home = dirs::home_dir().unwrap().join("a.ovpn");
        assert_eq!(dropped("~/a.ovpn"), Some(home.to_string_lossy().into_owned()));
    }

    /// El `%` de un nombre de archivo normal no se toca: solo se decodifica
    /// cuando la fuente era un URI, que sí viene percent-encoded.
    #[test]
    fn solo_decodifica_percent_en_uris() {
        assert_eq!(dropped("/x/100%25.ovpn").as_deref(), Some("/x/100%25.ovpn"));
        assert_eq!(dropped("file:///x/100%25.ovpn").as_deref(), Some("/x/100%.ovpn"));
    }

    #[test]
    fn texto_que_no_es_ruta_no_pasa() {
        assert_eq!(dropped(""), None);
        assert_eq!(dropped("   \n  "), None);
        assert_eq!(dropped("hola mundo"), None);
        assert_eq!(dropped("https://ejemplo.com/a.ovpn"), None);
    }

    /// Soltar el archivo con el dropzone enfocado llena la ruta; el salto de
    /// línea final NO debe enviar el formulario.
    #[test]
    fn soltar_un_ovpn_llena_el_dropzone() {
        let mut app = test_app();
        let file = std::env::temp_dir().join("vpn-tui-test-drop.ovpn");
        std::fs::write(&file, "client\n").unwrap();

        app.open_profile_form_new(Kind::OpenVpn);
        app.on_paste(format!("{}\n", file.display()));

        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.fields[ProfileForm::OVPN_PATH].input.value(), file.to_string_lossy());
        assert_eq!(form.error, None);
        // Sigue en el formulario: no se guardó ni se salió por el `\n`.
        assert_eq!(app.screen, Screen::Config(ConfigScreen::ProfileForm));
        std::fs::remove_file(&file).ok();
    }

    /// Soltarlo con otro campo enfocado igual llena el dropzone: es lo que
    /// quiere quien arrastra un archivo, no escribir la ruta en "nombre".
    #[test]
    fn soltar_un_archivo_existente_va_al_dropzone_aunque_el_foco_este_en_otro_campo() {
        let mut app = test_app();
        let file = std::env::temp_dir().join("vpn-tui-test-drop2.ovpn");
        std::fs::write(&file, "client\n").unwrap();

        app.open_profile_form_new(Kind::OpenVpn);
        app.profile_form.as_mut().unwrap().focus = ProfileForm::OVPN_NAME;
        app.on_paste(file.to_string_lossy().into_owned());

        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.fields[ProfileForm::OVPN_PATH].input.value(), file.to_string_lossy());
        assert!(
            form.fields[ProfileForm::OVPN_NAME].input.value().is_empty(),
            "el nombre queda limpio"
        );
        assert_eq!(
            form.focus,
            ProfileForm::OVPN_PATH,
            "el foco se mueve a la zona de importación"
        );
        std::fs::remove_file(&file).ok();
    }

    /// Una ruta que no existe se muestra igual (para poder corregirla) pero con
    /// un error explícito, nunca en silencio.
    #[test]
    fn una_ruta_inexistente_avisa_en_el_dropzone() {
        let mut app = test_app();
        app.open_profile_form_new(Kind::OpenVpn);
        app.on_paste("/no/existe/x.ovpn".to_string());

        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.fields[ProfileForm::OVPN_PATH].input.value(), "/no/existe/x.ovpn");
        assert!(form.error.as_deref().unwrap_or_default().contains("no existe"));
    }

    /// Un archivo real con otra extensión se acepta, solo avisa.
    #[test]
    fn otra_extension_avisa_pero_no_bloquea() {
        let mut app = test_app();
        let file = std::env::temp_dir().join("vpn-tui-test-drop.conf");
        std::fs::write(&file, "client\n").unwrap();

        app.open_profile_form_new(Kind::OpenVpn);
        app.on_paste(file.to_string_lossy().into_owned());

        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.fields[ProfileForm::OVPN_PATH].input.value(), file.to_string_lossy());
        assert!(form.error.as_deref().unwrap_or_default().contains(".ovpn"));
        std::fs::remove_file(&file).ok();
    }

    /// En un campo de texto normal, pegar inserta el texto (sin el salto final).
    #[test]
    fn pegar_texto_normal_llena_el_campo_enfocado() {
        let mut app = test_app();
        app.open_profile_form_new(Kind::Forti);
        app.profile_form.as_mut().unwrap().focus = 0; // host:puerto
        app.on_paste("vpn.ejemplo.com:10443\n".to_string());

        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.fields[0].input.value(), "vpn.ejemplo.com:10443");
    }

    // -------------------------------------------------- asistente de dos pasos

    fn tmp_ovpn(name: &str) -> PathBuf {
        let file = std::env::temp_dir().join(name);
        std::fs::write(&file, "client\n").unwrap();
        file
    }

    /// El alta de OpenVPN3 va por pasos: primero el archivo, y solo después se
    /// pregunta el nombre. Antes era un formulario con todo a la vez.
    #[test]
    fn el_alta_de_openvpn_pregunta_primero_el_archivo_y_luego_el_nombre() {
        let mut app = test_app();
        let file = tmp_ovpn("vpn-tui-wizard-1.ovpn");

        app.open_profile_form_new(Kind::OpenVpn);
        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.step, FormStep::PickFile);
        assert_eq!(form.focus, ProfileForm::OVPN_PATH, "arranca en el dropzone");

        app.on_paste(file.to_string_lossy().into_owned());
        assert!(app.advance_profile_step(), "con archivo válido sí avanza");

        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.step, FormStep::Name);
        assert_eq!(form.focus, ProfileForm::OVPN_NAME);

        // `esc` en el paso del nombre regresa al del archivo, sin perderlo.
        app.back_profile_step();
        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.step, FormStep::PickFile);
        assert_eq!(form.fields[ProfileForm::OVPN_PATH].input.value(), file.to_string_lossy());
        std::fs::remove_file(&file).ok();
    }

    /// Sin archivo (o con uno inexistente) no se pasa al paso del nombre.
    #[test]
    fn no_se_avanza_de_paso_sin_un_archivo_valido() {
        let mut app = test_app();
        app.open_profile_form_new(Kind::OpenVpn);

        assert!(!app.advance_profile_step(), "sin ruta no avanza");
        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.step, FormStep::PickFile);
        assert!(form.error.as_deref().unwrap_or_default().contains("archivo"));

        app.on_paste("/no/existe/x.ovpn".to_string());
        assert!(!app.advance_profile_step(), "una ruta inexistente tampoco");
        assert_eq!(app.profile_form.as_ref().unwrap().step, FormStep::PickFile);
    }

    /// Editar no es alta: ahí se ven todos los campos juntos, como siempre.
    #[test]
    fn editar_un_perfil_muestra_todos_los_campos() {
        let mut app = test_app();
        let file = tmp_ovpn("vpn-tui-wizard-2.ovpn");
        app.db.insert_openvpn_profile(Some("casa"), &file).unwrap();
        app.reload_profiles();

        app.open_profile_form_edit(Kind::OpenVpn, 0);
        assert_eq!(app.profile_form.as_ref().unwrap().step, FormStep::All);
        std::fs::remove_file(&file).ok();
    }

    /// En Forti el nombre va al final: primero los datos de conexión, igual que
    /// en el asistente de OpenVPN3.
    #[test]
    fn forti_pide_el_nombre_al_final() {
        let form = ProfileForm::new_forti(None);
        assert_eq!(form.step, FormStep::All);
        assert_eq!(form.fields[0].label, "host:puerto");
        let last = form.fields.last().unwrap().label;
        assert!(last.starts_with("nombre"), "el último campo es el nombre: {last}");
        assert_eq!(form.fields.len() - 1, ProfileForm::FORTI_NAME);
    }

    #[test]
    fn la_sugerencia_de_nombre_sale_del_nombre_del_archivo() {
        assert_eq!(
            suggested_profile_name(Path::new("/home/user/vpns/thinkpad.ovpn")).as_deref(),
            Some("thinkpad")
        );
        assert_eq!(suggested_profile_name(Path::new("/")), None);
    }

    /// El primer perfil puede quedarse sin nombre; el segundo del mismo tipo lo
    /// exige, y la pantalla tiene que poder anunciarlo *antes* de guardar.
    #[test]
    fn el_nombre_es_obligatorio_desde_el_segundo_perfil_del_mismo_tipo() {
        let mut app = test_app();
        let file = tmp_ovpn("vpn-tui-wizard-3.ovpn");

        app.open_profile_form_new(Kind::OpenVpn);
        app.on_paste(file.to_string_lossy().into_owned());
        assert!(app.advance_profile_step());
        assert!(!app.name_required(), "el primero no necesita nombre");
        assert!(app.save_profile_form(), "guarda sin nombre");
        assert_eq!(app.openvpn_profiles.len(), 1);

        // Segundo perfil del mismo tipo: ahora sí es obligatorio.
        app.open_profile_form_new(Kind::OpenVpn);
        app.on_paste(file.to_string_lossy().into_owned());
        assert!(app.advance_profile_step());
        assert!(app.name_required(), "la pantalla debe avisarlo desde el paso");
        assert!(!app.save_profile_form(), "sin nombre no guarda");

        insert_text(
            &mut app.profile_form.as_mut().unwrap().fields[ProfileForm::OVPN_NAME].input,
            "trabajo",
        );
        assert!(app.save_profile_form());
        assert_eq!(app.openvpn_profiles.len(), 2);
        std::fs::remove_file(&file).ok();
    }

    /// Soltar el .ovpn sobre el asistente abre el formulario ya con la ruta.
    #[test]
    fn soltar_un_ovpn_en_el_asistente_abre_el_formulario_con_la_ruta() {
        let mut app = test_app();
        let file = std::env::temp_dir().join("vpn-tui-test-drop3.ovpn");
        std::fs::write(&file, "client\n").unwrap();

        app.enter_setup(Kind::OpenVpn);
        app.on_paste(file.to_string_lossy().into_owned());

        assert_eq!(app.screen, Screen::Config(ConfigScreen::ProfileForm));
        let form = app.profile_form.as_ref().unwrap();
        assert_eq!(form.kind, Kind::OpenVpn);
        assert_eq!(form.origin, FormOrigin::Setup);
        assert_eq!(form.fields[ProfileForm::OVPN_PATH].input.value(), file.to_string_lossy());
        std::fs::remove_file(&file).ok();
    }
}
