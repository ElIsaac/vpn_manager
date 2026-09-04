//! Carga y resolución de la configuración (`config.toml`).

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Plantilla que se escribe cuando no existe ningún `config.toml`.
pub const TEMPLATE: &str = include_str!("../config.example.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    /// Perfiles de OpenVPN3.
    #[serde(default, rename = "openvpn")]
    pub openvpn: Vec<OpenVpnProfile>,
    /// Perfiles de OpenFortiVPN.
    #[serde(default, rename = "fortivpn")]
    pub fortivpn: Vec<FortiProfile>,

    /// Ruta desde la que se cargó (no viene del TOML).
    #[serde(skip)]
    pub source: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct General {
    /// Script opcional para deshabilitar IPv6 (se ejecuta con `sudo -n`).
    pub ipv6_script: Option<PathBuf>,
    /// Cada cuánto se refresca el estado de las VPNs.
    pub poll_interval_ms: u64,
    /// Líneas de log que se conservan en memoria.
    pub log_capacity: usize,
    /// Deshabilitar IPv6 automáticamente tras una conexión exitosa.
    pub ipv6_on_connect: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            ipv6_script: None,
            poll_interval_ms: 2000,
            log_capacity: 2000,
            ipv6_on_connect: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenVpnProfile {
    pub name: String,
    /// Ruta al archivo `.ovpn`.
    pub config: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FortiProfile {
    pub name: String,
    /// `host:puerto`.
    pub host: String,
    pub username: String,
    /// Password en claro (usa `password_env` si prefieres no guardarlo aquí).
    #[serde(default)]
    pub password: Option<String>,
    /// Nombre de la variable de entorno que contiene el password.
    #[serde(default)]
    pub password_env: Option<String>,
    /// Huella del certificado aceptado (`--trusted-cert`).
    #[serde(default)]
    pub trusted_cert: Option<String>,
    /// Argumentos extra para `openfortivpn`.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

impl FortiProfile {
    /// Resuelve el password: variable de entorno primero, luego el literal.
    pub fn resolve_password(&self) -> Result<String> {
        if let Some(var) = &self.password_env {
            if let Ok(v) = env::var(var) {
                if !v.is_empty() {
                    return Ok(v);
                }
            }
        }
        match &self.password {
            Some(p) if !p.is_empty() => Ok(p.clone()),
            _ => bail!(
                "el perfil '{}' no tiene password (define `password` o exporta `{}`)",
                self.name,
                self.password_env.as_deref().unwrap_or("VPN_TUI_PASSWORD")
            ),
        }
    }
}

impl Config {
    /// Busca el `config.toml` en, por orden: `$VPN_TUI_CONFIG`, el directorio
    /// del binario, `./config.toml` y `~/.config/vpn-tui/config.toml`.
    pub fn load() -> Result<Self> {
        let path = match Self::find() {
            Some(p) => p,
            None => {
                let target = Self::user_path();
                Self::write_template(&target)?;
                bail!(
                    "no había configuración; escribí una plantilla en {}\n\
                     Edítala y vuelve a ejecutar vpn-tui.",
                    target.display()
                );
            }
        };
        Self::from_path(&path)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("no pude leer {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("{} no es un TOML válido", path.display()))?;
        cfg.source = path.to_path_buf();

        // Expande `~` y resuelve rutas relativas contra el directorio del config.
        let base = path.parent().unwrap_or(Path::new("."));
        for p in &mut cfg.openvpn {
            p.config = resolve(&p.config, base);
        }
        if let Some(script) = cfg.general.ipv6_script.take() {
            cfg.general.ipv6_script = Some(resolve(&script, base));
        }

        if cfg.openvpn.is_empty() && cfg.fortivpn.is_empty() {
            bail!(
                "{} no define ningún perfil ([[openvpn]] o [[fortivpn]])",
                path.display()
            );
        }
        if cfg.general.poll_interval_ms < 250 {
            cfg.general.poll_interval_ms = 250;
        }
        if cfg.general.log_capacity < 100 {
            cfg.general.log_capacity = 100;
        }
        Ok(cfg)
    }

    fn find() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = env::var("VPN_TUI_CONFIG") {
            candidates.push(PathBuf::from(p));
        }
        if let Ok(exe) = env::current_exe() {
            // ./target/<profile>/vpn-tui -> raíz del proyecto
            for up in [1usize, 3, 4] {
                let mut dir = exe.clone();
                for _ in 0..up {
                    if !dir.pop() {
                        break;
                    }
                }
                candidates.push(dir.join("config.toml"));
            }
        }
        candidates.push(PathBuf::from("config.toml"));
        candidates.push(Self::user_path());
        candidates.into_iter().find(|p| p.is_file())
    }

    pub fn user_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("vpn-tui")
            .join("config.toml")
    }

    fn write_template(target: &Path) -> Result<()> {
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("no pude crear {}", dir.display()))?;
        }
        std::fs::write(target, TEMPLATE)
            .with_context(|| format!("no pude escribir {}", target.display()))?;
        restrict_permissions(target);
        Ok(())
    }
}

/// Expande `~` y vuelve absolutas las rutas relativas respecto a `base`.
fn resolve(path: &Path, base: &Path) -> PathBuf {
    let expanded = if let Ok(rest) = path.strip_prefix("~") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => path.to_path_buf(),
        }
    } else {
        path.to_path_buf()
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

/// `chmod 600` — el config puede contener credenciales.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}
