use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::backend::{MsgState, Trust};

const PEER_PALETTE: [Color; 8] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::Blue,
    Color::LightCyan,
    Color::LightGreen,
    Color::LightMagenta,
];

pub fn peer_color(peer_id: &str) -> Color {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in peer_id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    PEER_PALETTE[(h % 8) as usize]
}

pub fn unread_badge(n: u32) -> Option<Span<'static>> {
    (n > 0).then(|| {
        Span::styled(
            format!(" {n}"),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    })
}

pub fn trust_glyph(trust: Trust) -> Span<'static> {
    match trust {
        Trust::Verified => Span::styled("●", Style::default().fg(Color::Green)),
        Trust::Unverified => Span::styled("○", Style::default().fg(Color::DarkGray)),
        Trust::KeyChanged => Span::styled(
            "✕",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    }
}

pub fn tick(state: MsgState) -> &'static str {
    match state {
        MsgState::Queued => "…",
        MsgState::Sent => "✓",
        MsgState::Delivered => "✓✓",
        MsgState::Failed => "!",
    }
}

pub fn read_marker_line(width: usize) -> Line<'static> {
    let label = " new ";
    let side = width.saturating_sub(label.len()) / 2;
    let line = format!(
        "{}{}{}",
        "─".repeat(side),
        label,
        "─".repeat(width.saturating_sub(side + label.len()))
    );
    Line::styled(line, Style::default().fg(Color::DarkGray))
}

pub fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(v);
    h
}

pub fn help_lines() -> Vec<Line<'static>> {
    let rows: &[(&str, &str)] = &[
        ("Enter", "send (or run /command)"),
        ("C-g", "fuzzy chat switcher"),
        ("M-1..9", "jump to chat"),
        ("M-a", "next unread chat"),
        ("M-k", "compose in $EDITOR"),
        ("PgUp/PgDn", "scroll history"),
        ("Esc", "close overlay"),
        ("C-c", "quit"),
        ("", ""),
        ("/add <link>", "add contact by cinder:// URI"),
        ("/verify [sas]", "mark contact verified"),
        ("/finger", "show peer fingerprint + SAS"),
        ("/clear", "clear draft"),
        ("/status", "connection info"),
        ("/next", "next unread"),
        ("/quit", "quit"),
    ];
    rows.iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(
                    format!("{k:<13}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(*d),
            ])
        })
        .collect()
}

pub fn fuzzy_lines(
    query: &str,
    titles: &[String],
    matches: &[usize],
    sel: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            query.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ])];
    for (row, &idx) in matches.iter().enumerate() {
        let style = if row == sel {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::styled(
            titles.get(idx).cloned().unwrap_or_default(),
            style,
        ));
    }
    lines
}
