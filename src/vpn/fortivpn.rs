//! Operaciones sobre `openfortivpn`.
//!
//! El proceso corre bajo `sudo -n`, así que la TUI nunca puede quedarse
//! esperando una contraseña. Las credenciales van en un archivo temporal
//! `chmod 600` en vez de en la línea de comandos, para que no aparezcan en `ps`.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::db::FortiProfile;
use crate::event::{Level, Msg};

use super::{run, run_checked};

const BIN: &str = "openfortivpn";

/// Marca que `openfortivpn` imprime cuando el túnel queda operativo.
const UP_MARKER: &str = "tunnel is up and running";

/// Lanza `openfortivpn` y devuelve el PID de `sudo`.
///
/// Los hilos de lectura reenvían cada línea al canal como [`Msg::Log`] y
/// notifican [`Msg::FortiUp`] / [`Msg::FortiExited`].
pub fn spawn(idx: usize, profile: &FortiProfile, tx: Sender<Msg>) -> Result<u32> {
    if !super::has_command(BIN) {
        bail!(
            "`{BIN}` no está instalado (sudo dnf install openfortivpn / apt install openfortivpn)"
        );
    }

    let secrets = SecretFile::write(profile)?;

    let mut cmd = Command::new("sudo");
    cmd.arg("-n")
        .arg(BIN)
        .arg("-c")
        .arg(secrets.path())
        .args(&profile.extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("no pude ejecutar `sudo {BIN}`"))?;
    let pid = child.id();

    forward(child.stdout.take(), idx, tx.clone());
    forward(child.stderr.take(), idx, tx.clone());

    std::thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code());
        drop(secrets); // borra el archivo de credenciales
        let _ = tx.send(Msg::FortiExited { idx, code });
    });

    Ok(pid)
}

/// Envía una señal al túnel. Devuelve el método que funcionó.
///
/// `openfortivpn` corre como root, así que un `kill` directo desde el usuario
/// falla con EPERM; hay que pasar por `sudo -n`. Se intenta primero `pkill -x`
/// porque es la regla acotada que instala `install.sh`; el `kill` por PID (más
/// preciso, necesario si hay varios túneles Forti) va después, por si sudoers
/// también lo permite.
pub fn signal(pid: u32, sig: &str) -> Result<String> {
    let pid_str = pid.to_string();
    let flag = format!("-{sig}");
    let attempts: [(&str, &str, Vec<&str>); 3] = [
        ("sudo pkill", "sudo", vec!["-n", "pkill", &flag, "-x", BIN]),
        ("sudo kill", "sudo", vec!["-n", "kill", &flag, &pid_str]),
        ("kill", "kill", vec![&flag, &pid_str]),
    ];

    let mut last = String::from("sin intentos");
    let mut missing_sudoers = false;
    for (label, program, args) in attempts {
        match run_checked(program, &args, Duration::from_secs(10)) {
            Ok(_) => return Ok(format!("{label} -{sig}")),
            Err(err) => {
                if err.to_string().to_ascii_lowercase().contains("password is required") {
                    missing_sudoers = true;
                }
                last = format!("{label}: {err}");
            }
        }
    }
    if missing_sudoers {
        bail!(
            "faltan permisos de sudo para openfortivpn — corre 'sudo ./install.sh' \
             en el proyecto para instalarlos (último intento: {last})"
        );
    }
    bail!("no pude enviar SIG{sig} al PID {pid} ({last})")
}

/// PIDs de procesos `openfortivpn` vivos.
pub fn running_pids() -> Vec<u32> {
    let Ok(out) = run("pgrep", &["-x", BIN], Duration::from_secs(5)) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Reenvía línea por línea una tubería del proceso al canal de mensajes.
fn forward<R: Read + Send + 'static>(pipe: Option<R>, idx: usize, tx: Sender<Msg>) {
    let Some(pipe) = pipe else { return };
    std::thread::spawn(move || {
        let mut up_reported = false;
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            let line = line.trim_end().to_string();
            if line.is_empty() {
                continue;
            }
            let (level, text) = classify(&line);
            if !up_reported && text.to_ascii_lowercase().contains(UP_MARKER) {
                up_reported = true;
                if tx.send(Msg::FortiUp { idx }).is_err() {
                    break;
                }
            }
            if tx.send(Msg::Log(Some(idx), level, text)).is_err() {
                break;
            }
        }
    });
}

/// Traduce el prefijo de `openfortivpn` (`INFO:`, `ERROR:`…) a un [`Level`].
fn classify(line: &str) -> (Level, String) {
    let trimmed = line.trim();
    for (prefix, level) in [
        ("ERROR:", Level::Error),
        ("WARN:", Level::Warn),
        ("INFO:", Level::Info),
        ("DEBUG:", Level::Info),
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return (level, rest.trim().to_string());
        }
    }
    if trimmed.starts_with("sudo:") {
        return (Level::Error, trimmed.to_string());
    }
    (Level::Info, trimmed.to_string())
}

/// Archivo temporal `chmod 600` con las credenciales; se borra al soltarlo.
struct SecretFile {
    path: PathBuf,
}

impl SecretFile {
    fn write(profile: &FortiProfile) -> Result<Self> {
        let password = profile.resolve_password()?;
        let (host, port) = split_host_port(&profile.host);

        let dir = runtime_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("no pude crear {}", dir.display()))?;
        harden(&dir, 0o700);

        let path = dir.join(format!(
            "forti-{}-{}.conf",
            std::process::id(),
            profile.name
        ));
        let mut body = String::new();
        body.push_str("# generado por vpn-tui; se borra al desconectar\n");
        body.push_str(&format!("host = {host}\n"));
        body.push_str(&format!("port = {port}\n"));
        body.push_str(&format!("username = {}\n", profile.username));
        body.push_str(&format!("password = {password}\n"));
        if let Some(cert) = &profile.trusted_cert {
            if !cert.is_empty() {
                body.push_str(&format!("trusted-cert = {cert}\n"));
            }
        }

        let mut file = std::fs::File::create(&path)
            .with_context(|| format!("no pude crear {}", path.display()))?;
        harden(&path, 0o600);
        file.write_all(body.as_bytes())
            .with_context(|| format!("no pude escribir {}", path.display()))?;
        file.sync_all().ok();

        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `$XDG_RUNTIME_DIR/vpn-tui`, con respaldo en el directorio temporal.
fn runtime_dir() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("vpn-tui")
}

#[cfg(unix)]
fn harden(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn harden(_path: &Path, _mode: u32) {}

/// Separa `host:puerto`; el puerto por omisión de FortiGate SSL-VPN es 443.
fn split_host_port(raw: &str) -> (String, String) {
    match raw.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            (host.to_string(), port.to_string())
        }
        _ => (raw.to_string(), "443".to_string()),
    }
}

/// Mensaje corto para explicar por qué murió el proceso.
pub fn exit_reason(code: Option<i32>) -> String {
    match code {
        Some(0) => "terminó normalmente".to_string(),
        Some(1) => "openfortivpn falló (credenciales o certificado?)".to_string(),
        Some(c) => format!("openfortivpn terminó con código {c}"),
        None => "openfortivpn terminó por señal".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separa_host_y_puerto() {
        assert_eq!(
            split_host_port("vpn.example.com:10443"),
            ("vpn.example.com".into(), "10443".into())
        );
        assert_eq!(
            split_host_port("203.0.113.10:10443"),
            ("203.0.113.10".into(), "10443".into())
        );
        assert_eq!(
            split_host_port("vpn.example.com"),
            ("vpn.example.com".into(), "443".into())
        );
    }

    #[test]
    fn clasifica_lineas() {
        assert_eq!(
            classify("ERROR:  Login failed."),
            (Level::Error, "Login failed.".to_string())
        );
        assert_eq!(
            classify("INFO:   Tunnel is up and running."),
            (Level::Info, "Tunnel is up and running.".to_string())
        );
        assert_eq!(classify("sudo: a password is required").0, Level::Error);
    }
}
