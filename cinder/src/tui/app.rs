use std::collections::HashMap;

use ratatui_textarea::TextArea;

use super::backend::{
    now_ts, ChatSummary, ConnState, Dir, Event, HistoryPage, Message, MsgState, Trust,
};

#[derive(Debug, Clone)]
pub struct ChatBuffer {
    pub messages: Vec<Message>,
    pub next_before: Option<String>,
    pub has_more: bool,
    pub loaded: bool,
    pub fetching: bool,
    pub scroll: u16,
    pub read_marker: Option<usize>,
    pub pending_mark: Option<u32>,
    pub render_rows: usize,
}

impl ChatBuffer {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            next_before: None,
            has_more: false,
            loaded: false,
            fetching: false,
            scroll: 0,
            read_marker: None,
            pending_mark: None,
            render_rows: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Overlay {
    Fuzzy { query: String, sel: usize },
    Help,
    Finger { lines: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Add(String),
    Verify(Option<String>),
    Finger,
    Clear,
    Quit,
    Status,
    Next,
    Unknown(String),
}

pub fn parse_command(line: &str) -> Command {
    let line = line.trim_end();
    let (cmd, arg) = match line.split_once(char::is_whitespace) {
        Some((c, a)) => (c, Some(a.trim())),
        None => (line, None),
    };
    match cmd {
        "/help" | "/h" | "/?" => Command::Help,
        "/add" | "/invite" => match arg.filter(|a| !a.is_empty()) {
            Some(a) => Command::Add(a.to_string()),
            None => Command::Unknown("usage: /add <cinder://link>".to_string()),
        },
        "/verify" => Command::Verify(arg.filter(|a| !a.is_empty()).map(str::to_string)),
        "/finger" | "/fp" => Command::Finger,
        "/clear" => Command::Clear,
        "/quit" | "/q" => Command::Quit,
        "/status" => Command::Status,
        "/next" => Command::Next,
        other => Command::Unknown(format!("unknown command {other} (/help)")),
    }
}

fn state_rank(s: MsgState) -> u8 {
    match s {
        MsgState::Queued => 0,
        MsgState::Sent => 1,
        MsgState::Delivered => 2,
        MsgState::Failed => 3,
    }
}

fn apply_state(cur: &mut MsgState, new: MsgState) {
    if *cur == MsgState::Failed {
        return;
    }
    if new == MsgState::Failed || state_rank(new) > state_rank(*cur) {
        *cur = new;
    }
}

pub fn first_unread_idx(loaded: usize, unread: u32) -> usize {
    loaded.saturating_sub(unread as usize)
}

pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let q: Vec<char> = query.to_lowercase().chars().collect();
    if q.is_empty() {
        return Some(0);
    }
    let mut score = 0i32;
    let mut qi = 0;
    let mut prev_matched = false;
    let mut prev_char: Option<char> = None;
    for c in candidate.to_lowercase().chars() {
        if qi < q.len() && c == q[qi] {
            score += 1;
            if prev_char.is_none() || prev_char == Some(' ') {
                score += 3;
            }
            if prev_matched {
                score += 2;
            }
            qi += 1;
            prev_matched = true;
        } else {
            prev_matched = false;
        }
        prev_char = Some(c);
    }
    (qi == q.len()).then_some(score)
}

pub struct App {
    pub peer_id: String,
    pub conn: ConnState,
    pub chats: Vec<ChatSummary>,
    pub active_chat: Option<String>,
    pub buffers: HashMap<String, ChatBuffer>,
    pub unread: HashMap<String, u32>,
    pub input: TextArea<'static>,
    pub overlay: Option<Overlay>,
    pub quit: bool,
    pub status_line: Option<String>,
    pub pending: HashMap<String, String>,
    pub chat_view_height: u16,
}

impl App {
    pub fn new(peer_id: String, conn: ConnState) -> Self {
        Self {
            peer_id,
            conn,
            chats: Vec::new(),
            active_chat: None,
            buffers: HashMap::new(),
            unread: HashMap::new(),
            input: fresh_textarea(),
            overlay: None,
            quit: false,
            status_line: None,
            pending: HashMap::new(),
            chat_view_height: 1,
        }
    }

    pub fn set_chats(&mut self, mut chats: Vec<ChatSummary>) {
        chats.sort_by_key(|c| std::cmp::Reverse(c.last_activity));
        for c in &chats {
            if c.unread > 0 {
                self.unread.insert(c.chat_id.clone(), c.unread);
            }
        }
        self.chats = chats;
        if self.active_chat.is_none() {
            if let Some(first) = self.chats.first() {
                let id = first.chat_id.clone();
                self.activate(&id);
            }
        }
    }

    pub fn active_idx(&self) -> Option<usize> {
        self.chats
            .iter()
            .position(|c| Some(&c.chat_id) == self.active_chat.as_ref())
    }

    pub fn active_chat(&self) -> Option<&ChatSummary> {
        self.chats
            .iter()
            .find(|c| Some(&c.chat_id) == self.active_chat.as_ref())
    }

    pub fn buffer(&self, chat_id: &str) -> Option<&ChatBuffer> {
        self.buffers.get(chat_id)
    }

    pub fn buffer_mut(&mut self, chat_id: &str) -> &mut ChatBuffer {
        self.buffers
            .entry(chat_id.to_string())
            .or_insert_with(ChatBuffer::new)
    }

    pub fn activate(&mut self, chat_id: &str) {
        if self.chats.iter().all(|c| c.chat_id != chat_id) {
            return;
        }
        self.active_chat = Some(chat_id.to_string());
        let unread = self.unread.remove(chat_id).unwrap_or(0);
        if unread > 0 {
            let buf = self.buffer_mut(chat_id);
            if buf.loaded {
                buf.read_marker = Some(first_unread_idx(buf.messages.len(), unread));
            } else {
                buf.pending_mark = Some(unread);
            }
        }
        if let Some(buf) = self.buffers.get_mut(chat_id) {
            buf.scroll = 0;
        }
    }

    pub fn activate_index(&mut self, idx: usize) {
        if let Some(c) = self.chats.get(idx) {
            let id = c.chat_id.clone();
            self.activate(&id);
        }
    }

    pub fn next_unread(&mut self) -> bool {
        if let Some(c) = self
            .chats
            .iter()
            .find(|c| self.unread.get(&c.chat_id).copied().unwrap_or(0) > 0)
        {
            let id = c.chat_id.clone();
            self.activate(&id);
            true
        } else {
            self.status_line = Some("no unread chats".to_string());
            false
        }
    }

    pub fn insert_history(&mut self, chat_id: &str, page: HistoryPage) {
        let buf = self.buffer_mut(chat_id);
        let n = page.messages.len();
        let mut older = page.messages;
        older.reverse();
        buf.messages.splice(0..0, older);
        if let Some(mk) = buf.read_marker {
            buf.read_marker = Some(mk + n);
        }
        buf.next_before = page.next_before;
        buf.has_more = page.has_more && buf.next_before.is_some();
        buf.fetching = false;
        buf.loaded = true;
        if let Some(mark) = buf.pending_mark.take() {
            buf.read_marker = Some(first_unread_idx(buf.messages.len(), mark));
        }
    }

    pub fn apply_event(&mut self, ev: Event) {
        match ev {
            Event::MessageReceived(m) => {
                let chat_id = m.chat_id.clone();
                let ts = m.ts;
                let preview = m.text.clone();
                self.buffer_mut(&chat_id).messages.push(m);
                self.touch_chat(&chat_id, ts, &preview);
                if self.active_chat.as_deref() != Some(chat_id.as_str()) {
                    *self.unread.entry(chat_id).or_insert(0) += 1;
                }
            }
            Event::MsgState {
                msg_id,
                client_msg_id,
                chat_id,
                state,
            } => {
                self.on_msg_state(&chat_id, &msg_id, &client_msg_id, state);
            }
            Event::ContactConnected {
                contact_id,
                peer_id,
                chat_id,
                display_name,
                trust,
            } => {
                if self.chats.iter().all(|c| c.chat_id != chat_id) {
                    self.chats.push(ChatSummary {
                        chat_id: chat_id.clone(),
                        contact_id,
                        peer_id,
                        display_name,
                        trust,
                        unread: 0,
                        last_activity: now_ts(),
                        last_message: None,
                    });
                    self.sort_chats();
                }
                self.status_line = Some("contact connected".to_string());
            }
            Event::IdentityChanged {
                contact_id,
                chat_id,
            } => {
                if let Some(c) = self.chats.iter_mut().find(|c| c.chat_id == chat_id) {
                    c.trust = Trust::KeyChanged;
                }
                self.status_line = Some(format!(
                    "WARNING: {contact_id} key changed — sending blocked until /verify"
                ));
            }
            Event::VerificationNeeded { contact_id, sas } => {
                self.status_line = Some(if sas.is_empty() {
                    format!("{contact_id} unverified — /finger to inspect")
                } else {
                    format!("{contact_id} unverified — sas: {sas} — /finger to inspect")
                });
            }
            Event::ConnChanged(s) => {
                self.conn = s;
            }
        }
    }

    fn touch_chat(&mut self, chat_id: &str, ts: i64, preview: &str) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.chat_id == chat_id) {
            c.last_activity = ts;
            c.last_message = Some(preview.chars().take(60).collect());
        }
        self.sort_chats();
    }

    fn sort_chats(&mut self) {
        self.chats
            .sort_by_key(|c| std::cmp::Reverse(c.last_activity));
    }

    pub fn on_msg_state(
        &mut self,
        chat_id: &str,
        msg_id: &str,
        client_msg_id: &str,
        state: MsgState,
    ) {
        let resolved = self
            .pending
            .get(client_msg_id)
            .cloned()
            .unwrap_or_else(|| msg_id.to_string());
        if let Some(buf) = self.buffers.get_mut(chat_id) {
            if let Some(m) = buf
                .messages
                .iter_mut()
                .find(|m| m.msg_id == resolved || m.msg_id == msg_id)
            {
                apply_state(&mut m.state, state);
            }
        }
        if matches!(state, MsgState::Delivered | MsgState::Failed) {
            self.pending.remove(client_msg_id);
        }
    }

    pub fn push_outgoing(&mut self, chat_id: &str, msg_id: &str, client_msg_id: &str, body: &str) {
        let msg = Message {
            msg_id: msg_id.to_string(),
            chat_id: chat_id.to_string(),
            dir: Dir::Out,
            from: self.peer_id.clone(),
            text: body.to_string(),
            state: MsgState::Queued,
            ts: now_ts(),
        };
        self.pending
            .insert(client_msg_id.to_string(), msg_id.to_string());
        self.buffer_mut(chat_id).messages.push(msg);
        let ts = now_ts();
        self.touch_chat(chat_id, ts, body);
    }

    pub fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    pub fn take_draft(&mut self) -> String {
        let text = self.input_text();
        self.input = fresh_textarea();
        text.trim_end().to_string()
    }

    pub fn set_draft(&mut self, text: &str) {
        let mut ta = fresh_textarea();
        for line in text.lines() {
            ta.insert_str(line);
            ta.insert_newline();
        }
        self.input = ta;
    }

    pub fn page_scroll(&mut self, up: bool) {
        let Some(chat_id) = self.active_chat.clone() else {
            return;
        };
        let view_h = self.chat_view_height as usize;
        let buf = self.buffer_mut(&chat_id);
        let page = view_h.saturating_sub(2).max(1);
        if up {
            let max = buf.render_rows.saturating_sub(view_h);
            buf.scroll = (buf.scroll as usize + page).min(max) as u16;
        } else {
            buf.scroll = buf.scroll.saturating_sub(page as u16);
        }
    }

    pub fn needs_older(&self) -> Option<String> {
        let chat_id = self.active_chat.as_ref()?;
        let buf = self.buffers.get(chat_id)?;
        if !buf.has_more || buf.fetching {
            return None;
        }
        let max = buf
            .render_rows
            .saturating_sub(self.chat_view_height as usize);
        ((buf.scroll as usize) + 4 >= max && max > 0).then(|| chat_id.clone())
    }

    pub fn fuzzy_matches(&self, query: &str) -> Vec<usize> {
        let mut scored: Vec<(usize, i32)> = self
            .chats
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let title = c.display_name.as_deref().unwrap_or(&c.peer_id);
                fuzzy_score(query, title).map(|s| (i, s))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.into_iter().map(|(i, _)| i).collect()
    }
}

pub fn fresh_textarea() -> TextArea<'static> {
    use ratatui::style::{Color, Style};
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(Style::default());
    ta.set_cursor_style(Style::default().fg(Color::Black).bg(Color::White));
    ta
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::backend::{Event, MsgState};

    fn chat(id: &str, unread: u32, ts: i64) -> ChatSummary {
        ChatSummary {
            chat_id: id.to_string(),
            contact_id: format!("contact:{id}"),
            peer_id: format!("peer:{id}"),
            display_name: Some(id.to_string()),
            trust: Trust::Unverified,
            unread,
            last_activity: ts,
            last_message: None,
        }
    }

    fn app() -> App {
        let mut a = App::new("peer:self".to_string(), ConnState::Connected);
        a.set_chats(vec![chat("a", 0, 30), chat("b", 2, 20), chat("c", 0, 10)]);
        a
    }

    fn page(msgs: Vec<Message>) -> HistoryPage {
        HistoryPage {
            messages: msgs,
            has_more: false,
            next_before: None,
        }
    }

    fn msg(chat_id: &str, n: usize, dir: Dir) -> Message {
        Message {
            msg_id: format!("msg:{chat_id}:{n}"),
            chat_id: chat_id.to_string(),
            dir,
            from: "peer:x".to_string(),
            text: format!("m{n}"),
            state: MsgState::Delivered,
            ts: n as i64,
        }
    }

    #[test]
    fn unread_increments_only_inactive() {
        let mut a = app();
        a.activate("a");
        a.apply_event(Event::MessageReceived(msg("a", 1, Dir::In)));
        assert_eq!(a.unread.get("a"), None);
        a.apply_event(Event::MessageReceived(msg("b", 1, Dir::In)));
        a.apply_event(Event::MessageReceived(msg("b", 2, Dir::In)));
        assert_eq!(a.unread.get("b"), Some(&4));
    }

    #[test]
    fn read_marker_math() {
        assert_eq!(first_unread_idx(50, 5), 45);
        assert_eq!(first_unread_idx(3, 10), 0);
        let mut a = app();
        a.activate("b");
        assert_eq!(a.unread.get("b"), None);
        let msgs: Vec<_> = (0..10).map(|i| msg("b", i, Dir::In)).collect();
        a.insert_history("b", page(msgs));
        let buf = a.buffer("b").unwrap();
        assert_eq!(buf.read_marker, Some(8));
    }

    #[test]
    fn marker_shifts_on_older_page() {
        let mut a = app();
        a.activate("b");
        let msgs: Vec<_> = (0..10).map(|i| msg("b", i, Dir::In)).collect();
        a.insert_history("b", page(msgs));
        assert_eq!(a.buffer("b").unwrap().read_marker, Some(8));
        let older: Vec<_> = (10..15).map(|i| msg("b", i, Dir::In)).collect();
        a.insert_history("b", page(older));
        assert_eq!(a.buffer("b").unwrap().read_marker, Some(13));
    }

    #[test]
    fn msg_state_transitions() {
        let mut a = app();
        a.activate("a");
        a.push_outgoing("a", "msg:1", "cid:1", "hi");
        let st = |a: &App| a.buffer("a").unwrap().messages.last().unwrap().state;
        assert_eq!(st(&a), MsgState::Queued);
        a.apply_event(Event::MsgState {
            msg_id: "msg:1".into(),
            client_msg_id: "cid:1".into(),
            chat_id: "a".into(),
            state: MsgState::Sent,
        });
        assert_eq!(st(&a), MsgState::Sent);
        a.apply_event(Event::MsgState {
            msg_id: "msg:1".into(),
            client_msg_id: "cid:1".into(),
            chat_id: "a".into(),
            state: MsgState::Delivered,
        });
        assert_eq!(st(&a), MsgState::Delivered);
        a.apply_event(Event::MsgState {
            msg_id: "msg:1".into(),
            client_msg_id: "cid:1".into(),
            chat_id: "a".into(),
            state: MsgState::Queued,
        });
        assert_eq!(st(&a), MsgState::Delivered);
    }

    #[test]
    fn failed_is_terminal() {
        let mut a = app();
        a.activate("a");
        a.push_outgoing("a", "msg:1", "cid:1", "hi");
        a.apply_event(Event::MsgState {
            msg_id: "msg:1".into(),
            client_msg_id: "cid:1".into(),
            chat_id: "a".into(),
            state: MsgState::Failed,
        });
        a.apply_event(Event::MsgState {
            msg_id: "msg:1".into(),
            client_msg_id: "cid:1".into(),
            chat_id: "a".into(),
            state: MsgState::Sent,
        });
        assert_eq!(a.buffer("a").unwrap().messages[0].state, MsgState::Failed);
    }

    #[test]
    fn command_table() {
        assert_eq!(parse_command("/help"), Command::Help);
        assert_eq!(parse_command("/quit"), Command::Quit);
        assert_eq!(parse_command("/q"), Command::Quit);
        assert_eq!(
            parse_command("/add cinder://x@y"),
            Command::Add("cinder://x@y".into())
        );
        assert!(matches!(parse_command("/add"), Command::Unknown(_)));
        assert_eq!(parse_command("/verify"), Command::Verify(None));
        assert_eq!(
            parse_command("/verify 44-19"),
            Command::Verify(Some("44-19".into()))
        );
        assert_eq!(parse_command("/finger"), Command::Finger);
        assert_eq!(parse_command("/clear"), Command::Clear);
        assert_eq!(parse_command("/status"), Command::Status);
        assert_eq!(parse_command("/next"), Command::Next);
        assert!(matches!(parse_command("/bogus"), Command::Unknown(_)));
    }

    #[test]
    fn fuzzy_ranking() {
        assert!(fuzzy_score("eid", "eidolon").unwrap() > fuzzy_score("eid", "veiled").unwrap());
        assert_eq!(fuzzy_score("xyz", "eidolon"), None);
        assert!(fuzzy_score("", "anything").is_some());
        let mut a = app();
        a.set_chats(vec![chat("zzz", 0, 1), chat("eidolon", 0, 2)]);
        let m = a.fuzzy_matches("eid");
        assert_eq!(a.chats[m[0]].chat_id, "eidolon");
    }

    #[test]
    fn next_unread_cycles() {
        let mut a = app();
        assert!(a.next_unread());
        assert_eq!(a.active_chat.as_deref(), Some("b"));
        assert!(!a.next_unread());
    }
}
