//! Dibujado de la interfaz.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, FrameExt as _, List, ListItem, ListState, Padding,
    Paragraph, Sparkline, Wrap,
};
use ratatui::Frame;

use crate::app::{App, ConfigScreen, Focus, Kind, LogEntry, Screen, TailscaleScreen};
use crate::event::Level;
use crate::vpn::{human_bytes, human_duration, State};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

/// Alto reservado para el panel de detalle + gráficas.
const DETAIL_HEIGHT: u16 = 11;

pub fn draw(frame: &mut Frame, app: &mut App) {
    match app.screen {
        Screen::Dashboard => draw_dashboard(frame, app),
        Screen::Tailscale(screen) => draw_tailscale(frame, app, screen),
        // La configuración y el asistente son modales: el dashboard se sigue
        // viendo detrás para no perder de vista el estado de las conexiones.
        Screen::Config(screen) => {
            draw_dashboard(frame, app);
            draw_config(frame, app, screen);
        }
        Screen::Setup(kind) => {
            draw_dashboard(frame, app);
            draw_setup(frame, app, kind);
        }
    }

    if app.show_help {
        draw_help(frame, frame.area());
    }
}

/// Marco común de todos los modales: recorta el fondo y devuelve el área
/// interior ya dividida en cuerpo + pie de teclas.
fn modal(frame: &mut Frame, title: &str, width_pct: u16, height_pct: u16) -> (Rect, Rect) {
    let area = frame.area();
    let width = (area.width * width_pct / 100).clamp(30, area.width.saturating_sub(2));
    let height = (area.height * height_pct / 100).clamp(8, area.height.saturating_sub(2));
    let popup = center(area, width, height);

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(format!(" {title} "), Style::new().fg(ACCENT).bold()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [body, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(inner);
    (body, footer)
}

// ----------------------------------------------------------------- dashboard

fn draw_dashboard(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);

    let sidebar = sidebar_width(body.width);
    let [left, right] =
        Layout::horizontal([Constraint::Length(sidebar), Constraint::Min(20)]).areas(body);

    // La columna izquierda es la lista de VPNs con el botón de configuración
    // debajo: es un panel más del recorrido de ←/→, no una tecla escondida.
    let [list_area, config_button] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(left);
    draw_profiles(frame, app, list_area);
    draw_config_button(frame, app, config_button);

    let detail_height = DETAIL_HEIGHT.min(right.height.saturating_sub(4));
    let [detail, logs] =
        Layout::vertical([Constraint::Length(detail_height), Constraint::Min(3)]).areas(right);
    let [info, graphs] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Min(24)]).areas(detail);

    draw_details(frame, app, info);
    draw_graphs(frame, app, graphs);
    draw_logs(frame, app, logs);
    draw_dashboard_footer(frame, app, footer);
}

/// La barra lateral se adapta al ancho disponible.
fn sidebar_width(total: u16) -> u16 {
    (total / 3)
        .clamp(22, 34)
        .min(total.saturating_sub(20).max(22))
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let (up, total) = app.summary();
    let status_color = if up == 0 {
        MUTED
    } else if up == total {
        Color::Green
    } else {
        Color::Yellow
    };

    let mut spans = vec![
        Span::styled(" VPNS ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw("  "),
        Span::styled(
            format!("{up}/{total} activas"),
            Style::new().fg(status_color).bold(),
        ),
    ];

    match app.ipv6_disabled {
        Some(true) => spans.push(Span::styled("   IPv6 off", Style::new().fg(Color::Green))),
        Some(false) => spans.push(Span::styled("   IPv6 on", Style::new().fg(MUTED))),
        None => {}
    }

    let mut missing: Vec<&str> = Vec::new();
    if !app.openvpn_profiles.is_empty() && !app.openvpn_available {
        missing.push("openvpn3");
    }
    if !app.forti_profiles.is_empty() && !app.fortivpn_available {
        missing.push("openfortivpn");
    }
    if !app.tailscale_available {
        missing.push("tailscale");
    }
    if !missing.is_empty() {
        spans.push(Span::styled(
            format!("   ⚠ falta: {}", missing.join(", ")),
            Style::new().fg(Color::Red),
        ));
    }

    let ifaces = app
        .interfaces
        .iter()
        .map(|i| format!("{} {}", i.name, i.addr))
        .collect::<Vec<_>>()
        .join("  ");
    if !ifaces.is_empty() {
        spans.push(Span::styled(format!("   {ifaces}"), Style::new().fg(MUTED)));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Line::from(vec![Span::styled(
            " VPNS ",
            Style::new().fg(ACCENT).bold(),
        )]))
        .title_bottom(
            Line::from(Span::styled(
                format!(" {} ", chrono::Local::now().format("%H:%M:%S")),
                Style::new().fg(MUTED),
            ))
            .right_aligned(),
        );

    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn draw_profiles(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Profiles;
    let ticks = app.ticks;
    let visible = app.visible_profile_indices();

    let items: Vec<ListItem> = visible
        .iter()
        .map(|&i| &app.profiles[i])
        .map(|p| {
            // Una fila "No Configurada" no tiene estado que mostrar: se ve
            // apagada y explica que enter lleva a configurarla.
            if p.placeholder {
                let line = Line::from(vec![
                    Span::styled("＋ ", Style::new().fg(MUTED)),
                    Span::styled(p.name.clone(), Style::new().fg(MUTED).bold()),
                ]);
                let sub = Line::from(vec![
                    Span::raw("      "),
                    Span::styled("enter para configurar", Style::new().fg(MUTED)),
                ]);
                return ListItem::new(Text::from(vec![line, sub]));
            }

            let (icon, color) = state_style(&p.state, ticks);
            let time = p
                .elapsed()
                .filter(|_| p.state.is_up())
                .map(human_duration)
                .unwrap_or_default();
            let line = Line::from(vec![
                Span::styled(format!("{icon} "), Style::new().fg(color)),
                Span::styled(p.name.clone(), Style::new().fg(Color::White).bold()),
            ]);
            let sub = Line::from(vec![
                Span::raw("      "),
                Span::styled(
                    if time.is_empty() {
                        p.state.label().to_string()
                    } else {
                        format!("{} · {time}", p.state.label())
                    },
                    Style::new().fg(color),
                ),
            ]);
            ListItem::new(Text::from(vec![line, sub]))
        })
        .collect();

    let block = panel("VPNS", focused).padding(Padding::new(1, 1, 0, 0));
    let list = List::new(items).block(block).highlight_style(
        Style::new()
            .bg(if focused {
                Color::Indexed(236)
            } else {
                Color::Reset
            })
            .add_modifier(Modifier::BOLD),
    );

    let mut state = ListState::default();
    if !visible.is_empty() {
        state.select(Some(app.selected.min(visible.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_config_button(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::ConfigButton;
    let style = if focused {
        Style::new().fg(Color::Black).bg(ACCENT).bold()
    } else {
        Style::new().fg(MUTED)
    };
    let label = Line::from(vec![Span::styled(
        if focused {
            " ⚙  configuración  ↵ "
        } else {
            " ⚙  configuración "
        },
        style,
    )])
    .centered();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { ACCENT } else { MUTED }));
    frame.render_widget(Paragraph::new(label).block(block), area);
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("detalle", false).padding(Padding::new(1, 1, 0, 0));
    let Some(p) = app.selected_profile() else {
        frame.render_widget(
            Paragraph::new("sin perfiles configurados — agrega uno en el menú de config")
                .block(block),
            area,
        );
        return;
    };

    if p.placeholder {
        let lines = vec![
            field("tipo", Span::raw(p.kind.label())),
            field(
                "estado",
                Span::styled("no configurada", Style::new().fg(MUTED).bold()),
            ),
            Line::from(""),
            Line::from(Span::styled(
                "  enter abre el asistente para configurar este tipo de VPN",
                Style::new().fg(MUTED),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    let (_, color) = state_style(&p.state, app.ticks);
    let kind = p.kind.label();

    let mut lines = vec![
        field("tipo", Span::raw(kind)),
        field(
            "destino",
            Span::styled(p.target.clone(), Style::new().fg(ACCENT)),
        ),
        field(
            "estado",
            Span::styled(
                match p.elapsed() {
                    Some(d) => format!("{} · {}", p.state.label(), human_duration(d)),
                    None => p.state.label().to_string(),
                },
                Style::new().fg(color).bold(),
            ),
        ),
    ];

    if p.kind == Kind::Tailscale {
        if let Some(status) = &app.tailscale_status {
            lines.push(field(
                "exit-node",
                Span::raw(status.exit_node_ip.clone().unwrap_or_else(|| "ninguno".into())),
            ));
            lines.push(field(
                "rutas anunciadas",
                Span::raw(if status.advertise_routes.is_empty() {
                    "ninguna".to_string()
                } else {
                    status.advertise_routes.join(", ")
                }),
            ));
            if !status.advertise_routes.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  recuerda: el admin del tailnet debe aprobar las rutas anunciadas",
                    Style::new().fg(Color::Yellow),
                )));
            }
        }
        lines.push(Line::from(Span::styled(
            "  'e' exit-node   'R' rutas anunciadas",
            Style::new().fg(MUTED),
        )));
    } else {
        if let Some(session) = &p.session_path {
            lines.push(field(
                "sesión",
                Span::styled(short_session(session), Style::new().fg(MUTED)),
            ));
        }
        if let Some(pid) = p.pid {
            lines.push(field(
                "pid",
                Span::styled(pid.to_string(), Style::new().fg(MUTED)),
            ));
        }
        if p.device.is_some() || p.addr.is_some() {
            let dev = p.device.clone().unwrap_or_else(|| "-".into());
            let addr = p.addr.clone().unwrap_or_else(|| "-".into());
            lines.push(field("interfaz", Span::raw(format!("{dev}  {addr}"))));
        }
        if p.state.is_up() {
            lines.push(field(
                "total",
                Span::raw(format!(
                    "↓ {}   ↑ {}",
                    human_bytes(p.stats.bytes_in),
                    human_bytes(p.stats.bytes_out)
                )),
            ));
            lines.push(field("velocidad", Span::raw(p.rate_summary())));
        }
    }
    if p.adopted {
        lines.push(Line::from(Span::styled(
            "  conexión externa — no se cerrará al salir",
            Style::new().fg(Color::Yellow),
        )));
    }
    if let Some(err) = &p.last_error {
        lines.push(Line::from(vec![
            Span::styled("  error  ", Style::new().fg(Color::Red).bold()),
            Span::styled(err.clone(), Style::new().fg(Color::Red)),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_graphs(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("tráfico", false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(p) = app.selected_profile() else {
        return;
    };
    if inner.height < 4 {
        return;
    }

    let [rx, tx] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);

    spark(frame, rx, "↓ rx", &p.rate_in, Color::Green);
    spark(frame, tx, "↑ tx", &p.rate_out, Color::Magenta);
}

fn spark(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    data: &std::collections::VecDeque<u64>,
    color: Color,
) {
    let width = area.width as usize;
    let values: Vec<u64> = data.iter().rev().take(width).rev().copied().collect();
    let current = values.last().copied().unwrap_or(0);
    let widget = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::NONE)
                .title(Line::from(vec![
                    Span::styled(format!("{label} "), Style::new().fg(MUTED)),
                    Span::styled(
                        format!("{}/s", human_bytes(current)),
                        Style::new().fg(color),
                    ),
                ])),
        )
        .data(values)
        .style(Style::new().fg(color));
    frame.render_widget(widget, area);
}

fn draw_logs(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Logs;
    let viewport = area.height.saturating_sub(2) as usize;
    app.log_viewport = viewport;

    // Cada perfil ve solo su propio registro; las entradas sin perfil (avisos
    // globales: guardar config, script de IPv6, arranque…) se muestran
    // siempre, sin importar la selección.
    let selected_name = app.selected_profile().map(|p| p.name.clone());
    let filtered: Vec<&LogEntry> = app
        .logs
        .iter()
        .filter(|e| e.profile.is_none() || e.profile == selected_name)
        .collect();

    let total = filtered.len();
    let max_offset = total.saturating_sub(viewport);
    let offset = app.log_scroll.unwrap_or(max_offset).min(max_offset);

    let lines: Vec<Line> = filtered
        .iter()
        .skip(offset)
        .take(viewport.max(1))
        .map(|entry| {
            let mut spans = vec![Span::styled(
                entry.at.format("%H:%M:%S ").to_string(),
                Style::new().fg(MUTED),
            )];
            if let Some(name) = &entry.profile {
                spans.push(Span::styled(format!("[{name}] "), Style::new().fg(ACCENT)));
            }
            spans.push(Span::styled(entry.text.clone(), level_style(entry.level)));
            Line::from(spans)
        })
        .collect();

    let title = if app.log_scroll.is_some() {
        format!(
            "registro  {}/{}  (fijado)",
            offset + viewport.min(total),
            total
        )
    } else {
        format!("registro  {total}")
    };

    frame.render_widget(Paragraph::new(lines).block(panel(&title, focused)), area);
}

fn draw_dashboard_footer(frame: &mut Frame, app: &App, area: Rect) {
    if let Some((level, text)) = app.active_toast() {
        let style = level_style(level);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ● ", style),
                Span::styled(text.to_string(), style),
            ])),
            area,
        );
        return;
    }

    let keys = [
        ("↑↓", "mover"),
        ("←→", "panel"),
        ("1/2/3", "saltar a tipo"),
        ("enter", "conectar/cortar"),
        ("a", "todas"),
        ("x", "cortar todas"),
        ("⚙", "config (←→ hasta el botón)"),
        ("?", "ayuda"),
        ("esc/q", "salir"),
    ];
    render_key_hints(frame, area, &keys);
}

fn render_key_hints(frame: &mut Frame, area: Rect, keys: &[(&str, &str)]) {
    let mut spans = Vec::new();
    for (key, desc) in keys {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().fg(Color::Black).bg(MUTED),
        ));
        spans.push(Span::styled(format!(" {desc}  "), Style::new().fg(MUTED)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let rows: [(&str, &str); 16] = [
        ("← / →", "cambiar de panel (VPNS, registro, ⚙ configuración)"),
        ("↑ / k, ↓ / j", "moverse en listas / desplazar el registro"),
        ("1 / 2 / 3", "saltar selección a OpenVPN3 / OpenFortiVPN / Tailscale"),
        ("enter / space", "conectar, desconectar, confirmar, abrir ⚙"),
        ("c / d", "conectar / desconectar el seleccionado"),
        ("a", "conectar todos"),
        ("x", "desconectar todos"),
        ("e", "exit-node de Tailscale (con Tailscale seleccionado)"),
        ("R", "rutas anunciadas de Tailscale (con Tailscale seleccionado)"),
        ("n / e / x", "nuevo / editar / borrar perfil (listas de config)"),
        ("F3", "elegir archivo (zona de importación)"),
        ("tab / shift-tab", "siguiente / anterior campo (dentro de un formulario)"),
        ("pgup / pgdn, g / G", "desplazar el registro"),
        ("r", "limpiar el registro"),
        ("? / F1", "abrir o cerrar esta ayuda"),
        ("q / esc / ctrl-c", "atrás / salir"),
    ];

    let width = 62u16.min(area.width.saturating_sub(4));
    let height = (rows.len() as u16 + 4).min(area.height.saturating_sub(2));
    let popup = center(area, width, height);

    frame.render_widget(Clear, popup);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!("  {key:<20}"), Style::new().fg(ACCENT).bold()),
                Span::raw(*desc),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(" atajos ", Style::new().fg(ACCENT).bold()))
        .padding(Padding::new(0, 0, 1, 0));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

// --------------------------------------------------------------- tailscale

fn draw_tailscale(frame: &mut Frame, app: &App, screen: TailscaleScreen) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let title = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(" Tailscale ", Style::new().fg(ACCENT).bold()));
    frame.render_widget(Paragraph::new("").block(title), header);

    match screen {
        TailscaleScreen::ExitNodePicker => draw_exit_node_picker(frame, app, body),
        TailscaleScreen::RoutesForm => draw_routes_form(frame, app, body),
    }

    let keys: &[(&str, &str)] = match screen {
        TailscaleScreen::ExitNodePicker => &[
            ("↑↓", "mover"),
            ("enter", "elegir"),
            ("c", "quitar exit-node"),
            ("esc", "cancelar"),
        ],
        TailscaleScreen::RoutesForm => &[("enter", "guardar"), ("esc", "cancelar")],
    };
    render_key_hints(frame, footer, keys);
}

fn draw_exit_node_picker(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("elegir exit-node", true).padding(Padding::new(1, 1, 0, 0));
    let candidates = app
        .tailscale_status
        .as_ref()
        .map(|s| s.exit_node_candidates.as_slice())
        .unwrap_or(&[]);

    if candidates.is_empty() {
        frame.render_widget(
            Paragraph::new("no hay peers que ofrezcan exit-node").block(block),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = candidates
        .iter()
        .map(|c| {
            let marker = if c.is_current { "● " } else { "○ " };
            let color = if c.online { Color::White } else { MUTED };
            Line::from(vec![
                Span::styled(marker, Style::new().fg(if c.is_current { Color::Green } else { MUTED })),
                Span::styled(c.host_name.clone(), Style::new().fg(color).bold()),
                Span::raw("  "),
                Span::styled(
                    c.tailscale_ip.clone().unwrap_or_default(),
                    Style::new().fg(MUTED),
                ),
            ])
            .into()
        })
        .collect();

    let list = List::new(items).block(block).highlight_style(
        Style::new()
            .bg(Color::Indexed(236))
            .add_modifier(Modifier::BOLD),
    );
    let mut state = ListState::default();
    state.select(Some(app.exit_node_selected.min(candidates.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_routes_form(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("rutas a anunciar (CIDR separados por coma)", true)
        .padding(Padding::new(1, 1, 1, 0));
    let value = app.routes_input.value();
    frame.render_widget(
        Paragraph::new(if value.is_empty() { "(vacío = no anunciar nada)" } else { value }),
        block.inner(area),
    );
    frame.render_widget(block, area);
}

// ------------------------------------------------------------------- config

fn draw_config(frame: &mut Frame, app: &mut App, screen: ConfigScreen) {
    match screen {
        ConfigScreen::Menu => draw_config_menu(frame, app),
        ConfigScreen::ProfileList(kind) => draw_profile_list(frame, app, kind),
        ConfigScreen::ProfileForm => draw_profile_form(frame, app),
        ConfigScreen::FilePicker => draw_file_picker(frame, app),
        ConfigScreen::General => draw_general(frame, app),
        ConfigScreen::Ipv6Status => draw_ipv6_status(frame, app),
        ConfigScreen::OpenVpnSessions => draw_sessions(frame, app),
        ConfigScreen::ConfirmDelete { kind, idx } => draw_confirm_delete(frame, app, kind, idx),
    }
}

/// Asistente de primera configuración: un carrusel con un pane por tipo. El
/// indicador (‹ ● ○ ○ ›) es explícito para que se lea como deslizable y no
/// como una pantalla fija.
fn draw_setup(frame: &mut Frame, app: &App, kind: Kind) {
    let (body, footer) = modal(frame, "configurar VPN", 70, 60);

    let [tabs, content] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(body);

    let mut spans = vec![Span::styled("  ‹  ", Style::new().fg(ACCENT).bold())];
    for k in Kind::ALL {
        let current = k == kind;
        spans.push(Span::styled(
            if current { "● " } else { "○ " },
            Style::new().fg(if current { ACCENT } else { MUTED }),
        ));
        spans.push(Span::styled(
            format!("{}  ", k.label()),
            if current {
                Style::new().fg(Color::White).bold()
            } else {
                Style::new().fg(MUTED)
            },
        ));
    }
    spans.push(Span::styled("  ›", Style::new().fg(ACCENT).bold()));
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(spans).centered(),
            Line::from(Span::styled(
                "usa ← → para ver los otros tipos de VPN",
                Style::new().fg(MUTED),
            ))
            .centered(),
        ]),
        tabs,
    );

    let configured = match kind {
        Kind::OpenVpn => !app.openvpn_profiles.is_empty(),
        Kind::Forti => !app.forti_profiles.is_empty(),
        Kind::Tailscale => app
            .tailscale_status
            .as_ref()
            .map(|s| s.is_running())
            .unwrap_or(false),
    };

    let lines = match kind {
        Kind::OpenVpn => vec![
            Line::from(Span::styled(
                "  Importa un archivo .ovpn para crear tu primer perfil.",
                Style::new().fg(Color::White),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  enter → abre el formulario con la zona de importación",
                Style::new().fg(MUTED),
            )),
        ],
        Kind::Forti => vec![
            Line::from(Span::styled(
                "  Captura host, usuario y contraseña de tu FortiGate.",
                Style::new().fg(Color::White),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  enter → abre el formulario de OpenFortiVPN",
                Style::new().fg(MUTED),
            )),
        ],
        Kind::Tailscale => vec![
            Line::from(Span::styled(
                "  Tailscale no usa perfiles: basta con levantarlo en este equipo.",
                Style::new().fg(Color::White),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  enter → tailscale up",
                Style::new().fg(MUTED),
            )),
        ],
    };
    let mut lines = lines;
    if configured {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ✓ este tipo ya está configurado",
            Style::new().fg(Color::Green),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), content);

    render_key_hints(
        frame,
        footer,
        &[("←→", "cambiar tipo"), ("enter", "configurar"), ("esc", "salir")],
    );
}

const CONFIG_MENU_ITEMS: [&str; 5] = [
    "Perfiles OpenVPN3",
    "Perfiles OpenFortiVPN",
    "General",
    "IPv6",
    "Sesiones OpenVPN3 (ver y matar)",
];

fn draw_config_menu(frame: &mut Frame, app: &App) {
    let (body, footer) = modal(frame, "configuración", 60, 60);

    let items: Vec<ListItem> = CONFIG_MENU_ITEMS
        .iter()
        .map(|s| ListItem::new(format!("  {s}")))
        .collect();
    let list = List::new(items).highlight_style(
        Style::new().bg(Color::Indexed(236)).add_modifier(Modifier::BOLD),
    );
    let mut state = ListState::default();
    state.select(Some(app.config_menu_selected));
    frame.render_stateful_widget(list, body, &mut state);

    render_key_hints(frame, footer, &[("↑↓", "mover"), ("enter", "entrar"), ("esc", "cerrar")]);
}

fn draw_profile_list(frame: &mut Frame, app: &App, kind: Kind) {
    let title = format!("perfiles {}", kind.label());
    let (body, footer) = modal(frame, &title, 70, 60);

    let items: Vec<ListItem> = match kind {
        Kind::OpenVpn => app
            .openvpn_profiles
            .iter()
            .map(|p| {
                ListItem::new(format!(
                    "  {}  {}",
                    crate::app::display_name(kind, p.name.as_deref()),
                    p.config.display()
                ))
            })
            .collect(),
        Kind::Forti => app
            .forti_profiles
            .iter()
            .map(|p| {
                ListItem::new(format!(
                    "  {}  {}@{}",
                    crate::app::display_name(kind, p.name.as_deref()),
                    p.username,
                    p.host
                ))
            })
            .collect(),
        // El menú de config nunca abre esta pantalla con Kind::Tailscale.
        Kind::Tailscale => Vec::new(),
    };
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("  sin perfiles — pulsa 'n' para agregar uno"),
            body,
        );
    } else {
        let list = List::new(items).highlight_style(
            Style::new().bg(Color::Indexed(236)).add_modifier(Modifier::BOLD),
        );
        let mut state = ListState::default();
        state.select(Some(app.profile_list_selected));
        frame.render_stateful_widget(list, body, &mut state);
    }

    render_key_hints(
        frame,
        footer,
        &[
            ("↑↓", "mover"),
            ("n", "nuevo"),
            ("e", "editar"),
            ("x", "borrar"),
            ("esc/q", "atrás"),
        ],
    );
}

fn draw_profile_form(frame: &mut Frame, app: &App) {
    let Some(form) = &app.profile_form else {
        return;
    };
    let title = match form.editing_id {
        Some(_) => format!("editar perfil {}", form.kind.label()),
        None => format!("nuevo perfil {}", form.kind.label()),
    };
    let (body, footer) = modal(frame, &title, 72, 70);

    // Los campos de ruta se dibujan como zona de importación (dropzone), con
    // su propio recuadro: es lo primero que busca el ojo al dar de alta un
    // .ovpn, y antes se perdía como una línea más del formulario.
    let dropzones = form.fields.iter().filter(|f| f.pickable).count() as u16;
    let text_rows = (form.fields.len() as u16 - dropzones) + 2;
    let [text_area, drop_area, _] = Layout::vertical([
        Constraint::Length(text_rows),
        Constraint::Length(if dropzones > 0 { 4 } else { 0 }),
        Constraint::Min(0),
    ])
    .areas(body);

    let mut lines: Vec<Line> = Vec::new();
    for (i, field) in form.fields.iter().enumerate() {
        if field.pickable {
            continue;
        }
        let focused = i == form.focus;
        let value = if field.mask {
            "•".repeat(field.input.value().chars().count())
        } else {
            field.input.value().to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(if focused { "▶ " } else { "  " }, Style::new().fg(ACCENT)),
            Span::styled(format!("{:<24}", field.label), Style::new().fg(MUTED)),
            Span::styled(
                value,
                if focused {
                    Style::new().fg(Color::White).bold()
                } else {
                    Style::new().fg(Color::White)
                },
            ),
        ]));
    }
    if let Some(err) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {err}"),
            Style::new().fg(Color::Red),
        )));
    }
    frame.render_widget(Paragraph::new(lines), text_area);

    if let Some((i, field)) = form.fields.iter().enumerate().find(|(_, f)| f.pickable) {
        draw_dropzone(frame, drop_area, field.label, field.input.value(), i == form.focus);
    }

    let pickable = form.fields.get(form.focus).map(|f| f.pickable).unwrap_or(false);
    let mut keys = vec![
        ("tab", "campo"),
        ("enter", "guardar"),
        ("esc", "cancelar"),
    ];
    if pickable {
        keys.insert(1, ("F3", "elegir archivo"));
    }
    render_key_hints(frame, footer, &keys);
}

/// Zona de importación de archivo: recuadro propio, ícono y una instrucción
/// explícita. Sin esto el campo de ruta no se leía como "aquí va un archivo".
fn draw_dropzone(frame: &mut Frame, area: Rect, label: &str, value: &str, focused: bool) {
    if area.height < 3 {
        return;
    }
    let border = if focused { ACCENT } else { MUTED };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(border))
        .title(Span::styled(
            format!(" ⤓ {label} "),
            Style::new().fg(border).bold(),
        ))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = if value.is_empty() {
        vec![
            Line::from(Span::styled(
                "suelta aquí tu archivo .ovpn",
                Style::new().fg(if focused { Color::White } else { MUTED }).bold(),
            ))
            .centered(),
            Line::from(Span::styled(
                "o pulsa F3 para buscarlo en el disco",
                Style::new().fg(MUTED),
            ))
            .centered(),
        ]
    } else {
        vec![
            Line::from(Span::styled(value.to_string(), Style::new().fg(Color::White)))
                .centered(),
            Line::from(Span::styled(
                "F3 para elegir otro",
                Style::new().fg(MUTED),
            ))
            .centered(),
        ]
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_file_picker(frame: &mut Frame, app: &App) {
    let (body, footer) = modal(frame, "elegir archivo .ovpn", 76, 80);
    if let Some(explorer) = &app.file_explorer {
        frame.render_widget_ref(explorer.widget(), body);
    }
    render_key_hints(frame, footer, &[("↑↓", "mover"), ("enter", "abrir/elegir"), ("esc", "cancelar")]);
}

fn draw_general(frame: &mut Frame, app: &App) {
    let (inner, footer) = modal(frame, "general", 76, 50);

    let Some(form) = &app.general_form else {
        return;
    };

    let row = |i: usize, label: &str, value: String| {
        let focused = i == form.focus;
        Line::from(vec![
            Span::styled(if focused { "▶ " } else { "  " }, Style::new().fg(ACCENT)),
            Span::styled(format!("{label:<28}"), Style::new().fg(MUTED)),
            Span::styled(value, if focused { Style::new().fg(Color::White).bold() } else { Style::new().fg(Color::White) }),
        ])
    };

    let lines = vec![
        row(0, "poll_interval_ms", form.poll_interval_ms.value().to_string()),
        row(1, "log_capacity", form.log_capacity.value().to_string()),
        row(2, "ipv6_disable_script (F3)", form.ipv6_disable_script.value().to_string()),
        row(3, "ipv6_enable_script (F3)", form.ipv6_enable_script.value().to_string()),
        row(4, "ipv6_on_connect (space)", form.ipv6_on_connect.to_string()),
    ];
    frame.render_widget(Paragraph::new(lines), inner);

    render_key_hints(
        frame,
        footer,
        &[("tab", "campo"), ("F3", "buscar archivo"), ("enter", "guardar"), ("esc", "cancelar")],
    );
}

fn draw_ipv6_status(frame: &mut Frame, app: &App) {
    let (body, footer) = modal(frame, "ipv6", 66, 35);

    let (label, color) = match app.ipv6_disabled {
        Some(true) => ("deshabilitado", Color::Green),
        Some(false) => ("habilitado", Color::Yellow),
        None => ("desconocido", MUTED),
    };
    let lines = vec![
        field("estado actual", Span::styled(label, Style::new().fg(color).bold())),
        Line::from(""),
        Line::from(Span::styled(
            "  't' alterna: si está habilitado corre el script de deshabilitar, y viceversa.",
            Style::new().fg(MUTED),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), body);

    render_key_hints(frame, footer, &[("t", "alternar"), ("esc/q", "atrás")]);
}

fn draw_sessions(frame: &mut Frame, app: &App) {
    let (body, footer) = modal(frame, "sesiones openvpn3 (todas, no solo las de esta TUI)", 80, 60);
    let spinner = SPINNER[(app.ticks as usize) % SPINNER.len()];

    if app.all_openvpn_sessions.is_empty() {
        frame.render_widget(Paragraph::new("  no hay sesiones activas"), body);
    } else {
        let items: Vec<ListItem> = app
            .all_openvpn_sessions
            .iter()
            .map(|s| {
                // Mientras se está cerrando (una o todas) la fila lo dice con
                // el mismo spinner que usan los perfiles al desconectarse.
                let killing = app.killing_all || app.killing_sessions.contains(&s.path);
                if killing {
                    return ListItem::new(Line::from(vec![
                        Span::styled(format!("  {spinner} "), Style::new().fg(Color::Yellow)),
                        Span::styled(short_session(&s.path), Style::new().fg(Color::Yellow)),
                        Span::styled("  cerrando…", Style::new().fg(Color::Yellow).bold()),
                    ]));
                }
                ListItem::new(format!(
                    "  ● {}  {}  {}",
                    short_session(&s.path),
                    s.device.clone().unwrap_or_else(|| "-".into()),
                    s.status
                ))
            })
            .collect();
        let list = List::new(items).highlight_style(
            Style::new().bg(Color::Indexed(236)).add_modifier(Modifier::BOLD),
        );
        let mut state = ListState::default();
        state.select(Some(app.sessions_selected.min(app.all_openvpn_sessions.len() - 1)));
        frame.render_stateful_widget(list, body, &mut state);
    }

    if app.sessions_busy() {
        let text = if app.killing_all {
            format!(" {spinner} cerrando todas las sesiones…")
        } else {
            format!(" {spinner} cerrando la sesión…")
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::new().fg(Color::Yellow).bold(),
            ))),
            footer,
        );
        return;
    }

    render_key_hints(
        frame,
        footer,
        &[("↑↓", "mover"), ("d", "matar seleccionada"), ("a", "matar todas"), ("esc/n", "cancelar")],
    );
}

fn draw_confirm_delete(frame: &mut Frame, app: &App, kind: Kind, idx: usize) {
    let (body, footer) = modal(frame, "confirmar borrado", 66, 40);
    let warning = app
        .delete_warning(kind, idx)
        .unwrap_or_else(|| "¿Borrar este perfil?".to_string());

    let mut lines = vec![Line::from(Span::styled(
        "  ⚠ esta acción no se puede deshacer",
        Style::new().fg(Color::Yellow).bold(),
    ))];
    lines.push(Line::from(""));
    for l in warning.lines() {
        lines.push(Line::from(Span::raw(format!("  {l}"))));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), body);

    render_key_hints(frame, footer, &[("s/enter", "borrar"), ("n/esc", "cancelar")]);
}

// ------------------------------------------------------------------ utilidades

fn panel(title: &str, focused: bool) -> Block<'static> {
    let border = if focused { ACCENT } else { MUTED };
    let style = if focused {
        Style::new().fg(ACCENT).bold()
    } else {
        Style::new().fg(MUTED)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(Span::styled(format!(" {title} "), style))
}

fn field<'a>(label: &'a str, value: Span<'a>) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:>10}  "), Style::new().fg(MUTED)),
        value,
    ])
}

fn state_style(state: &State, ticks: u64) -> (String, Color) {
    match state {
        State::Connected => ("●".to_string(), Color::Green),
        State::Disconnected => ("○".to_string(), MUTED),
        State::Connecting | State::Disconnecting => (
            SPINNER[(ticks as usize) % SPINNER.len()].to_string(),
            Color::Yellow,
        ),
        State::Failed(_) => ("✖".to_string(), Color::Red),
    }
}

fn level_style(level: Level) -> Style {
    match level {
        Level::Info => Style::new().fg(Color::Gray),
        Level::Ok => Style::new().fg(Color::Green),
        Level::Warn => Style::new().fg(Color::Yellow),
        Level::Error => Style::new().fg(Color::Red),
    }
}

/// `/net/openvpn/v3/sessions/abc…` → `…/abc…`, para que quepa en el panel.
fn short_session(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((_, id)) if id.len() > 12 => format!("…{}", &id[id.len() - 12..]),
        Some((_, id)) => id.to_string(),
        None => path.to_string(),
    }
}

fn center(area: Rect, width: u16, height: u16) -> Rect {
    let [_, mid, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, popup, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width),
        Constraint::Fill(1),
    ])
    .areas(mid);
    popup
}
