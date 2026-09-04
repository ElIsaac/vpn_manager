//! Envoltorios sobre `openvpn3` y `openfortivpn`.

pub mod fortivpn;
pub mod openvpn3;
pub mod tailscale;

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

/// Estado de un perfil dentro de la TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Failed(String),
}

impl State {
    pub fn label(&self) -> &str {
        match self {
            State::Disconnected => "desconectado",
            State::Connecting => "conectando",
            State::Connected => "conectado",
            State::Disconnecting => "desconectando",
            State::Failed(_) => "error",
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, State::Connecting | State::Disconnecting)
    }

    pub fn is_up(&self) -> bool {
        matches!(self, State::Connected)
    }
}

/// Contadores de tráfico de un túnel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// `true` si el ejecutable existe en el `PATH`.
pub fn has_command(cmd: &str) -> bool {
    which(cmd).is_some()
}

/// Resuelve un ejecutable recorriendo el `PATH`.
pub fn which(cmd: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(cmd))
        .find(|candidate| candidate.is_file())
}

/// Ejecuta un comando capturando su salida, abortándolo si excede `timeout`.
///
/// Lee stdout/stderr en hilos aparte para no bloquearse si el proceso llena el
/// buffer de la tubería.
pub fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("no pude ejecutar `{program}`"))?;

    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!(
                    "`{program}` excedió el tiempo límite ({}s)",
                    timeout.as_secs()
                ));
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };

    Ok(Output {
        status,
        stdout: stdout.recv().unwrap_or_default(),
        stderr: stderr.recv().unwrap_or_default(),
    })
}

/// Lanza un hilo que vacía una tubería y devuelve su contenido por el canal.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

/// Ejecuta un comando y devuelve su salida combinada, o un error con el motivo.
pub fn run_checked(program: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let out = run(program, args, timeout)?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if out.status.success() {
        Ok(format!("{stdout}{stderr}"))
    } else {
        let msg = first_meaningful_line(&stderr)
            .or_else(|| first_meaningful_line(&stdout))
            .unwrap_or_else(|| format!("código de salida {:?}", out.status.code()));
        Err(anyhow!(msg))
    }
}

/// Primera línea no vacía de un texto, útil para mensajes de error compactos.
pub fn first_meaningful_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.to_string())
}

/// Formatea bytes en unidades legibles.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Formatea una duración como `1d 02:03:04` / `02:03` según su magnitud.
pub fn human_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (days, rem) = (total / 86_400, total % 86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    if days > 0 {
        format!("{days}d {h:02}:{m:02}:{s:02}")
    } else if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Una interfaz de túnel activa con su dirección y contadores de tráfico.
#[derive(Debug, Clone, Default)]
pub struct Iface {
    pub name: String,
    pub addr: String,
    pub rx: u64,
    pub tx: u64,
}

/// Interfaces de túnel activas (`tun*`, `ppp*`, `vpn*`).
pub fn tunnel_interfaces() -> Vec<Iface> {
    let Ok(out) = run("ip", &["-o", "-4", "addr", "show"], Duration::from_secs(5)) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _idx = fields.next()?;
            let name = fields.next()?;
            if !(name.starts_with("tun") || name.starts_with("ppp") || name.starts_with("vpn")) {
                return None;
            }
            let addr = fields
                .skip_while(|f| *f != "inet")
                .nth(1)
                .unwrap_or("-")
                .to_string();
            let (rx, tx) = iface_counters(name);
            Some(Iface {
                name: name.to_string(),
                addr,
                rx,
                tx,
            })
        })
        .collect()
}

/// Contadores `rx_bytes` / `tx_bytes` de una interfaz.
fn iface_counters(name: &str) -> (u64, u64) {
    let read = |kind: &str| {
        std::fs::read_to_string(format!("/sys/class/net/{name}/statistics/{kind}"))
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    (read("rx_bytes"), read("tx_bytes"))
}

/// Lee el estado global de IPv6: `Some(true)` si está deshabilitado.
pub fn ipv6_disabled() -> Option<bool> {
    let raw = std::fs::read_to_string("/proc/sys/net/ipv6/conf/all/disable_ipv6").ok()?;
    Some(raw.trim() == "1")
}
