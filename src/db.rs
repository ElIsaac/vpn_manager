//! Persistencia en SQLite: perfiles de OpenVPN3/FortiVPN y ajustes generales.
//!
//! Reemplaza el `config.toml` original. Si al arrancar no existe la base de
//! datos pero sí un `config.toml` localizable (misma búsqueda que usaba el
//! `Config` viejo), sus perfiles se importan una sola vez.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct General {
    pub ipv6_disable_script: Option<PathBuf>,
    pub ipv6_enable_script: Option<PathBuf>,
    pub poll_interval_ms: u64,
    pub log_capacity: usize,
    pub ipv6_on_connect: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            ipv6_disable_script: None,
            ipv6_enable_script: None,
            poll_interval_ms: 2000,
            log_capacity: 2000,
            ipv6_on_connect: false,
        }
    }
}

/// Rutas estándar donde el instalador deja los scripts de IPv6 (root:root, no
/// editables por el usuario — la regla de sudoers apunta ahí).
pub const STD_IPV6_DISABLE: &str = "/usr/local/libexec/vpn/ipv6-disable.sh";
pub const STD_IPV6_ENABLE: &str = "/usr/local/libexec/vpn/ipv6-enable.sh";

/// Nombres de los scripts en su ubicación vieja (dentro del home del usuario).
const LEGACY_IPV6_DISABLE: &str = "ipv6_deshabilitar.sh";
const LEGACY_IPV6_ENABLE: &str = "habilitar_ipv6.sh";

#[derive(Debug, Clone)]
pub struct OpenVpnProfile {
    pub id: i64,
    /// Opcional: la fila se muestra como `OpenVPN3 (<nombre>)`, o solo
    /// `OpenVPN3` cuando no hay nombre.
    pub name: Option<String>,
    pub config: PathBuf,
}

#[derive(Debug, Clone)]
pub struct FortiProfile {
    pub id: i64,
    pub name: Option<String>,
    pub host: String,
    pub username: String,
    pub password: Option<String>,
    pub password_env: Option<String>,
    pub trusted_cert: Option<String>,
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
                self.name.as_deref().unwrap_or("OpenFortiVPN"),
                self.password_env.as_deref().unwrap_or("VPN_TUI_PASSWORD")
            ),
        }
    }
}

/// Todo lo que la TUI necesita, ya cargado desde la base de datos.
pub struct Db {
    conn: Connection,
    pub source: PathBuf,
}

impl Db {
    /// Abre (creando si hace falta) la base de datos en su ubicación por
    /// omisión, migrando desde `config.toml` la primera vez que se detecta.
    pub fn open() -> Result<Self> {
        let path = Self::default_path();
        Self::open_at(&path)
    }

    pub fn open_at(path: &Path) -> Result<Self> {
        let is_new = !path.exists();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("no pude crear {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("no pude abrir la base de datos {}", path.display()))?;
        init_schema(&conn)?;
        migrate_name_nullable(&conn)?;
        restrict_permissions(path);

        if is_new {
            if let Some(toml_path) = find_legacy_toml() {
                migrate_from_toml(&conn, &toml_path)
                    .with_context(|| format!("migrando {}", toml_path.display()))?;
            }
        }

        upgrade_ipv6_script_paths(
            &conn,
            Path::new(STD_IPV6_DISABLE),
            Path::new(STD_IPV6_ENABLE),
        )?;

        Ok(Self {
            conn,
            source: path.to_path_buf(),
        })
    }

    /// Base de datos en memoria, sin migración ni archivo en disco (tests).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        init_schema(&conn)?;
        Ok(Self {
            conn,
            source: PathBuf::from(":memory:"),
        })
    }

    pub fn default_path() -> PathBuf {
        if let Ok(p) = env::var("VPN_TUI_DB") {
            return PathBuf::from(p);
        }
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("vpn-tui")
            .join("vpn-tui.db")
    }

    // --------------------------------------------------------------- general

    pub fn general(&self) -> Result<General> {
        let mut g = General::default();
        for (key, value) in self.all_settings()? {
            match key.as_str() {
                "ipv6_disable_script" if !value.is_empty() => {
                    g.ipv6_disable_script = Some(PathBuf::from(value))
                }
                "ipv6_enable_script" if !value.is_empty() => {
                    g.ipv6_enable_script = Some(PathBuf::from(value))
                }
                "poll_interval_ms" => g.poll_interval_ms = value.parse().unwrap_or(2000).max(250),
                "log_capacity" => g.log_capacity = value.parse().unwrap_or(2000).max(100),
                "ipv6_on_connect" => g.ipv6_on_connect = value == "true",
                _ => {}
            }
        }
        Ok(g)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn all_settings(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM settings")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // -------------------------------------------------------------- perfiles

    pub fn openvpn_profiles(&self) -> Result<Vec<OpenVpnProfile>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, config_path FROM openvpn_profiles ORDER BY id")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(OpenVpnProfile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    config: PathBuf::from(row.get::<_, String>(2)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn forti_profiles(&self) -> Result<Vec<FortiProfile>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, host, username, password, password_env, trusted_cert, extra_args
             FROM forti_profiles ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let extra_args_json: String = row.get(7)?;
                let extra_args: Vec<String> =
                    serde_json::from_str(&extra_args_json).unwrap_or_default();
                Ok(FortiProfile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    host: row.get(2)?,
                    username: row.get(3)?,
                    password: row.get(4)?,
                    password_env: row.get(5)?,
                    trusted_cert: row.get(6)?,
                    extra_args,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn insert_openvpn_profile(&self, name: Option<&str>, config: &Path) -> Result<i64> {
        let now = chrono::Local::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO openvpn_profiles (name, config_path, created_at) VALUES (?1, ?2, ?3)",
            params![name, config.to_string_lossy(), now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_openvpn_profile(&self, id: i64, name: Option<&str>, config: &Path) -> Result<()> {
        self.conn.execute(
            "UPDATE openvpn_profiles SET name = ?2, config_path = ?3 WHERE id = ?1",
            params![id, name, config.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn delete_openvpn_profile(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM openvpn_profiles WHERE id = ?1", params![id])?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_forti_profile(
        &self,
        name: Option<&str>,
        host: &str,
        username: &str,
        password: Option<&str>,
        password_env: Option<&str>,
        trusted_cert: Option<&str>,
        extra_args: &[String],
    ) -> Result<i64> {
        let now = chrono::Local::now().to_rfc3339();
        let extra_args_json = serde_json::to_string(extra_args)?;
        self.conn.execute(
            "INSERT INTO forti_profiles
                (name, host, username, password, password_env, trusted_cert, extra_args, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                name,
                host,
                username,
                password,
                password_env,
                trusted_cert,
                extra_args_json,
                now
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_forti_profile(
        &self,
        id: i64,
        name: Option<&str>,
        host: &str,
        username: &str,
        password: Option<&str>,
        password_env: Option<&str>,
        trusted_cert: Option<&str>,
        extra_args: &[String],
    ) -> Result<()> {
        let extra_args_json = serde_json::to_string(extra_args)?;
        self.conn.execute(
            "UPDATE forti_profiles SET name = ?2, host = ?3, username = ?4, password = ?5,
                password_env = ?6, trusted_cert = ?7, extra_args = ?8 WHERE id = ?1",
            params![
                id,
                name,
                host,
                username,
                password,
                password_env,
                trusted_cert,
                extra_args_json
            ],
        )?;
        Ok(())
    }

    pub fn delete_forti_profile(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM forti_profiles WHERE id = ?1", params![id])?;
        Ok(())
    }
}

/// Columnas de cada tabla de perfiles, en el orden en que se copian durante la
/// reconstrucción del esquema.
const OPENVPN_COLUMNS: &str = "id, name, config_path, created_at, last_used";
const FORTI_COLUMNS: &str = "id, name, host, username, password, password_env, \
                             trusted_cert, extra_args, created_at, last_used";

fn openvpn_table(name: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {name} (
            id INTEGER PRIMARY KEY,
            name TEXT,
            config_path TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_used TEXT
        );"
    )
}

fn forti_table(name: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {name} (
            id INTEGER PRIMARY KEY,
            name TEXT,
            host TEXT NOT NULL,
            username TEXT NOT NULL,
            password TEXT,
            password_env TEXT,
            trusted_cert TEXT,
            extra_args TEXT NOT NULL DEFAULT '[]',
            created_at TEXT NOT NULL,
            last_used TEXT
        );"
    )
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;
    conn.execute_batch(&openvpn_table("openvpn_profiles"))?;
    conn.execute_batch(&forti_table("forti_profiles"))?;
    Ok(())
}

/// El esquema original declaraba `name TEXT NOT NULL UNIQUE`. El nombre pasó a
/// ser opcional (la fila se muestra como `<TipoVPN> (<nombre>)`, y el tipo
/// solo basta cuando hay un perfil de ese tipo), así que hay que reconstruir
/// la tabla: SQLite no permite aflojar un `NOT NULL`/`UNIQUE` con `ALTER`.
fn migrate_name_nullable(conn: &Connection) -> Result<()> {
    for (table, columns, create) in [
        (
            "openvpn_profiles",
            OPENVPN_COLUMNS,
            openvpn_table as fn(&str) -> String,
        ),
        ("forti_profiles", FORTI_COLUMNS, forti_table),
    ] {
        if !name_is_required(conn, table)? {
            continue;
        }
        let tmp = format!("{table}_v2");
        let create_tmp = create(&tmp);
        conn.execute_batch(&format!(
            "BEGIN;
             {create_tmp}
             INSERT INTO {tmp} ({columns}) SELECT {columns} FROM {table};
             DROP TABLE {table};
             ALTER TABLE {tmp} RENAME TO {table};
             COMMIT;"
        ))
        .with_context(|| format!("migrando el esquema de {table}"))?;
    }
    Ok(())
}

/// ¿La columna `name` de `table` sigue declarada `NOT NULL`?
///
/// Se consulta `PRAGMA table_info` en vez de buscar texto en el `CREATE TABLE`:
/// "username TEXT NOT NULL" contiene la subcadena "name TEXT NOT NULL", así
/// que un `contains` daría un falso positivo eterno en `forti_profiles`.
fn name_is_required(conn: &Connection, table: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let column: String = row.get(1)?;
        if column == "name" {
            let notnull: i64 = row.get(3)?;
            return Ok(notnull != 0);
        }
    }
    Ok(false)
}

/// Los scripts de IPv6 vivían sueltos en el home del usuario; el instalador
/// ahora los deja en `/usr/local/libexec/vpn/` (root:root, para que la regla
/// `NOPASSWD` de sudoers no apunte a un archivo que el usuario pueda editar).
/// Si la configuración guardada todavía apunta a los viejos y los nuevos ya
/// existen, se actualiza sola.
fn upgrade_ipv6_script_paths(conn: &Connection, disable: &Path, enable: &Path) -> Result<()> {
    for (key, legacy_name, new_path) in [
        ("ipv6_disable_script", LEGACY_IPV6_DISABLE, disable),
        ("ipv6_enable_script", LEGACY_IPV6_ENABLE, enable),
    ] {
        if !new_path.is_file() {
            continue;
        }
        use rusqlite::OptionalExtension;
        let current: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        let is_legacy = current
            .as_deref()
            .map(|v| {
                Path::new(v).file_name().and_then(|n| n.to_str()) == Some(legacy_name)
                    && Path::new(v) != new_path
            })
            .unwrap_or(false);
        if !is_legacy {
            continue;
        }
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, new_path.to_string_lossy()],
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

// --------------------------------------------------------- migración legacy

/// Forma mínima del `config.toml` viejo, solo para leerlo una vez y migrarlo.
#[derive(Debug, Deserialize)]
struct LegacyConfig {
    #[serde(default)]
    general: LegacyGeneral,
    #[serde(default, rename = "openvpn")]
    openvpn: Vec<LegacyOpenVpn>,
    #[serde(default, rename = "fortivpn")]
    fortivpn: Vec<LegacyForti>,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyGeneral {
    ipv6_script: Option<PathBuf>,
    poll_interval_ms: Option<u64>,
    log_capacity: Option<usize>,
    #[serde(default)]
    ipv6_on_connect: bool,
}

#[derive(Debug, Deserialize)]
struct LegacyOpenVpn {
    name: String,
    config: PathBuf,
}

#[derive(Debug, Deserialize)]
struct LegacyForti {
    name: String,
    host: String,
    username: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    password_env: Option<String>,
    #[serde(default)]
    trusted_cert: Option<String>,
    #[serde(default)]
    extra_args: Vec<String>,
}

/// Misma búsqueda que hacía `Config::find()`: `$VPN_TUI_CONFIG`, el
/// directorio del binario (subiendo hasta la raíz del proyecto), `./config.toml`
/// y `~/.config/vpn-tui/config.toml`.
fn find_legacy_toml() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = env::var("VPN_TUI_CONFIG") {
        candidates.push(PathBuf::from(p));
    }
    if let Ok(exe) = env::current_exe() {
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
    candidates.push(
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("vpn-tui")
            .join("config.toml"),
    );
    candidates.into_iter().find(|p| p.is_file())
}

fn migrate_from_toml(conn: &Connection, toml_path: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(toml_path)
        .with_context(|| format!("no pude leer {}", toml_path.display()))?;
    let cfg: LegacyConfig = toml::from_str(&raw)
        .with_context(|| format!("{} no es un TOML válido", toml_path.display()))?;
    let base = toml_path.parent().unwrap_or(Path::new("."));

    let now = chrono::Local::now().to_rfc3339();

    if let Some(script) = &cfg.general.ipv6_script {
        let resolved = resolve(script, base);
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('ipv6_disable_script', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![resolved.to_string_lossy()],
        )?;
        // El script viejo solo cubría deshabilitar IPv6; si en el mismo
        // directorio hay un `habilitar_ipv6.sh` (convención de este
        // proyecto), se toma como su contraparte para que el toggle
        // funcione en ambas direcciones sin que el usuario tenga que
        // configurarlo a mano.
        if let Some(dir) = resolved.parent() {
            let enable = dir.join("habilitar_ipv6.sh");
            if enable.is_file() {
                conn.execute(
                    "INSERT INTO settings (key, value) VALUES ('ipv6_enable_script', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![enable.to_string_lossy()],
                )?;
            }
        }
    }
    if let Some(ms) = cfg.general.poll_interval_ms {
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('poll_interval_ms', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![ms.to_string()],
        )?;
    }
    if let Some(cap) = cfg.general.log_capacity {
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('log_capacity', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![cap.to_string()],
        )?;
    }
    conn.execute(
        "INSERT INTO settings (key, value) VALUES ('ipv6_on_connect', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![cfg.general.ipv6_on_connect.to_string()],
    )?;

    // El `name` ya no es UNIQUE en el esquema (pasó a ser opcional), así que
    // la idempotencia de la migración se garantiza aquí con un `NOT EXISTS`
    // en vez de depender del índice único.
    for p in &cfg.openvpn {
        let resolved = resolve(&p.config, base);
        conn.execute(
            "INSERT INTO openvpn_profiles (name, config_path, created_at)
             SELECT ?1, ?2, ?3
             WHERE NOT EXISTS (SELECT 1 FROM openvpn_profiles WHERE config_path = ?2)",
            params![p.name, resolved.to_string_lossy(), now],
        )?;
    }
    for p in &cfg.fortivpn {
        let extra_args_json = serde_json::to_string(&p.extra_args)?;
        conn.execute(
            "INSERT INTO forti_profiles
                (name, host, username, password, password_env, trusted_cert, extra_args, created_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
             WHERE NOT EXISTS (
                 SELECT 1 FROM forti_profiles WHERE host = ?2 AND username = ?3
             )",
            params![
                p.name,
                p.host,
                p.username,
                p.password,
                p.password_env,
                p.trusted_cert,
                extra_args_json,
                now
            ],
        )?;
    }
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_toml(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn abre_en_memoria_sin_perfiles() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.openvpn_profiles().unwrap().is_empty());
        assert!(db.forti_profiles().unwrap().is_empty());
        assert_eq!(db.general().unwrap().poll_interval_ms, 2000);
    }

    #[test]
    fn migra_desde_config_toml_una_sola_vez() {
        let dir = tempdir();
        let toml_path = write_toml(
            &dir,
            r#"
[general]
poll_interval_ms = 500
log_capacity = 300
ipv6_on_connect = true

[[openvpn]]
name = "thinkpad"
config = "thinkpad.ovpn"

[[fortivpn]]
name = "corporativa"
host = "vpn.example.com:10443"
username = "usuario"
password = "secreto"
trusted_cert = "abc123"
"#,
        );

        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        migrate_from_toml(&conn, &toml_path).unwrap();

        let db = Db {
            conn,
            source: PathBuf::from(":memory:"),
        };

        let ovpn = db.openvpn_profiles().unwrap();
        assert_eq!(ovpn.len(), 1);
        assert_eq!(ovpn[0].name.as_deref(), Some("thinkpad"));
        assert_eq!(ovpn[0].config, dir.join("thinkpad.ovpn"));

        let forti = db.forti_profiles().unwrap();
        assert_eq!(forti.len(), 1);
        assert_eq!(forti[0].name.as_deref(), Some("corporativa"));
        assert_eq!(forti[0].resolve_password().unwrap(), "secreto");

        let general = db.general().unwrap();
        assert_eq!(general.poll_interval_ms, 500);
        assert_eq!(general.log_capacity, 300);
        assert!(general.ipv6_on_connect);

        // Migrar de nuevo (p. ej. si se reintentara) no debe duplicar filas:
        // INSERT OR IGNORE respeta el UNIQUE(name).
        migrate_from_toml(&db.conn, &toml_path).unwrap();
        assert_eq!(db.openvpn_profiles().unwrap().len(), 1);
        assert_eq!(db.forti_profiles().unwrap().len(), 1);
    }

    #[test]
    fn migracion_detecta_script_de_habilitar_ipv6_hermano() {
        let dir = tempdir();
        std::fs::write(dir.join("ipv6_deshabilitar.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("habilitar_ipv6.sh"), "#!/bin/sh\n").unwrap();
        let toml_path = write_toml(
            &dir,
            &format!(
                "[general]\nipv6_script = \"{}\"\n",
                dir.join("ipv6_deshabilitar.sh").display()
            ),
        );

        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        migrate_from_toml(&conn, &toml_path).unwrap();
        let db = Db {
            conn,
            source: PathBuf::from(":memory:"),
        };

        let general = db.general().unwrap();
        assert_eq!(general.ipv6_disable_script, Some(dir.join("ipv6_deshabilitar.sh")));
        assert_eq!(general.ipv6_enable_script, Some(dir.join("habilitar_ipv6.sh")));
    }

    #[test]
    fn open_at_no_migra_si_la_db_ya_existia() {
        let dir = tempdir();
        let db_path = dir.join("vpn-tui.db");
        write_toml(
            &dir,
            r#"
[[openvpn]]
name = "thinkpad"
config = "thinkpad.ovpn"
"#,
        );
        // set VPN_TUI_CONFIG to make find_legacy_toml() deterministic in test.
        std::env::set_var("VPN_TUI_CONFIG", dir.join("config.toml"));

        // Primera apertura: no existe la DB -> migra.
        {
            let db = Db::open_at(&db_path).unwrap();
            assert_eq!(db.openvpn_profiles().unwrap().len(), 1);
        }
        // Segunda apertura: la DB ya existe -> no vuelve a tocar el TOML
        // (aunque lo hiciera, INSERT OR IGNORE no duplicaría, pero lo
        // importante es que open_at ni siquiera intenta migrar).
        {
            let db = Db::open_at(&db_path).unwrap();
            assert_eq!(db.openvpn_profiles().unwrap().len(), 1);
        }
        std::env::remove_var("VPN_TUI_CONFIG");
    }

    #[test]
    fn crud_perfiles() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .insert_openvpn_profile(Some("thinkpad"), Path::new("/x/thinkpad.ovpn"))
            .unwrap();
        assert_eq!(db.openvpn_profiles().unwrap().len(), 1);
        db.update_openvpn_profile(id, Some("thinkpad2"), Path::new("/x/thinkpad2.ovpn"))
            .unwrap();
        assert_eq!(
            db.openvpn_profiles().unwrap()[0].name.as_deref(),
            Some("thinkpad2")
        );
        db.delete_openvpn_profile(id).unwrap();
        assert!(db.openvpn_profiles().unwrap().is_empty());

        let fid = db
            .insert_forti_profile(
                Some("corp"),
                "vpn.example.com:443",
                "user",
                Some("pass"),
                None,
                None,
                &[],
            )
            .unwrap();
        assert_eq!(db.forti_profiles().unwrap().len(), 1);
        db.delete_forti_profile(fid).unwrap();
        assert!(db.forti_profiles().unwrap().is_empty());
    }

    /// El nombre es opcional: se puede guardar un perfil sin nombre, y varios
    /// sin nombre no chocan (ya no hay `UNIQUE`).
    #[test]
    fn permite_perfiles_sin_nombre() {
        let db = Db::open_in_memory().unwrap();
        db.insert_openvpn_profile(None, Path::new("/x/a.ovpn"))
            .unwrap();
        db.insert_openvpn_profile(None, Path::new("/x/b.ovpn"))
            .unwrap();
        let rows = db.openvpn_profiles().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|p| p.name.is_none()));
    }

    /// Una base de datos creada con el esquema viejo (`name TEXT NOT NULL
    /// UNIQUE`) se reconstruye conservando las filas.
    #[test]
    fn migra_esquema_viejo_a_nombre_opcional() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE openvpn_profiles (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                config_path TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_used TEXT
            );
            CREATE TABLE forti_profiles (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                host TEXT NOT NULL,
                username TEXT NOT NULL,
                password TEXT,
                password_env TEXT,
                trusted_cert TEXT,
                extra_args TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL,
                last_used TEXT
            );
            INSERT INTO openvpn_profiles (name, config_path, created_at)
                VALUES ('thinkpad', '/x/thinkpad.ovpn', 'ayer');
            INSERT INTO forti_profiles (name, host, username, created_at)
                VALUES ('corp', 'h:443', 'u', 'ayer');",
        )
        .unwrap();

        assert!(name_is_required(&conn, "openvpn_profiles").unwrap());
        init_schema(&conn).unwrap();
        migrate_name_nullable(&conn).unwrap();
        assert!(!name_is_required(&conn, "openvpn_profiles").unwrap());
        assert!(!name_is_required(&conn, "forti_profiles").unwrap());

        let db = Db {
            conn,
            source: PathBuf::from(":memory:"),
        };
        let ovpn = db.openvpn_profiles().unwrap();
        assert_eq!(ovpn.len(), 1, "la migración no debe perder filas");
        assert_eq!(ovpn[0].name.as_deref(), Some("thinkpad"));
        assert_eq!(db.forti_profiles().unwrap().len(), 1);

        // Y ahora sí acepta nombres nulos.
        db.insert_openvpn_profile(None, Path::new("/x/otro.ovpn"))
            .unwrap();
        assert_eq!(db.openvpn_profiles().unwrap().len(), 2);

        // Correr la migración otra vez no hace nada (ya está al día).
        migrate_name_nullable(&db.conn).unwrap();
        assert_eq!(db.openvpn_profiles().unwrap().len(), 2);
    }

    #[test]
    fn actualiza_rutas_ipv6_legadas_a_las_estandar() {
        let dir = tempdir();
        let new_disable = dir.join("ipv6-disable.sh");
        let new_enable = dir.join("ipv6-enable.sh");
        std::fs::write(&new_disable, "#!/bin/sh\n").unwrap();
        std::fs::write(&new_enable, "#!/bin/sh\n").unwrap();

        let db = Db::open_in_memory().unwrap();
        db.set_setting("ipv6_disable_script", "/home/isaac/apps/vpns/ipv6_deshabilitar.sh")
            .unwrap();
        db.set_setting("ipv6_enable_script", "/home/isaac/apps/vpns/habilitar_ipv6.sh")
            .unwrap();

        upgrade_ipv6_script_paths(&db.conn, &new_disable, &new_enable).unwrap();

        let general = db.general().unwrap();
        assert_eq!(general.ipv6_disable_script, Some(new_disable));
        assert_eq!(general.ipv6_enable_script, Some(new_enable));
    }

    /// Si el usuario configuró un script propio (otro nombre), no se lo
    /// pisamos; y si los estándar todavía no existen, tampoco se toca nada.
    #[test]
    fn no_pisa_scripts_ipv6_personalizados_ni_inexistentes() {
        let dir = tempdir();
        let new_disable = dir.join("ipv6-disable.sh");
        let new_enable = dir.join("ipv6-enable.sh");
        std::fs::write(&new_disable, "#!/bin/sh\n").unwrap();
        std::fs::write(&new_enable, "#!/bin/sh\n").unwrap();

        let db = Db::open_in_memory().unwrap();
        db.set_setting("ipv6_disable_script", "/opt/mio/apaga-ipv6.sh")
            .unwrap();
        upgrade_ipv6_script_paths(&db.conn, &new_disable, &new_enable).unwrap();
        assert_eq!(
            db.general().unwrap().ipv6_disable_script,
            Some(PathBuf::from("/opt/mio/apaga-ipv6.sh"))
        );

        let db2 = Db::open_in_memory().unwrap();
        let legacy = "/home/isaac/apps/vpns/ipv6_deshabilitar.sh";
        db2.set_setting("ipv6_disable_script", legacy).unwrap();
        upgrade_ipv6_script_paths(
            &db2.conn,
            &dir.join("no-existe.sh"),
            &dir.join("tampoco.sh"),
        )
        .unwrap();
        assert_eq!(
            db2.general().unwrap().ipv6_disable_script,
            Some(PathBuf::from(legacy))
        );
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vpn-tui-test-{}", std::process::id()))
            .join(format!("{:?}", std::time::Instant::now()).replace([':', '.', ' '], "_"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
