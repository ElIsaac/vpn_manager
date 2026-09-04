//! Dibujado de la interfaz.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Sparkline,
    Wrap,
};
use ratatui::Frame;

use crate::app::{App, Focus, Kind};
use crate::event::Level;
use crate::vpn::{human_bytes, human_duration, State};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

/// Alto reservado para el panel de detalle + gráficas.
const DETAIL_HEIGHT: u16 = 11;

pub fn draw(frame: &mut Frame, app: &mut App) {
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

    draw_profiles(frame, app, left);

    let detail_height = DETAIL_HEIGHT.min(right.height.saturating_sub(4));
    let [detail, logs] =
        Layout::vertical([Constraint::Length(detail_height), Constraint::Min(3)]).areas(right);
    let [info, graphs] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Min(24)]).areas(detail);

    draw_details(frame, app, info);
    draw_graphs(frame, app, graphs);
    draw_logs(frame, app, logs);
    draw_footer(frame, app, footer);

    if app.show_help {
        draw_help(frame, frame.area());
    }
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
        Span::styled(" VPN ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
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

    if !app.openvpn_available {
        spans.push(Span::styled(
            "   ⚠ openvpn3 ausente",
            Style::new().fg(Color::Red),
        ));
    }
    if !app.fortivpn_available {
        spans.push(Span::styled(
            "   ⚠ openfortivpn ausente",
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
            " vpn-tui ",
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

    let items: Vec<ListItem> = app
        .profiles
        .iter()
        .map(|p| {
            let (icon, color) = state_style(&p.state, ticks);
            let time = p
                .elapsed()
                .filter(|_| p.state.is_up())
                .map(human_duration)
                .unwrap_or_default();
            let line = Line::from(vec![
                Span::styled(format!("{icon} "), Style::new().fg(color)),
                Span::styled(format!("{:<5} ", p.kind.tag()), Style::new().fg(MUTED)),
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

    let block = panel("perfiles", focused).padding(Padding::new(1, 1, 0, 0));
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
    state.select(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("detalle", false).padding(Padding::new(1, 1, 0, 0));
    let Some(p) = app.selected_profile() else {
        frame.render_widget(
            Paragraph::new("sin perfiles configurados").block(block),
            area,
        );
        return;
    };

    let (_, color) = state_style(&p.state, app.ticks);
    let kind = match p.kind {
        Kind::OpenVpn => "OpenVPN3",
        Kind::Forti => "OpenFortiVPN",
    };

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
    let total = app.logs.len();
    let max_offset = total.saturating_sub(viewport);
    let offset = app.log_scroll.unwrap_or(max_offset).min(max_offset);

    let lines: Vec<Line> = app
        .logs
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

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
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
        ("↑↓/jk", "mover"),
        ("enter", "conectar/cortar"),
        ("a", "todas"),
        ("x", "cortar todas"),
        ("6", "ipv6"),
        ("tab", "panel"),
        ("?", "ayuda"),
        ("q", "salir"),
    ];
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
    let rows: [(&str, &str); 14] = [
        ("↑ / k", "perfil anterior"),
        ("↓ / j", "perfil siguiente"),
        ("enter / space", "conectar o desconectar el perfil"),
        ("c", "conectar el perfil"),
        ("d", "desconectar el perfil"),
        ("a", "conectar todos los perfiles"),
        ("x", "desconectar todos los perfiles"),
        ("6", "ejecutar el script de IPv6"),
        ("r", "limpiar el registro"),
        ("tab", "alternar entre perfiles y registro"),
        ("pgup / pgdn", "desplazar el registro"),
        ("g / G", "inicio / final del registro"),
        ("?", "abrir o cerrar esta ayuda"),
        ("q / esc / ctrl-c", "salir (cierra lo que abrió la TUI)"),
    ];

    let width = 60u16.min(area.width.saturating_sub(4));
    let height = (rows.len() as u16 + 4).min(area.height.saturating_sub(2));
    let popup = center(area, width, height);

    frame.render_widget(Clear, popup);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!("  {key:<18}"), Style::new().fg(ACCENT).bold()),
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
