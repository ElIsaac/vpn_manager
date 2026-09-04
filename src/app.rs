//! Estado de la aplicación y transiciones.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::event::{Level, Msg, Snapshot};
use crate::vpn::{self, fortivpn, human_bytes, openvpn3, Iface, State, Stats};

/// Muestras de velocidad que se conservan para las gráficas.
const RATE_HISTORY: usize = 80;
/// Cuánto se queda visible un aviso en la barra de estado.
const TOAST_TTL: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    OpenVpn,
    Forti,
}

impl Kind {
    pub fn tag(&self) -> &'static str {
        match self {
            Kind::OpenVpn => "OVPN",
            Kind::Forti => "FORTI",
        }
    }
}

/// Un perfil de VPN tal y como lo ve la interfaz.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub kind: Kind,
    /// Índice dentro de `cfg.openvpn` o `cfg.fortivpn`.
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
    pub cfg: Config,
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
    tx: Sender<Msg>,
}

impl App {
    pub fn new(cfg: Config, tx: Sender<Msg>) -> Self {
        let mut profiles = Vec::new();
        for (i, p) in cfg.openvpn.iter().enumerate() {
            profiles.push(Profile::new(
                p.name.clone(),
                Kind::OpenVpn,
                i,
                p.config.to_string_lossy().into_owned(),
            ));
        }
        for (i, p) in cfg.fortivpn.iter().enumerate() {
            profiles.push(Profile::new(p.name.clone(), Kind::Forti, i, p.host.clone()));
        }

        let mut app = Self {
            cfg,
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
            tx,
        };
        let source = app.cfg.source.display().to_string();
        app.log(None, Level::Info, format!("configuración: {source}"));
        app
    }

    // ---------------------------------------------------------------- logging

    pub fn log(&mut self, idx: Option<usize>, level: Level, text: impl Into<String>) {
        let profile = idx
            .and_then(|i| self.profiles.get(i))
            .map(|p| p.name.clone());
        if self.logs.len() >= self.cfg.general.log_capacity {
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

    pub fn select_next(&mut self) {
        if !self.profiles.is_empty() {
            self.selected = (self.selected + 1) % self.profiles.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.profiles.is_empty() {
            self.selected = (self.selected + self.profiles.len() - 1) % self.profiles.len();
        }
    }

    pub fn selected_profile(&self) -> Option<&Profile> {
        self.profiles.get(self.selected)
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
                let config: PathBuf = self.cfg.openvpn[cfg_index].config.clone();
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
                let profile_cfg = self.cfg.fortivpn[cfg_index].clone();
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
        }
    }

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

    /// Ejecuta el script de IPv6 configurado.
    pub fn run_ipv6_script(&mut self) {
        let Some(script) = self.cfg.general.ipv6_script.clone() else {
            self.toast(Level::Warn, "no hay `ipv6_script` en la configuración");
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
        if self.cfg.general.ipv6_on_connect {
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

        // --- OpenVPN3 --------------------------------------------------------
        for idx in 0..self.profiles.len() {
            if self.profiles[idx].kind != Kind::OpenVpn || self.profiles[idx].state.is_busy() {
                continue;
            }
            let config = self.cfg.openvpn[self.profiles[idx].cfg_index]
                .config
                .clone();
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
