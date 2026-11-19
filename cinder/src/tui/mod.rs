pub mod app;
pub mod backend;
pub mod keys;
pub mod sanitize;
pub mod ui;
pub mod widgets;

use std::io;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{App, Command, Overlay};
use backend::{Backend, Trust};
use keys::Action;
use sanitize::sanitize;

const HISTORY_PAGE: usize = 50;

pub async fn run(mut backend: impl Backend) -> Result<()> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;
    let result = event_loop(&mut terminal, &mut backend).await;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev(info);
    }));
}

async fn event_loop<B: Backend>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    backend: &mut B,
) -> Result<()> {
    let mut app = App::new(backend.peer_id().to_string(), backend.conn_state());
    let mut events = backend.subscribe();
    let mut keys = EventStream::new();

    app.set_chats(backend.list_chats().await?);
    ensure_history(backend, &mut app).await;

    while !app.quit {
        terminal.draw(|f| ui::draw(f, &mut app))?;
        tokio::select! {
            maybe_key = keys.next() => {
                match maybe_key {
                    Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(terminal, backend, &mut app, key).await?;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                }
            }
            maybe_event = events.recv() => {
                if let Some(ev) = maybe_event {
                    app.apply_event(ev);
                }
            }
        }
    }
    Ok(())
}

async fn ensure_history<B: Backend>(backend: &mut B, app: &mut App) {
    let Some(chat_id) = app.active_chat.clone() else {
        return;
    };
    let need = match app.buffer(&chat_id) {
        Some(b) => !b.loaded && !b.fetching,
        None => true,
    };
    if !need {
        return;
    }
    app.buffer_mut(&chat_id).fetching = true;
    match backend.history(&chat_id, None, HISTORY_PAGE).await {
        Ok(page) => app.insert_history(&chat_id, page),
        Err(e) => {
            app.buffer_mut(&chat_id).fetching = false;
            app.status_line = Some(format!("history: {e}"));
        }
    }
}

async fn fetch_older<B: Backend>(backend: &mut B, app: &mut App) {
    let Some(chat_id) = app.needs_older() else {
        return;
    };
    let cursor = app.buffer(&chat_id).and_then(|b| b.next_before.clone());
    app.buffer_mut(&chat_id).fetching = true;
    match backend
        .history(&chat_id, cursor.as_deref(), HISTORY_PAGE)
        .await
    {
        Ok(page) => app.insert_history(&chat_id, page),
        Err(e) => {
            app.buffer_mut(&chat_id).fetching = false;
            app.status_line = Some(format!("history: {e}"));
        }
    }
}

async fn handle_key<B: Backend>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    backend: &mut B,
    app: &mut App,
    key: crossterm::event::KeyEvent,
) -> Result<()> {
    match keys::handle_key(app, key) {
        Action::Quit => app.quit = true,
        Action::Noop => {}
        Action::Input(k) => {
            app.input.input(k);
        }
        Action::Submit => submit(backend, app).await?,
        Action::Jump(i) => {
            app.activate_index(i);
            ensure_history(backend, app).await;
        }
        Action::NextUnread => {
            app.next_unread();
            ensure_history(backend, app).await;
        }
        Action::OpenFuzzy => {
            app.overlay = Some(Overlay::Fuzzy {
                query: String::new(),
                sel: 0,
            });
        }
        Action::CloseOverlay => app.overlay = None,
        Action::FuzzyKey(k) => fuzzy_key(app, k),
        Action::FuzzyAccept => {
            if let Some(Overlay::Fuzzy { query, sel }) = app.overlay.take() {
                let matches = app.fuzzy_matches(&query);
                if let Some(&idx) = matches.get(sel.min(matches.len().saturating_sub(1))) {
                    app.activate_index(idx);
                    ensure_history(backend, app).await;
                }
            }
        }
        Action::Compose => compose_external(terminal, app)?,
        Action::PageUp => {
            app.page_scroll(true);
            fetch_older(backend, app).await;
        }
        Action::PageDown => app.page_scroll(false),
    }
    Ok(())
}

fn fuzzy_key(app: &mut App, key: crossterm::event::KeyEvent) {
    use crossterm::event::KeyCode;
    let count = match &app.overlay {
        Some(Overlay::Fuzzy { query, .. }) => app.fuzzy_matches(query).len(),
        _ => return,
    };
    let Some(Overlay::Fuzzy { query, sel }) = &mut app.overlay else {
        return;
    };
    match key.code {
        KeyCode::Char(c) => {
            query.push(c);
            *sel = 0;
        }
        KeyCode::Backspace => {
            query.pop();
            *sel = 0;
        }
        KeyCode::Up => *sel = sel.saturating_sub(1),
        KeyCode::Down => *sel = (*sel + 1).min(count.saturating_sub(1)),
        _ => {}
    }
}

async fn submit<B: Backend>(backend: &mut B, app: &mut App) -> Result<()> {
    let text = app.take_draft();
    if text.is_empty() {
        return Ok(());
    }
    if text.starts_with('/') {
        return run_command(backend, app, &text).await;
    }
    let Some(chat) = app.active_chat() else {
        app.status_line = Some("no chat selected".to_string());
        return Ok(());
    };
    if chat.trust == Trust::KeyChanged {
        app.status_line = Some("key changed — /verify before sending".to_string());
        return Ok(());
    }
    let chat_id = chat.chat_id.clone();
    match backend.send(&chat_id, &text).await {
        Ok(ack) => app.push_outgoing(
            &chat_id,
            &ack.msg_id,
            &ack.client_msg_id,
            &text,
            ack.expires_at,
        ),
        Err(e) => app.status_line = Some(format!("send: {e}")),
    }
    Ok(())
}

async fn run_command<B: Backend>(backend: &mut B, app: &mut App, line: &str) -> Result<()> {
    match app::parse_command(line) {
        Command::Help => app.overlay = Some(Overlay::Help),
        Command::Quit => app.quit = true,
        Command::Clear => {
            app.input = app::fresh_textarea();
        }
        Command::Next => {
            app.next_unread();
            ensure_history(backend, app).await;
        }
        Command::Status => {
            let n = app.chats.len();
            let unread: u32 = app.unread.values().sum();
            app.status_line = Some(format!(
                "{:?} · {} · {n} chats · {unread} unread",
                app.conn, app.peer_id
            ));
        }
        Command::Add(uri) => match backend.add_contact(&uri).await {
            Ok(id) => app.status_line = Some(format!("contact {id} connecting…")),
            Err(e) => app.status_line = Some(format!("add: {e}")),
        },
        Command::Verify(sas) => {
            let Some(chat) = app.active_chat() else {
                app.status_line = Some("no chat selected".to_string());
                return Ok(());
            };
            let contact_id = chat.contact_id.clone();
            match sas {
                Some(s) => match backend.verify_contact(&contact_id, &s).await {
                    Ok(()) => {
                        if let Some(c) = app.chats.iter_mut().find(|c| c.contact_id == contact_id) {
                            c.trust = Trust::Verified;
                        }
                        app.status_line = Some("verified".to_string());
                    }
                    Err(e) => app.status_line = Some(format!("verify: {e}")),
                },
                None => show_finger(backend, app, &contact_id).await,
            }
        }
        Command::Finger => {
            let Some(chat) = app.active_chat() else {
                app.status_line = Some("no chat selected".to_string());
                return Ok(());
            };
            let contact_id = chat.contact_id.clone();
            show_finger(backend, app, &contact_id).await;
        }
        Command::Ttl(secs) => {
            let Some(chat) = app.active_chat() else {
                app.status_line = Some("no chat selected".to_string());
                return Ok(());
            };
            let chat_id = chat.chat_id.clone();
            match backend
                .set_disappearing(&chat_id, secs.saturating_mul(1000))
                .await
            {
                Ok(()) => {
                    app.status_line = Some(if secs == 0 {
                        "disappearing messages off".to_string()
                    } else {
                        format!("disappearing after {secs}s (local default)")
                    });
                }
                Err(e) => app.status_line = Some(format!("ttl: {e}")),
            }
        }
        Command::SendFile(path) => {
            let Some(chat) = app.active_chat() else {
                app.status_line = Some("no chat selected".to_string());
                return Ok(());
            };
            if chat.trust == Trust::KeyChanged {
                app.status_line = Some("key changed — /verify before sending".to_string());
                return Ok(());
            }
            let chat_id = chat.chat_id.clone();
            match backend.send_file(&chat_id, &path).await {
                Ok(ack) => {
                    app.push_outgoing_file(&chat_id, &ack.msg_id, &ack.client_msg_id, &path);
                    app.status_line = Some("file offer queued".to_string());
                }
                Err(e) => app.status_line = Some(format!("send: {e}")),
            }
        }
        Command::Save(id) => {
            let Some(chat) = app.active_chat() else {
                app.status_line = Some("no chat selected".to_string());
                return Ok(());
            };
            let file_id = match id.or_else(|| app.latest_inbound_file(&chat.chat_id)) {
                Some(f) => f,
                None => {
                    app.status_line = Some("no file to save in this chat".to_string());
                    return Ok(());
                }
            };
            match backend.file_save(&file_id, None).await {
                Ok(path) => app.status_line = Some(format!("saved → {path}")),
                Err(e) => app.status_line = Some(format!("save: {e}")),
            }
        }
        Command::Cancel(id) => match backend.file_cancel(&id).await {
            Ok(()) => app.status_line = Some("transfer cancelled".to_string()),
            Err(e) => app.status_line = Some(format!("cancel: {e}")),
        },
        Command::Accept(id) => file_decision(backend, app, id, true).await,
        Command::Decline(id) => file_decision(backend, app, id, false).await,
        Command::Files => {
            let Some(chat) = app.active_chat() else {
                app.status_line = Some("no chat selected".to_string());
                return Ok(());
            };
            app.overlay = Some(Overlay::Files {
                lines: app.files_panel(&chat.chat_id),
            });
        }
        Command::Unknown(msg) => app.status_line = Some(msg),
    }
    Ok(())
}

/// Shared `/accept` + `/decline` path — explicit id else newest pending
/// inbound offer in the focused chat.
async fn file_decision<B: Backend>(
    backend: &mut B,
    app: &mut App,
    id: Option<String>,
    accept: bool,
) {
    let Some(chat) = app.active_chat() else {
        app.status_line = Some("no chat selected".to_string());
        return;
    };
    let file_id = match id.or_else(|| app.pending_offer(&chat.chat_id)) {
        Some(f) => f,
        None => {
            app.status_line = Some("no pending offer in this chat".to_string());
            return;
        }
    };
    let res = if accept {
        backend.file_accept(&file_id).await
    } else {
        backend.file_decline(&file_id).await
    };
    match res {
        Ok(()) => {
            app.status_line = Some(format!(
                "offer {}",
                if accept { "accepted" } else { "declined" }
            ))
        }
        Err(e) => app.status_line = Some(format!("file: {e}")),
    }
}

async fn show_finger<B: Backend>(backend: &mut B, app: &mut App, contact_id: &str) {
    let Some(chat) = app.active_chat() else {
        return;
    };
    let name = sanitize(chat.display_name.as_deref().unwrap_or(&chat.peer_id));
    let peer_id = chat.peer_id.clone();
    let trust = chat.trust;
    let sas = backend
        .sas_hint(contact_id)
        .await
        .unwrap_or_else(|_| "unavailable".to_string());
    app.overlay = Some(Overlay::Finger {
        lines: vec![
            format!("contact:  {contact_id}"),
            format!("name:     {name}"),
            format!("peer-id:  {peer_id}"),
            format!("trust:    {trust:?}"),
            format!("sas:      {sas}"),
            String::new(),
            "compare out-of-band, then /verify <sas>".to_string(),
        ],
    });
}

fn compose_external(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    let result = edit_in_editor();
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.clear()?;
    match result {
        Ok(Some(text)) => app.set_draft(&text),
        Ok(None) => app.status_line = Some("editor: empty, discarded".to_string()),
        Err(e) => app.status_line = Some(format!("editor: {e}")),
    }
    Ok(())
}

fn edit_in_editor() -> io::Result<Option<String>> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let path = std::env::temp_dir().join(format!("cinder-draft-{}.txt", std::process::id()));
    std::fs::write(&path, "")?;
    let status = std::process::Command::new(&editor).arg(&path).status();
    let body = match status {
        Ok(s) if s.success() => std::fs::read_to_string(&path).ok(),
        Ok(s) => return Err(io::Error::other(format!("editor exited {s}"))),
        Err(e) => return Err(e),
    };
    let _ = std::fs::remove_file(&path);
    let trimmed = body.unwrap_or_default().trim().to_string();
    Ok((!trimmed.is_empty()).then_some(trimmed))
}
