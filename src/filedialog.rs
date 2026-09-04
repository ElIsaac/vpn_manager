//! Explorador de archivos *gráfico* del sistema (zenity / kdialog / yad).
//!
//! El explorador que trae la TUI (`ratatui-explorer`, tecla F3) sirve, pero
//! quien está en un escritorio espera el diálogo de siempre: el del sistema.
//! Aquí no se habla el protocolo del portal XDG a mano — zenity ya pasa por él
//! en Wayland, y hacerlo por D-Bus obligaría a suscribirse a la señal de
//! respuesta para nada.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;

use crate::app::normalize_dropped_path;
use crate::event::Msg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiPicker {
    Zenity,
    Kdialog,
    Yad,
}

/// Orden de preferencia: zenity es el más extendido y el que mejor se integra
/// con el portal; kdialog es el nativo en KDE; yad queda de última red.
const CANDIDATES: [GuiPicker; 3] = [GuiPicker::Zenity, GuiPicker::Kdialog, GuiPicker::Yad];

impl GuiPicker {
    pub fn bin(self) -> &'static str {
        match self {
            GuiPicker::Zenity => "zenity",
            GuiPicker::Kdialog => "kdialog",
            GuiPicker::Yad => "yad",
        }
    }

    /// Argumentos para pedir *un* archivo, filtrando `.ovpn` pero dejando ver
    /// el resto (hay perfiles que llegan con otra extensión).
    pub fn args(self, start: Option<&Path>) -> Vec<String> {
        let title = "Elige tu archivo .ovpn";
        match self {
            GuiPicker::Zenity | GuiPicker::Yad => {
                let mut args = vec![
                    match self {
                        GuiPicker::Yad => "--file".to_string(),
                        _ => "--file-selection".to_string(),
                    },
                    format!("--title={title}"),
                    "--file-filter=Perfiles OpenVPN | *.ovpn *.conf".to_string(),
                    "--file-filter=Todos los archivos | *".to_string(),
                ];
                if let Some(dir) = start {
                    // La barra final es lo que le dice al diálogo "ábrete
                    // *dentro* de esta carpeta" en vez de preseleccionarla.
                    args.push(format!("--filename={}/", dir.display()));
                }
                args
            }
            GuiPicker::Kdialog => vec![
                "--getopenfilename".to_string(),
                start
                    .map(|d| d.display().to_string())
                    .unwrap_or_else(|| "~".to_string()),
                "*.ovpn *.conf|Perfiles OpenVPN".to_string(),
                format!("--title={title}"),
            ],
        }
    }
}

/// ¿Hay sesión gráfica a la que mandar una ventana? Por SSH sin display no la
/// hay, y lanzar zenity ahí solo produce un error feo.
pub fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()))
}

/// Separado de [`detect`] para poder probar la elección sin depender de qué
/// haya instalado en la máquina donde corren los tests.
pub fn detect_with(has_display: bool, exists: impl Fn(&str) -> bool) -> Option<GuiPicker> {
    if !has_display {
        return None;
    }
    CANDIDATES.into_iter().find(|p| exists(p.bin()))
}

pub fn detect() -> Option<GuiPicker> {
    detect_with(has_display(), crate::vpn::has_command)
}

/// Por qué no se ofrece el explorador gráfico, dicho como se lo diríamos al
/// usuario en pantalla.
pub fn unavailable_reason() -> &'static str {
    if has_display() {
        "no encuentro zenity, kdialog ni yad"
    } else {
        "sin sesión gráfica (¿estás por SSH?)"
    }
}

/// Interpreta la salida del diálogo. Cancelar = salida no-cero o vacío, y eso
/// no es un error: es que el usuario cambió de idea.
pub fn parse_output(success: bool, stdout: &str) -> Option<PathBuf> {
    if !success {
        return None;
    }
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    // Algunos diálogos devuelven URI en vez de ruta; `normalize_dropped_path`
    // ya sabe de `file://`, comillas y percent-encoding.
    normalize_dropped_path(line).or_else(|| Some(PathBuf::from(line)))
}

/// Lanza el diálogo en un hilo: el bucle de render no puede quedarse esperando
/// a que el usuario elija (o se olvide de la ventana abierta).
pub fn open_async(picker: GuiPicker, start: Option<PathBuf>, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let output = Command::new(picker.bin())
            .args(picker.args(start.as_deref()))
            .output();
        let msg = match output {
            Ok(out) => Msg::GuiPickerDone {
                path: parse_output(out.status.success(), &String::from_utf8_lossy(&out.stdout)),
                err: None,
            },
            Err(err) => Msg::GuiPickerDone {
                path: None,
                err: Some(format!("no pude abrir {}: {err}", picker.bin())),
            },
        };
        let _ = tx.send(msg);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sin_sesion_grafica_no_hay_explorador_del_sistema() {
        assert_eq!(detect_with(false, |_| true), None);
    }

    #[test]
    fn elige_la_primera_herramienta_disponible() {
        let todas = detect_with(true, |_| true);
        assert_eq!(todas, Some(GuiPicker::Zenity), "zenity manda si está");

        let sin_zenity = detect_with(true, |b| b != "zenity");
        assert_eq!(sin_zenity, Some(GuiPicker::Kdialog));

        let solo_yad = detect_with(true, |b| b == "yad");
        assert_eq!(solo_yad, Some(GuiPicker::Yad));

        assert_eq!(detect_with(true, |_| false), None);
    }

    #[test]
    fn los_argumentos_filtran_ovpn_y_respetan_el_directorio_inicial() {
        let args = GuiPicker::Zenity.args(Some(Path::new("/home/user/vpns")));
        assert!(args.iter().any(|a| a == "--file-selection"));
        assert!(args.iter().any(|a| a.contains("*.ovpn")));
        assert!(args.iter().any(|a| a.contains("Todos los archivos")));
        assert!(args.iter().any(|a| a == "--filename=/home/user/vpns/"));

        // Sin directorio inicial no se inventa uno.
        let args = GuiPicker::Zenity.args(None);
        assert!(!args.iter().any(|a| a.starts_with("--filename=")));

        // kdialog usa otra sintaxis: posicional + filtro "patrón|etiqueta".
        let args = GuiPicker::Kdialog.args(None);
        assert_eq!(args[0], "--getopenfilename");
        assert!(args.iter().any(|a| a.starts_with("*.ovpn")));

        assert!(GuiPicker::Yad.args(None).iter().any(|a| a == "--file"));
    }

    #[test]
    fn la_salida_del_dialogo_se_normaliza() {
        assert_eq!(
            parse_output(true, "/home/user/vpns/thinkpad.ovpn\n"),
            Some(PathBuf::from("/home/user/vpns/thinkpad.ovpn"))
        );
        // Algunos devuelven URI.
        assert_eq!(
            parse_output(true, "file:///x/a.ovpn\n"),
            Some(PathBuf::from("/x/a.ovpn"))
        );
    }

    #[test]
    fn cancelar_el_dialogo_no_es_un_error() {
        assert_eq!(parse_output(false, ""), None);
        // Cancelar con salida cero pero sin ruta (yad en algunas versiones).
        assert_eq!(parse_output(true, "\n"), None);
        assert_eq!(parse_output(true, ""), None);
    }
}
