//! Mensajes y productores de eventos (teclado, reloj, sondeo de estado).

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use ratatui::crossterm::event::{self as term, Event as TermEvent};

use crate::vpn::{self, openvpn3, tailscale, Iface, Stats};

/// Severidad de una línea de log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

/// Todo lo que puede despertar al bucle principal.
#[derive(Debug)]
pub enum Msg {
    /// Entrada de terminal.
    Input(TermEvent),
    /// Latido para redibujar (animaciones, contadores de tiempo).
    Tick,
    /// Línea de log. `Some(idx)` la atribuye a un perfil.
    Log(Option<usize>, Level, String),

    OvpnConnected {
        idx: usize,
        session: String,
    },
    OvpnDisconnected {
        idx: usize,
    },
    OvpnFailed {
        idx: usize,
        err: String,
    },

    FortiSpawned {
        idx: usize,
        pid: u32,
    },
    FortiUp {
        idx: usize,
    },
    FortiExited {
        idx: usize,
        code: Option<i32>,
    },
    FortiFailed {
        idx: usize,
        err: String,
    },

    /// Terminó un cierre de sesión openvpn3 lanzado desde la pantalla de
    /// sesiones. `path` = `None` cuando era "matar todas".
    SessionKillDone {
        path: Option<String>,
        level: Level,
        text: String,
    },

    /// Cerró el diálogo de archivo gráfico del sistema (zenity/kdialog/yad).
    /// `path: None` = el usuario canceló, que no es un error.
    GuiPickerDone {
        path: Option<std::path::PathBuf>,
        err: Option<String>,
    },

    /// Estado del sistema recogido periódicamente.
    Poll(Box<Snapshot>),
}

/// Foto del estado real del sistema, independiente de lo que crea la TUI.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub openvpn_available: bool,
    pub fortivpn_available: bool,
    pub sessions: Vec<openvpn3::Session>,
    /// Estadísticas por *session path*.
    pub stats: Vec<(String, Stats)>,
    /// PIDs de procesos `openfortivpn` vivos.
    pub forti_pids: Vec<u32>,
    pub interfaces: Vec<Iface>,
    pub ipv6_disabled: Option<bool>,
    pub tailscale: Option<tailscale::Status>,
}

/// Canal único por el que llegan todos los mensajes al bucle principal.
pub struct Events {
    pub rx: Receiver<Msg>,
    pub tx: Sender<Msg>,
}

impl Events {
    /// Arranca los hilos de teclado, latido y sondeo.
    pub fn start(poll_interval: Duration) -> Self {
        let (tx, rx) = mpsc::channel();

        // Teclado / redimensionado.
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                while let Ok(ev) = term::read() {
                    if tx.send(Msg::Input(ev)).is_err() {
                        break;
                    }
                }
            });
        }

        // Latido: mantiene vivos los contadores de tiempo y el spinner.
        {
            let tx = tx.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(250));
                if tx.send(Msg::Tick).is_err() {
                    break;
                }
            });
        }

        // Sondeo del estado real del sistema.
        {
            let tx = tx.clone();
            std::thread::spawn(move || loop {
                let snapshot = collect_snapshot();
                if tx.send(Msg::Poll(Box::new(snapshot))).is_err() {
                    break;
                }
                std::thread::sleep(poll_interval);
            });
        }

        Self { rx, tx }
    }
}

/// Consulta `openvpn3`, `pgrep` y `/proc` para saber qué hay realmente activo.
fn collect_snapshot() -> Snapshot {
    let openvpn_available = vpn::has_command("openvpn3");
    let fortivpn_available = vpn::has_command("openfortivpn");

    let sessions = if openvpn_available {
        openvpn3::sessions().unwrap_or_default()
    } else {
        Vec::new()
    };

    let stats = sessions
        .iter()
        .filter(|s| s.is_connected())
        .filter_map(|s| openvpn3::stats(&s.path).ok().map(|st| (s.path.clone(), st)))
        .collect();

    let tailscale = if tailscale::is_available() {
        tailscale::status().ok()
    } else {
        None
    };

    Snapshot {
        openvpn_available,
        fortivpn_available,
        sessions,
        stats,
        forti_pids: crate::vpn::fortivpn::running_pids(),
        interfaces: vpn::tunnel_interfaces(),
        ipv6_disabled: vpn::ipv6_disabled(),
        tailscale,
    }
}
