use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::{App, Overlay};
use super::backend::{ConnState, Dir, Trust};
use super::sanitize::sanitize;
use super::widgets;

pub fn fmt_hhmm(ts: i64) -> String {
    let t = ts.rem_euclid(86_400);
    format!("{:02}:{:02}", t / 3600, t % 3600 / 60)
}

pub fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    for raw in s.split('\n') {
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for word in raw.split(' ') {
            let ww = UnicodeWidthStr::width(word);
            if cur_w > 0 && cur_w + 1 + ww > width {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            } else if cur_w > 0 {
                cur.push(' ');
                cur_w += 1;
            }
            if ww > width {
                for ch in word.chars() {
                    let cw = UnicodeWidthStr::width(ch.to_string().as_str());
                    if cur_w + cw > width {
                        lines.push(std::mem::take(&mut cur));
                        cur_w = 0;
                    }
                    cur.push(ch);
                    cur_w += cw;
                }
            } else {
                cur.push_str(word);
                cur_w += ww;
            }
        }
        lines.push(cur);
    }
    lines
}

fn chat_title(name: Option<&str>, peer_id: &str) -> String {
    match name {
        Some(n) if !n.is_empty() => sanitize(n),
        _ => peer_id
            .trim_start_matches("peer:")
            .chars()
            .take(10)
            .collect(),
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let [main, input_area, status_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);
    let [list_area, chat_area] =
        Layout::horizontal([Constraint::Length(26), Constraint::Min(20)]).areas(main);

    draw_chat_list(f, app, list_area);
    draw_chat(f, app, chat_area);
    draw_input(f, app, input_area);
    draw_status(f, app, status_area);

    match app.overlay.clone() {
        Some(Overlay::Fuzzy { query, sel }) => draw_fuzzy(f, app, &query, sel),
        Some(Overlay::Help) => {
            let rect = widgets::centered(46, 20, area);
            f.render_widget(Clear, rect);
            let block = Block::default()
                .title(" help ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            f.render_widget(
                Paragraph::new(Text::from(widgets::help_lines())).block(block),
                rect,
            );
        }
        Some(Overlay::Finger { lines }) => {
            let rect = widgets::centered(54, (lines.len() + 2) as u16, area);
            f.render_widget(Clear, rect);
            let block = Block::default()
                .title(" fingerprint ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            let text: Vec<Line> = lines.iter().map(|l| Line::raw(l.clone())).collect();
            f.render_widget(Paragraph::new(Text::from(text)).block(block), rect);
        }
        None => {}
    }
}

fn draw_chat_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .chats
        .iter()
        .map(|c| {
            let mut spans = vec![
                widgets::trust_glyph(c.trust),
                Span::raw(" "),
                Span::raw(chat_title(c.display_name.as_deref(), &c.peer_id)),
            ];
            if let Some(badge) =
                widgets::unread_badge(app.unread.get(&c.chat_id).copied().unwrap_or(0))
            {
                spans.push(badge);
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(app.active_idx());
    let list = List::new(items)
        .block(
            Block::default()
                .title(" chats ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_chat(f: &mut Frame, app: &mut App, area: Rect) {
    let inner_w = area.width.saturating_sub(2) as usize;
    app.chat_view_height = area.height.saturating_sub(2);

    let Some(chat) = app.active_chat() else {
        f.render_widget(
            Paragraph::new("no chat selected").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };
    let chat_id = chat.chat_id.clone();
    let title = format!(
        " {} {} {} ",
        chat_title(chat.display_name.as_deref(), &chat.peer_id),
        match chat.trust {
            Trust::Verified => "●",
            Trust::Unverified => "○",
            Trust::KeyChanged => "✕ key changed — /verify to re-trust",
        },
        chat.peer_id
            .trim_start_matches("peer:")
            .chars()
            .take(12)
            .collect::<String>(),
    );

    let mut lines: Vec<Line> = Vec::new();
    if let Some(buf) = app.buffers.get(&chat_id) {
        for (i, m) in buf.messages.iter().enumerate() {
            if buf.read_marker == Some(i) {
                lines.push(widgets::read_marker_line(inner_w));
            }
            let out = m.dir == Dir::Out;
            let name = if out {
                "you".to_string()
            } else {
                chat_title(chat.display_name.as_deref(), &m.from)
            };
            let body_w = inner_w.saturating_sub(8).max(1);
            let chunks = wrap_text(&sanitize(&m.text), body_w);
            let name_color = if out {
                Color::White
            } else {
                widgets::peer_color(&m.from)
            };
            for (ci, chunk) in chunks.iter().enumerate() {
                let mut spans = vec![Span::styled(
                    if ci == 0 {
                        format!("{} ", fmt_hhmm(m.ts))
                    } else {
                        " ".repeat(6)
                    },
                    Style::default().fg(Color::DarkGray),
                )];
                if ci == 0 {
                    spans.push(Span::styled(
                        format!("{name}: "),
                        Style::default().fg(name_color).add_modifier(Modifier::BOLD),
                    ));
                }
                let is_last = ci + 1 == chunks.len();
                if out && is_last {
                    let tick = widgets::tick(m.state);
                    let used = UnicodeWidthStr::width(chunk.as_str())
                        + 6
                        + if ci == 0 { name.len() + 2 } else { 0 };
                    let pad = inner_w.saturating_sub(used + tick.len());
                    spans.push(Span::raw(chunk.clone()));
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.push(Span::styled(
                        tick.to_string(),
                        Style::default().fg(Color::DarkGray),
                    ));
                } else {
                    spans.push(Span::raw(chunk.clone()));
                }
                lines.push(Line::from(spans));
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "  no messages yet",
            Style::default().fg(Color::DarkGray),
        ));
    }

    if let Some(buf) = app.buffers.get_mut(&chat_id) {
        buf.render_rows = lines.len();
        let max = lines.len().saturating_sub(app.chat_view_height as usize);
        buf.scroll = buf.scroll.min(max as u16);
        let top = lines
            .len()
            .saturating_sub(app.chat_view_height as usize + buf.scroll as usize)
            as u16;
        let para = Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(Wrap { trim: false })
            .scroll((top, 0));
        f.render_widget(para, area);
    } else {
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .block(
                    Block::default()
                        .title(title)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .wrap(Wrap { trim: false }),
            area,
        );
    }
}

fn draw_input(f: &mut Frame, app: &mut App, area: Rect) {
    app.input.set_block(
        Block::default()
            .title(" message ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(&app.input, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let (conn_glyph, conn_color, conn_text) = match app.conn {
        ConnState::Connected => ("●", Color::Green, "connected"),
        ConnState::Connecting => ("…", Color::Yellow, "connecting"),
        ConnState::Offline => ("○", Color::Red, "offline"),
    };
    let peer_short: String = app
        .peer_id
        .trim_start_matches("peer:")
        .chars()
        .take(10)
        .collect();
    let trust = app
        .active_chat()
        .map(|c| match c.trust {
            Trust::Verified => "verified",
            Trust::Unverified => "unverified",
            Trust::KeyChanged => "KEY CHANGED",
        })
        .unwrap_or("-");
    let left = Line::from(vec![
        Span::styled(format!(" {conn_glyph} "), Style::default().fg(conn_color)),
        Span::raw(conn_text),
        Span::raw("  "),
        Span::styled(peer_short, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(trust.to_string(), Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(left), area);
    let hint = app
        .status_line
        .clone()
        .unwrap_or_else(|| "/help · C-g switch · M-a unread · C-c quit".to_string());
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{hint} "),
            Style::default().fg(Color::DarkGray),
        )))
        .right_aligned(),
        area,
    );
}

fn draw_fuzzy(f: &mut Frame, app: &App, query: &str, sel: usize) {
    let titles: Vec<String> = app
        .chats
        .iter()
        .map(|c| {
            let u = app.unread.get(&c.chat_id).copied().unwrap_or(0);
            if u > 0 {
                format!(
                    "{} ({})",
                    chat_title(c.display_name.as_deref(), &c.peer_id),
                    u
                )
            } else {
                chat_title(c.display_name.as_deref(), &c.peer_id)
            }
        })
        .collect();
    let matches = app.fuzzy_matches(query);
    let sel = sel.min(matches.len().saturating_sub(1));
    let rect = widgets::centered(48, (matches.len().min(10) + 3) as u16, f.area());
    f.render_widget(Clear, rect);
    let lines = widgets::fuzzy_lines(query, &titles, &matches, sel);
    let block = Block::default()
        .title(" switch ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use crate::tui::backend::{ChatSummary, ConnState, Dir, Message, MsgState, Trust};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn buffer_string(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(c) = buf.cell((x, y)) {
                    s.push_str(c.symbol());
                }
            }
            s.push('\n');
        }
        s
    }

    fn app_with_evil() -> App {
        let mut a = App::new("peer:self".to_string(), ConnState::Connected);
        a.set_chats(vec![ChatSummary {
            chat_id: "chat:1".to_string(),
            contact_id: "contact:1".to_string(),
            peer_id: "peer:evil1".to_string(),
            display_name: Some("\x1b[2Jevil".to_string()),
            trust: Trust::Verified,
            unread: 0,
            last_activity: 100,
            last_message: None,
        }]);
        a.insert_history(
            "chat:1",
            crate::tui::backend::HistoryPage {
                messages: vec![Message {
                    msg_id: "msg:1".to_string(),
                    chat_id: "chat:1".to_string(),
                    dir: Dir::In,
                    from: "peer:evil1".to_string(),
                    text: "\x1b]8;;http://evil\x07click\x1b[2J\u{202e}me".to_string(),
                    state: MsgState::Delivered,
                    ts: 3600 * 14 + 3 * 60,
                }],
                has_more: false,
                next_before: None,
            },
        );
        a
    }

    #[test]
    fn render_sanitizes_peer_text() {
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut app = app_with_evil();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let s = buffer_string(&term);
        assert!(!s.contains('\x1b'), "raw ESC leaked into buffer: {s:?}");
        assert!(!s.contains('\u{202e}'));
        assert!(s.contains("click"), "sanitized text missing: {s:?}");
        assert!(s.contains("evil"), "display name missing: {s:?}");
    }

    #[test]
    fn render_shows_ticks_and_marker() {
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut a = App::new("peer:self".to_string(), ConnState::Connected);
        a.set_chats(vec![ChatSummary {
            chat_id: "chat:1".to_string(),
            contact_id: "contact:1".to_string(),
            peer_id: "peer:p1".to_string(),
            display_name: Some("alice".to_string()),
            trust: Trust::Unverified,
            unread: 1,
            last_activity: 100,
            last_message: None,
        }]);
        a.insert_history(
            "chat:1",
            crate::tui::backend::HistoryPage {
                messages: (0..5)
                    .map(|i| Message {
                        msg_id: format!("m{i}"),
                        chat_id: "chat:1".to_string(),
                        dir: if i % 2 == 0 { Dir::In } else { Dir::Out },
                        from: "peer:p1".to_string(),
                        text: format!("line {i}"),
                        state: MsgState::Delivered,
                        ts: 3600 * 14 + i as i64 * 60,
                    })
                    .collect(),
                has_more: false,
                next_before: None,
            },
        );
        a.activate("chat:1");
        term.draw(|f| draw(f, &mut a)).unwrap();
        let s = buffer_string(&term);
        assert!(s.contains("✓✓"), "delivered tick missing");
        assert!(s.contains("new"), "read marker missing: {s:?}");
        assert!(s.contains("14:0"), "timestamps missing: {s:?}");
    }

    #[test]
    fn wrap_basic() {
        assert_eq!(wrap_text("hello world", 20), vec!["hello world"]);
        assert_eq!(wrap_text("aaa bbb ccc", 4), vec!["aaa", "bbb", "ccc"]);
        assert_eq!(wrap_text("a\nb", 10), vec!["a", "b"]);
        assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }
}
