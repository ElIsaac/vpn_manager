//! Operaciones sobre `openvpn3` (D-Bus).

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use super::{first_meaningful_line, run, run_checked, Stats};

const BIN: &str = "openvpn3";

/// Una sesión reportada por `openvpn3 sessions-list`.
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub path: String,
    pub config_name: String,
    pub device: Option<String>,
    pub status: String,
}

impl Session {
    /// `openvpn3` reporta "Connection, Client connected" cuando el túnel está arriba.
    pub fn is_connected(&self) -> bool {
        let s = self.status.to_ascii_lowercase();
        s.contains("client connected") || s.contains("connection, connected")
    }
}

/// Inicia una sesión y devuelve su *session path*.
pub fn connect(config: &Path) -> Result<String> {
    if !config.is_file() {
        bail!(
            "no encuentro el archivo de configuración {}",
            config.display()
        );
    }
    if !super::has_command(BIN) {
        bail!("`{BIN}` no está instalado (sudo dnf install openvpn3 / apt install openvpn3)");
    }

    let path = config.to_string_lossy().into_owned();
    let out = run(
        BIN,
        &["session-start", "--config", &path],
        Duration::from_secs(45),
    )?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    match parse_session_path(&text) {
        Some(session) => Ok(session),
        None if out.status.success() => {
            Err(anyhow!("la sesión inició pero no reportó `Session path`"))
        }
        None => Err(anyhow!(first_meaningful_line(&text).unwrap_or_else(
            || format!("openvpn3 falló ({:?})", out.status.code())
        ))),
    }
}

/// Cierra una sesión por su *session path*.
pub fn disconnect(session_path: &str) -> Result<()> {
    run_checked(
        BIN,
        &["session-manage", "--disconnect", "--path", session_path],
        Duration::from_secs(20),
    )?;
    Ok(())
}

/// Lista las sesiones activas; `Ok(vec![])` si no hay ninguna.
pub fn sessions() -> Result<Vec<Session>> {
    let out = run(BIN, &["sessions-list"], Duration::from_secs(10))?;
    let text = String::from_utf8_lossy(&out.stdout);
    // Sin sesiones, `openvpn3` devuelve un código distinto de cero.
    if !out.status.success() && !text.contains("Path:") {
        return Ok(Vec::new());
    }
    Ok(parse_sessions(&text))
}

/// Contadores de tráfico de una sesión.
pub fn stats(session_path: &str) -> Result<Stats> {
    let out = run(
        BIN,
        &["session-stats", "--path", session_path],
        Duration::from_secs(10),
    )?;
    if !out.status.success() {
        bail!("no pude leer las estadísticas de la sesión");
    }
    Ok(parse_stats(&String::from_utf8_lossy(&out.stdout)))
}

/// `openvpn3` reporta el nombre de configuración como la ruta que se le pasó,
/// pero puede haberse importado con otro nombre: se comparan ruta, archivo y raíz.
pub fn matches_config(session: &Session, config: &Path) -> bool {
    if session.config_name.is_empty() {
        return false;
    }
    let candidates = [
        Some(config.to_string_lossy().into_owned()),
        config.file_name().map(|f| f.to_string_lossy().into_owned()),
        config.file_stem().map(|f| f.to_string_lossy().into_owned()),
    ];
    candidates
        .iter()
        .flatten()
        .any(|candidate| candidate == &session.config_name)
}

fn parse_session_path(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.split_once("Session path:")
            .map(|(_, rest)| rest.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

fn parse_sessions(text: &str) -> Vec<Session> {
    let mut sessions = Vec::new();
    let mut current: Option<Session> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = field(trimmed, "Path:") {
            if let Some(done) = current.take() {
                sessions.push(done);
            }
            current = Some(Session {
                path: rest.to_string(),
                ..Session::default()
            });
            continue;
        }
        let Some(session) = current.as_mut() else {
            continue;
        };
        if let Some(rest) = field(trimmed, "Config name:") {
            session.config_name = rest.to_string();
        } else if let Some(rest) = field(trimmed, "Status:") {
            session.status = rest.to_string();
        }
        // `Device:` viene en la misma línea que `Owner:`, alineado a la derecha.
        if let Some(rest) = column(trimmed, "Device:") {
            session.device = Some(rest.to_string());
        }
    }
    if let Some(done) = current {
        sessions.push(done);
    }
    sessions
}

/// Extrae el valor de un campo `Etiqueta: valor` al inicio de la línea,
/// recortando columnas extra como el `PID: 123` que `openvpn3` alinea a la derecha.
fn field<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    trim_column(line.strip_prefix(label)?)
}

/// Igual que [`field`], pero para etiquetas que aparecen en cualquier columna.
fn column<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    trim_column(line.split_once(label)?.1)
}

/// Recorta un valor hasta el siguiente hueco de dos espacios (fin de columna).
fn trim_column(rest: &str) -> Option<&str> {
    let rest = rest.trim();
    let cut = rest.find("  ").unwrap_or(rest.len());
    let value = rest[..cut].trim();
    (!value.is_empty()).then_some(value)
}

fn parse_stats(text: &str) -> Stats {
    let mut stats = Stats::default();
    for line in text.lines() {
        let trimmed = line.trim();
        let key: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let value: u64 = trimmed
            .trim_end()
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        match key.as_str() {
            "BYTES_IN" => stats.bytes_in = value,
            "BYTES_OUT" => stats.bytes_out = value,
            _ => {}
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\
-----------------------------------------------------------------------------
        Path: /net/openvpn/v3/sessions/abc123
     Created: Wed Feb  4 16:39:24 2026                  PID: 4242
       Owner: isaac                                  Device: tun0
 Config name: thinkpad.ovpn
Session name: vpn.example.com
      Status: Connection, Client connected
-----------------------------------------------------------------------------
";

    #[test]
    fn extrae_session_path() {
        let text = "Connected\nSession path: /net/openvpn/v3/sessions/xyz\n";
        assert_eq!(
            parse_session_path(text).as_deref(),
            Some("/net/openvpn/v3/sessions/xyz")
        );
        assert_eq!(parse_session_path("nada aquí"), None);
    }

    #[test]
    fn parsea_lista_de_sesiones() {
        let sessions = parse_sessions(LIST);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.path, "/net/openvpn/v3/sessions/abc123");
        assert_eq!(s.config_name, "thinkpad.ovpn");
        assert_eq!(s.device.as_deref(), Some("tun0"));
        assert!(s.is_connected());
    }

    #[test]
    fn empareja_configuracion() {
        let session = Session {
            config_name: "thinkpad.ovpn".into(),
            ..Session::default()
        };
        assert!(matches_config(
            &session,
            Path::new("/home/isaac/apps/vpns/thinkpad.ovpn")
        ));
        assert!(!matches_config(&session, Path::new("/otro/perfil.ovpn")));
        assert!(!matches_config(&Session::default(), Path::new("x.ovpn")));
    }

    #[test]
    fn parsea_estadisticas() {
        let text = "Connection statistics:\n     BYTES_IN....................226884\n     BYTES_OUT...................30928\n";
        let stats = parse_stats(text);
        assert_eq!(stats.bytes_in, 226_884);
        assert_eq!(stats.bytes_out, 30_928);
    }
}
