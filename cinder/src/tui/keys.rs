use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, Overlay};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Noop,
    Quit,
    Submit,
    Input(KeyEvent),
    Jump(usize),
    NextUnread,
    OpenFuzzy,
    CloseOverlay,
    FuzzyKey(KeyEvent),
    FuzzyAccept,
    Compose,
    PageUp,
    PageDown,
}

pub fn handle_key(app: &App, key: KeyEvent) -> Action {
    if let Some(overlay) = &app.overlay {
        return match overlay {
            Overlay::Fuzzy { .. } => match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => Action::CloseOverlay,
                (KeyCode::Enter, _) => Action::FuzzyAccept,
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => Action::Quit,
                _ => Action::FuzzyKey(key),
            },
            Overlay::Help | Overlay::Finger { .. } => match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => Action::CloseOverlay,
                (KeyCode::Char('q'), _) => Action::CloseOverlay,
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => Action::Quit,
                _ => Action::Noop,
            },
        };
    }
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => Action::Quit,
        (KeyCode::Esc, _) => Action::Noop,
        (KeyCode::Enter, _) => Action::Submit,
        (KeyCode::PageUp, _) => Action::PageUp,
        (KeyCode::PageDown, _) => Action::PageDown,
        (KeyCode::Char(c @ '1'..='9'), m) if m.contains(KeyModifiers::ALT) => {
            Action::Jump((c as usize) - ('1' as usize))
        }
        (KeyCode::Char('a'), m) if m.contains(KeyModifiers::ALT) => Action::NextUnread,
        (KeyCode::Char('g'), m) if m.contains(KeyModifiers::CONTROL) => Action::OpenFuzzy,
        (KeyCode::Char('k'), m) if m.contains(KeyModifiers::ALT) => Action::Compose,
        _ => Action::Input(key),
    }
}
