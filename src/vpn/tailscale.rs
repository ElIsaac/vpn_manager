//! Envoltorio sobre el CLI de `tailscale` (no `tailscaled`, que corre como
//! servicio del sistema y no se toca desde aquí).

use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::Value;

use super::{run, run_checked};

const BIN: &str = "tailscale";

/// Un peer candidato a exit-node (`ExitNodeOption: true` en `status --json`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExitNodeCandidate {
    pub host_name: String,
    pub dns_name: String,
    pub tailscale_ip: Option<String>,
    pub online: bool,
    pub is_current: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub backend_state: String,
    pub self_ips: Vec<String>,
    /// IP del exit-node en uso, leída de `tailscale debug prefs` (`ExitNodeIP`).
    pub exit_node_ip: Option<String>,
    /// Rutas que este equipo anuncia (`AdvertiseRoutes` en prefs). Anunciar no
    /// implica que ya estén aprobadas por el admin del tailnet.
    pub advertise_routes: Vec<String>,
    pub exit_node_candidates: Vec<ExitNodeCandidate>,
}

impl Status {
    pub fn is_running(&self) -> bool {
        self.backend_state == "Running"
    }
}

/// `true` si el binario `tailscale` existe en el `PATH`.
pub fn is_available() -> bool {
    super::has_command(BIN)
}

/// Consulta `tailscale status --json` + `tailscale debug prefs` y arma un
/// [`Status`] combinado. Ninguno de los dos requiere privilegios elevados.
pub fn status() -> Result<Status> {
    let out = run(BIN, &["status", "--json"], Duration::from_secs(10))?;
    if !out.status.success() {
        bail!("`tailscale status` falló");
    }
    let status_json: Value = serde_json::from_slice(&out.stdout)?;

    let prefs_json = run(BIN, &["debug", "prefs"], Duration::from_secs(10))
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
        .unwrap_or(Value::Null);

    Ok(parse_status(&status_json, &prefs_json))
}

fn parse_status(status: &Value, prefs: &Value) -> Status {
    let backend_state = status
        .get("BackendState")
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_string();

    let self_ips = str_array(status.get("TailscaleIPs"));

    let exit_node_ip = prefs
        .get("ExitNodeIP")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let advertise_routes = str_array(prefs.get("AdvertiseRoutes"));

    let mut exit_node_candidates = Vec::new();
    if let Some(peers) = status.get("Peer").and_then(Value::as_object) {
        for peer in peers.values() {
            let option = peer
                .get("ExitNodeOption")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !option {
                continue;
            }
            let host_name = peer
                .get("HostName")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let dns_name = peer
                .get("DNSName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_end_matches('.')
                .to_string();
            let tailscale_ip = peer
                .get("TailscaleIPs")
                .and_then(Value::as_array)
                .and_then(|a| a.iter().find_map(Value::as_str))
                .map(str::to_string);
            let online = peer.get("Online").and_then(Value::as_bool).unwrap_or(false);
            let is_current = exit_node_ip.is_some() && tailscale_ip == exit_node_ip;
            exit_node_candidates.push(ExitNodeCandidate {
                host_name,
                dns_name,
                tailscale_ip,
                online,
                is_current,
            });
        }
    }
    exit_node_candidates.sort_by(|a, b| a.host_name.cmp(&b.host_name));

    Status {
        backend_state,
        self_ips,
        exit_node_ip,
        advertise_routes,
        exit_node_candidates,
    }
}

fn str_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn up() -> Result<()> {
    run_privileged(&["up"])
}

pub fn down() -> Result<()> {
    run_privileged(&["down"])
}

/// `node = None` quita el exit-node actual (`--exit-node=`).
pub fn set_exit_node(node: Option<&str>) -> Result<()> {
    let flag = format!("--exit-node={}", node.unwrap_or(""));
    run_privileged(&["set", &flag])
}

/// Rutas vacías equivale a dejar de anunciar cualquier subnet route.
pub fn set_advertise_routes(routes: &[String]) -> Result<()> {
    let flag = format!("--advertise-routes={}", routes.join(","));
    run_privileged(&["set", &flag])
}

/// Corre `tailscale <args>`; si falla por falta de privilegios (según cómo
/// esté configurado `--operator` en este equipo), reintenta con `sudo -n`
/// para no dejar la TUI colgada pidiendo una contraseña.
fn run_privileged(args: &[&str]) -> Result<()> {
    if !is_available() {
        bail!("`{BIN}` no está instalado");
    }
    match run_checked(BIN, args, Duration::from_secs(20)) {
        Ok(_) => Ok(()),
        Err(err) => {
            let msg = err.to_string().to_ascii_lowercase();
            let needs_privilege = ["permission", "access denied", "must be root", "operator"]
                .iter()
                .any(|kw| msg.contains(kw));
            if needs_privilege {
                let mut sudo_args = vec!["-n", BIN];
                sudo_args.extend_from_slice(args);
                run_checked("sudo", &sudo_args, Duration::from_secs(20))?;
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture real (recortado) capturado en esta máquina con
    // `tailscale status --json`, con dos peers: uno candidato a exit-node
    // (ExitNodeOption: true) y otro que no.
    const STATUS_JSON: &str = r#"
    {
      "BackendState": "Running",
      "TailscaleIPs": ["100.111.47.60", "fd7a:115c:a1e0::3401:2fca"],
      "Self": {"HostName": "fedora", "ExitNodeOption": false},
      "Peer": {
        "nodekey:aaa": {
          "HostName": "jmra-devops",
          "DNSName": "jmra-devops.tailf3a539.ts.net.",
          "TailscaleIPs": ["100.124.96.77", "fd7a:115c:a1e0::7038:604e"],
          "Online": true,
          "ExitNodeOption": false
        },
        "nodekey:bbb": {
          "HostName": "exit-mx",
          "DNSName": "exit-mx.tailf3a539.ts.net.",
          "TailscaleIPs": ["100.90.1.2"],
          "Online": true,
          "ExitNodeOption": true
        }
      }
    }
    "#;

    const PREFS_JSON: &str = r#"
    {
      "ExitNodeID": "n123",
      "ExitNodeIP": "100.90.1.2",
      "AdvertiseRoutes": ["172.31.0.0/16"]
    }
    "#;

    #[test]
    fn parsea_estado_y_prefs() {
        let status: Value = serde_json::from_str(STATUS_JSON).unwrap();
        let prefs: Value = serde_json::from_str(PREFS_JSON).unwrap();
        let parsed = parse_status(&status, &prefs);

        assert_eq!(parsed.backend_state, "Running");
        assert!(parsed.is_running());
        assert_eq!(
            parsed.self_ips,
            vec!["100.111.47.60".to_string(), "fd7a:115c:a1e0::3401:2fca".to_string()]
        );
        assert_eq!(parsed.exit_node_ip.as_deref(), Some("100.90.1.2"));
        assert_eq!(parsed.advertise_routes, vec!["172.31.0.0/16".to_string()]);

        assert_eq!(parsed.exit_node_candidates.len(), 1);
        let cand = &parsed.exit_node_candidates[0];
        assert_eq!(cand.host_name, "exit-mx");
        assert_eq!(cand.dns_name, "exit-mx.tailf3a539.ts.net");
        assert_eq!(cand.tailscale_ip.as_deref(), Some("100.90.1.2"));
        assert!(cand.online);
        assert!(cand.is_current);
    }

    #[test]
    fn sin_exit_node_ninguno_queda_marcado_como_actual() {
        let status: Value = serde_json::from_str(STATUS_JSON).unwrap();
        let empty_prefs: Value = serde_json::from_str(r#"{"ExitNodeIP": "", "AdvertiseRoutes": null}"#).unwrap();
        let parsed = parse_status(&status, &empty_prefs);
        assert_eq!(parsed.exit_node_ip, None);
        assert!(parsed.advertise_routes.is_empty());
        assert!(!parsed.exit_node_candidates[0].is_current);
    }

    #[test]
    fn backend_state_ausente_cae_a_desconocido() {
        let status: Value = serde_json::from_str("{}").unwrap();
        let parsed = parse_status(&status, &Value::Null);
        assert_eq!(parsed.backend_state, "Unknown");
        assert!(!parsed.is_running());
        assert!(parsed.exit_node_candidates.is_empty());
    }
}
