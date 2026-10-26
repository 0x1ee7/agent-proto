//! Terminal loop: applies background messages, feeds keys to the app, and draws (RFC-0012 sec 4.10).

use std::io;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::{App, Command, Key};
use crate::driver::Msg;
use crate::render;

/// Longest wait for a key before the screen is redrawn.
pub const FRAME: Duration = Duration::from_millis(100);

/// Current wall-clock time in milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Applies one background message to the board or the app state.
pub fn apply(app: &mut App, msg: Msg, now: u64) {
    match msg {
        Msg::Synced(observed) => {
            app.board.heartbeat(now);
            app.board.apply_agents(now, observed);
        }
        Msg::LinkFailed(reason) => app.board.link_failed(&reason),
        Msg::Tasks { agent, tasks } => app.board.apply_tasks(now, &agent, &tasks),
        Msg::Event { agent, event } => app.board.apply_event(now, &agent, &event),
        Msg::Audit(event) => app.board.apply_audit(&event),
        Msg::Decision {
            request_id,
            decision_id,
            outcome,
        } => app.resolve(&request_id, &decision_id, outcome),
        Msg::Notice(text) => app.notice = Some(text),
    }
}

/// Maps a terminal key to the console's key model. Ctrl and Alt chords other than Ctrl+C are
/// ignored so they cannot be mistaken for typed text.
#[must_use]
pub fn map_key(key: KeyEvent) -> Option<Key> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Some(Key::CtrlC),
            _ => None,
        };
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Tab => Some(Key::Tab),
        KeyCode::BackTab => Some(Key::BackTab),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Backspace => Some(Key::Backspace),
        _ => None,
    }
}

/// Runs the console until the app sets `quit`. Commands produced by key presses are forwarded
/// to the driver after each event.
///
/// # Errors
/// Returns terminal I/O errors.
pub fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    messages: &Receiver<Msg>,
    commands: &UnboundedSender<Command>,
) -> io::Result<()> {
    loop {
        let now = now_ms();
        drain(app, messages, now);
        app.tick(now);
        flush(app, commands);
        terminal.draw(|frame| render::draw(frame, app, now))?;
        if event::poll(FRAME)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if let Some(mapped) = map_key(key) {
                        app.key(mapped, now_ms());
                    }
                }
            }
        }
        flush(app, commands);
        if app.quit {
            return Ok(());
        }
    }
}

fn drain(app: &mut App, messages: &Receiver<Msg>, now: u64) {
    loop {
        match messages.try_recv() {
            Ok(msg) => apply(app, msg, now),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                app.notice = Some("bus driver stopped".to_owned());
                return;
            }
        }
    }
}

fn flush(app: &mut App, commands: &UnboundedSender<Command>) {
    for command in app.take_commands() {
        if commands.send(command).is_err() {
            app.notice = Some("bus driver stopped".to_owned());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn req_0012_044_ctrl_c_quits_and_ctrl_chords_are_not_text() {
        assert_eq!(
            map_key(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Key::CtrlC)
        );
        assert_eq!(
            map_key(press(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(map_key(press(KeyCode::Char('x'), KeyModifiers::ALT)), None);
    }

    #[test]
    fn req_0012_044_navigation_keys_map_to_app_keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            map_key(press(KeyCode::Char('j'), none)),
            Some(Key::Char('j'))
        );
        assert_eq!(map_key(press(KeyCode::Enter, none)), Some(Key::Enter));
        assert_eq!(map_key(press(KeyCode::Esc, none)), Some(Key::Esc));
        assert_eq!(map_key(press(KeyCode::Tab, none)), Some(Key::Tab));
        assert_eq!(map_key(press(KeyCode::BackTab, none)), Some(Key::BackTab));
        assert_eq!(map_key(press(KeyCode::Up, none)), Some(Key::Up));
        assert_eq!(map_key(press(KeyCode::Down, none)), Some(Key::Down));
        assert_eq!(
            map_key(press(KeyCode::Backspace, none)),
            Some(Key::Backspace)
        );
        assert_eq!(map_key(press(KeyCode::F(5), none)), None);
    }

    #[test]
    fn req_0012_044_shifted_letters_stay_typed() {
        assert_eq!(
            map_key(press(KeyCode::Char('N'), KeyModifiers::SHIFT)),
            Some(Key::Char('N'))
        );
    }

    #[test]
    fn req_0012_044_apply_notice_sets_status_line() {
        let mut app = App::new(false);
        apply(&mut app, Msg::Notice("audit feed: down".to_owned()), 0);
        assert_eq!(app.notice.as_deref(), Some("audit feed: down"));
    }

    #[test]
    fn req_0012_044_apply_link_failure_is_visible_to_board() {
        let mut app = App::new(false);
        apply(&mut app, Msg::LinkFailed("timeout".to_owned()), 0);
        assert_eq!(app.board.link.failure(), Some("timeout"));
    }
}
