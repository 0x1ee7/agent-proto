//! Operator console state machine: screens, modes, key handling, and the decision
//! gates of RFC-0012 4.10 (dwell, link health, read-only operator, expiry, confirm).
//!
//! The app never performs I/O. It records `Command`s that the driver executes and
//! receives outcomes back through `Board`.

use a2a_client::approval::PendingApproval;
use a2a_types::ids::MessageId;
use a2a_types::proto::a2abus;

use crate::model::Board;
use crate::model::approvals::{DWELL_MS, DecisionOutcome, Phase, allow_once_id, reject_once_id};
use crate::redact;

/// Keys the console reacts to, independent of the terminal backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Up,
    Down,
    Backspace,
    CtrlC,
}

/// Screens of table 4.10.2, selected with `1`-`6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Agents,
    Tasks,
    Task,
    Approvals,
    Budget,
    Audit,
}

impl Screen {
    const ORDER: [Self; 6] = [
        Self::Agents,
        Self::Tasks,
        Self::Task,
        Self::Approvals,
        Self::Budget,
        Self::Audit,
    ];

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Agents => "Agents",
            Self::Tasks => "Tasks",
            Self::Task => "Task",
            Self::Approvals => "Approvals",
            Self::Budget => "Budget",
            Self::Audit => "Audit",
        }
    }

    fn step(self, forward: bool) -> Self {
        let index = Self::ORDER.iter().position(|s| *s == self).unwrap_or(0);
        let len = Self::ORDER.len();
        let next = if forward {
            (index + 1) % len
        } else {
            (index + len - 1) % len
        };
        Self::ORDER.get(next).copied().unwrap_or(self)
    }

    fn from_digit(digit: char) -> Option<Self> {
        let number = digit.to_digit(10)?;
        let index = usize::try_from(number.checked_sub(1)?).ok()?;
        Self::ORDER.get(index).copied()
    }
}

/// Actions that need a second key before they run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    Approve { request_id: String },
    Cancel { agent: String, task: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Filter,
    Note { request_id: String },
    Confirm(Confirm),
    Help,
}

/// Work the driver performs on behalf of the app.
#[derive(Debug)]
pub enum Command {
    Decide {
        agent: String,
        pending: Box<PendingApproval>,
        decision_id: String,
        option_id: String,
    },
    Cancel {
        agent: String,
        task: String,
    },
    Refresh,
    Quit,
}

pub struct App {
    pub board: Board,
    pub screen: Screen,
    pub mode: Mode,
    pub selected: usize,
    /// Approval whose card is open (detail view).
    pub detail: Option<String>,
    /// Task shown on the Task screen.
    pub task_focus: Option<String>,
    pub filter: String,
    pub input: String,
    /// No operator key loaded: the Approvals screen is read-only (0012-043).
    pub read_only: bool,
    /// Why the console is read-only, shown in the banner (RFC-0012 0012-043).
    pub read_only_reason: Option<String>,
    pub notice: Option<String>,
    pub quit: bool,
    card_opened_ms: u64,
    commands: Vec<Command>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("screen", &self.screen)
            .field("mode", &self.mode)
            .field("selected", &self.selected)
            .field("read_only", &self.read_only)
            .field("quit", &self.quit)
            .finish_non_exhaustive()
    }
}

impl App {
    #[must_use]
    pub fn new(read_only: bool) -> Self {
        Self {
            board: Board::default(),
            screen: Screen::Agents,
            mode: Mode::Normal,
            selected: 0,
            detail: None,
            task_focus: None,
            filter: String::new(),
            input: String::new(),
            read_only,
            read_only_reason: None,
            notice: None,
            quit: false,
            card_opened_ms: 0,
            commands: Vec::new(),
        }
    }

    /// Drains commands queued by key handling.
    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.commands)
    }

    /// Records a decision outcome from the driver.
    pub fn resolve(&mut self, request_id: &str, decision_id: &str, outcome: DecisionOutcome) {
        self.board
            .approvals
            .resolve(request_id, decision_id, outcome);
    }

    /// Time-driven transitions (expiry, link state).
    pub fn tick(&mut self, now_ms: u64) {
        self.board.tick(now_ms);
        self.clamp_selection();
    }

    /// Whether the operator may send decisions right now (0012-043, 0012-050).
    #[must_use]
    pub fn decisions_enabled(&self, now_ms: u64) -> bool {
        !self.read_only && self.board.link.decisions_enabled(now_ms)
    }

    /// Agents visible under the current filter, sorted by id.
    #[must_use]
    pub fn visible_agents(&self) -> Vec<String> {
        self.board
            .agents
            .iter()
            .filter(|row| {
                matches_filter(&self.filter, &[row.agent.as_str(), row.card_name.as_str()])
            })
            .map(|row| row.agent.clone())
            .collect()
    }

    /// Task ids visible under the current filter, in display order.
    #[must_use]
    pub fn visible_tasks(&self) -> Vec<String> {
        self.board
            .tasks
            .ordered()
            .into_iter()
            .filter(|row| {
                matches_filter(
                    &self.filter,
                    &[
                        row.id.as_str(),
                        row.agent.as_str(),
                        row.context_id.as_str(),
                        row.state_label(),
                    ],
                )
            })
            .map(|row| row.id.clone())
            .collect()
    }

    fn list_len(&self) -> usize {
        match self.screen {
            Screen::Agents => self.visible_agents().len(),
            Screen::Tasks => self.visible_tasks().len(),
            Screen::Task => 0,
            Screen::Approvals => self.board.approvals.entries().len(),
            Screen::Budget => self.board.tasks.len(),
            Screen::Audit => self.board.audit.len(),
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.list_len();
        self.selected = if len == 0 {
            0
        } else {
            self.selected.min(len - 1)
        };
    }

    /// Handles one key press at `now_ms`.
    pub fn key(&mut self, key: Key, now_ms: u64) {
        if key == Key::CtrlC {
            self.quit_now();
            return;
        }
        match self.mode.clone() {
            Mode::Filter => self.filter_key(key),
            Mode::Note { request_id } => self.note_key(key, &request_id),
            Mode::Confirm(action) => self.confirm_key(key, action, now_ms),
            Mode::Help => self.mode = Mode::Normal,
            Mode::Normal => self.normal_key(key, now_ms),
        }
        self.clamp_selection();
    }

    fn quit_now(&mut self) {
        self.quit = true;
        self.commands.push(Command::Quit);
    }

    fn normal_key(&mut self, key: Key, now_ms: u64) {
        if let Key::Char(c) = key {
            if let Some(screen) = Screen::from_digit(c) {
                self.switch(screen);
                return;
            }
        }
        match key {
            Key::Char('q') => self.quit_now(),
            Key::Char('?') => self.mode = Mode::Help,
            Key::Char('r') => {
                self.commands.push(Command::Refresh);
                self.notice = Some("refresh requested".to_owned());
            }
            Key::Tab => self.switch(self.screen.step(true)),
            Key::BackTab => self.switch(self.screen.step(false)),
            Key::Char('a') => self.switch(Screen::Approvals),
            Key::Char('/') if matches!(self.screen, Screen::Agents | Screen::Tasks) => {
                self.input = self.filter.clone();
                self.mode = Mode::Filter;
            }
            Key::Up | Key::Char('k') => self.selected = self.selected.saturating_sub(1),
            Key::Down | Key::Char('j') => self.selected = self.selected.saturating_add(1),
            Key::Enter => self.enter(now_ms),
            Key::Esc => self.escape(),
            Key::Char('c') if matches!(self.screen, Screen::Tasks | Screen::Task) => {
                self.begin_cancel();
            }
            Key::Char('y') if self.screen == Screen::Approvals => self.approve_key(now_ms),
            Key::Char('n') if self.screen == Screen::Approvals => self.deny_key(now_ms),
            Key::Char('N') if self.screen == Screen::Approvals => self.begin_note(),
            _ => {}
        }
    }

    fn switch(&mut self, screen: Screen) {
        self.screen = screen;
        self.selected = 0;
        self.detail = None;
        self.mode = Mode::Normal;
    }

    fn enter(&mut self, now_ms: u64) {
        match self.screen {
            Screen::Agents => {
                if let Some(agent) = self.visible_agents().get(self.selected).cloned() {
                    self.filter = agent;
                    self.switch(Screen::Tasks);
                }
            }
            Screen::Tasks => {
                if let Some(task) = self.visible_tasks().get(self.selected).cloned() {
                    self.task_focus = Some(task);
                    self.screen = Screen::Task;
                }
            }
            Screen::Approvals => {
                let id = self
                    .board
                    .approvals
                    .entries()
                    .get(self.selected)
                    .map(|e| e.request_id().to_owned());
                if let Some(id) = id {
                    self.detail = Some(id);
                    self.card_opened_ms = now_ms;
                }
            }
            _ => {}
        }
    }

    fn escape(&mut self) {
        if self.detail.is_some() {
            self.detail = None;
        } else if self.screen == Screen::Task {
            self.screen = Screen::Tasks;
        } else {
            self.filter.clear();
        }
    }

    /// The approval the operator is acting on: the open card, else the selected row.
    fn target_approval(&self) -> Option<String> {
        self.detail.clone().or_else(|| {
            self.board
                .approvals
                .entries()
                .get(self.selected)
                .map(|e| e.request_id().to_owned())
        })
    }

    fn approve_key(&mut self, now_ms: u64) {
        let Some(id) = self.detail.clone() else {
            self.notice = Some("open the approval card with Enter before approving".to_owned());
            return;
        };
        if now_ms.saturating_sub(self.card_opened_ms) < DWELL_MS {
            self.notice = Some(format!("wait {DWELL_MS} ms before approving (dwell)"));
            return;
        }
        let needs_confirm = self
            .board
            .approvals
            .get(&id)
            .is_some_and(|e| needs_second_confirm(e.request()));
        if needs_confirm {
            self.mode = Mode::Confirm(Confirm::Approve { request_id: id });
        } else {
            self.decide(&id, Choice::Allow, now_ms);
        }
    }

    fn deny_key(&mut self, now_ms: u64) {
        if let Some(id) = self.target_approval() {
            self.decide(&id, Choice::Reject, now_ms);
        }
    }

    fn begin_note(&mut self) {
        if let Some(id) = self.target_approval() {
            self.input.clear();
            self.mode = Mode::Note { request_id: id };
        }
    }

    fn begin_cancel(&mut self) {
        let focus = if self.screen == Screen::Task {
            self.task_focus.clone()
        } else {
            self.visible_tasks().get(self.selected).cloned()
        };
        let Some(task_id) = focus else {
            return;
        };
        let Some(agent) = self.board.tasks.get(&task_id).map(|t| t.agent.clone()) else {
            return;
        };
        self.mode = Mode::Confirm(Confirm::Cancel {
            agent,
            task: task_id,
        });
    }

    /// Applies the decision gates and queues the decision (RFC-0012 4.10, RFC-0006).
    fn decide(&mut self, request_id: &str, choice: Choice, now_ms: u64) {
        if self.read_only {
            self.notice = Some("read-only: no operator key loaded".to_owned());
            return;
        }
        if !self.board.link.decisions_enabled(now_ms) {
            self.notice = Some("disconnected: decisions disabled".to_owned());
            return;
        }
        let Some(entry) = self.board.approvals.get(request_id) else {
            return;
        };
        if !entry.is_open() {
            self.notice = Some(format!("approval is {}", phase_label(&entry.phase)));
            return;
        }
        let option = match choice {
            Choice::Allow => allow_once_id(entry.request()).map(str::to_owned),
            Choice::Reject => Some(reject_once_id(entry.request()).to_owned()),
        };
        let Some(option_id) = option else {
            self.notice = Some("approval offers no allow-once option".to_owned());
            return;
        };
        let agent = entry.agent.clone();
        let pending = entry.pending.clone();
        let decision_id = MessageId::generate().as_str().to_owned();
        if !self
            .board
            .approvals
            .begin(request_id, &decision_id, &option_id)
        {
            return;
        }
        self.detail = None;
        self.mode = Mode::Normal;
        self.notice = Some(format!("decision sent for {request_id}"));
        self.commands.push(Command::Decide {
            agent,
            pending: Box::new(pending),
            decision_id,
            option_id,
        });
    }

    fn confirm_key(&mut self, key: Key, action: Confirm, now_ms: u64) {
        self.mode = Mode::Normal;
        if key != Key::Char('y') {
            self.notice = Some("cancelled".to_owned());
            return;
        }
        match action {
            Confirm::Approve { request_id } => self.decide(&request_id, Choice::Allow, now_ms),
            Confirm::Cancel { agent, task } => {
                if !self.board.link.decisions_enabled(now_ms) {
                    self.notice = Some("disconnected: cancel disabled".to_owned());
                    return;
                }
                self.notice = Some(format!("cancel requested for {task}"));
                self.commands.push(Command::Cancel { agent, task });
            }
        }
    }

    fn note_key(&mut self, key: Key, request_id: &str) {
        match key {
            Key::Esc => self.mode = Mode::Normal,
            Key::Enter => {
                let note = redact::note(&self.input);
                let note = (!note.is_empty()).then_some(note);
                self.board.approvals.set_note(request_id, note);
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Key::Backspace => {
                self.input.pop();
            }
            Key::Char(c) if self.input.chars().count() < redact::NOTE_MAX_CHARS => {
                self.input.push(c);
            }
            _ => {}
        }
    }

    fn filter_key(&mut self, key: Key) {
        match key {
            Key::Esc => self.mode = Mode::Normal,
            Key::Enter => {
                self.filter = self.input.clone();
                self.mode = Mode::Normal;
            }
            Key::Backspace => {
                self.input.pop();
            }
            Key::Char(c) => self.input.push(c),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Allow,
    Reject,
}

/// Exec and delete_or_move approvals need a second `y` (RFC-0012 4.10.2).
fn needs_second_confirm(request: &a2abus::ApprovalRequest) -> bool {
    matches!(
        a2abus::ApprovalTier::try_from(request.tier),
        Ok(a2abus::ApprovalTier::Exec | a2abus::ApprovalTier::DeleteOrMove)
    )
}

pub(crate) const fn phase_label(phase: &Phase) -> &'static str {
    match phase {
        Phase::Open => "open",
        Phase::Sending { .. } => "sending",
        Phase::Unknown { .. } => "unknown",
        Phase::Allowed => "allowed",
        Phase::Denied => "denied",
        Phase::Refused { .. } => "refused",
        Phase::Expired => "expired",
        Phase::Closed => "closed",
    }
}

/// Case-insensitive substring filter over several fields; an empty filter matches all.
fn matches_filter(filter: &str, fields: &[&str]) -> bool {
    if filter.is_empty() {
        return true;
    }
    let needle = filter.to_lowercase();
    fields.iter().any(|f| f.to_lowercase().contains(&needle))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn req_0012_044_digit_keys_select_the_six_screens() {
        let mut app = App::new(false);
        app.key(Key::Char('4'), 0);
        assert_eq!(app.screen, Screen::Approvals);
        app.key(Key::Char('6'), 0);
        assert_eq!(app.screen, Screen::Audit);
        app.key(Key::Tab, 0);
        assert_eq!(app.screen, Screen::Agents);
        app.key(Key::BackTab, 0);
        assert_eq!(app.screen, Screen::Audit);
    }

    #[test]
    fn req_0012_044_a_jumps_to_approvals_and_q_quits_the_console() {
        let mut app = App::new(false);
        app.key(Key::Char('a'), 0);
        assert_eq!(app.screen, Screen::Approvals);
        app.key(Key::Char('q'), 0);
        assert!(app.quit);
        let commands = app.take_commands();
        assert!(matches!(commands.as_slice(), [Command::Quit]));
    }

    #[test]
    fn req_0012_044_help_overlay_opens_and_any_key_closes_it() {
        let mut app = App::new(false);
        app.key(Key::Char('?'), 0);
        assert_eq!(app.mode, Mode::Help);
        app.key(Key::Esc, 0);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn req_0012_044_refresh_queues_a_refresh_command() {
        let mut app = App::new(false);
        app.key(Key::Char('r'), 0);
        assert!(matches!(app.take_commands().as_slice(), [Command::Refresh]));
    }

    #[test]
    fn req_0012_043_read_only_operator_cannot_decide() {
        let mut app = App::new(true);
        app.key(Key::Char('a'), 0);
        app.key(Key::Char('n'), 0);
        assert!(app.take_commands().is_empty());
        assert!(!app.decisions_enabled(0));
    }

    #[test]
    fn req_0012_046_approve_is_refused_without_an_open_card() {
        let mut app = App::new(false);
        app.key(Key::Char('a'), 0);
        app.key(Key::Char('y'), 10);
        assert!(app.take_commands().is_empty());
        assert!(app.notice.as_deref().unwrap_or("").contains("Enter"));
    }

    #[test]
    fn req_0012_048_bulk_keys_do_nothing() {
        let mut app = App::new(false);
        app.key(Key::Char('a'), 0);
        app.key(Key::Char('Y'), 0);
        app.key(Key::Char('*'), 0);
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn req_0012_044_filter_mode_collects_text_and_commits_on_enter() {
        let mut app = App::new(false);
        app.key(Key::Char('/'), 0);
        assert_eq!(app.mode, Mode::Filter);
        app.key(Key::Char('o'), 0);
        app.key(Key::Char('m'), 0);
        app.key(Key::Char('p'), 0);
        app.key(Key::Enter, 0);
        assert_eq!(app.filter, "omp");
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn req_0012_044_cancel_is_refused_without_a_selected_task() {
        let mut app = App::new(false);
        app.key(Key::Char('2'), 0);
        app.key(Key::Char('c'), 0);
        assert_eq!(app.mode, Mode::Normal);
        app.key(Key::Char('y'), 0);
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn req_0012_045_matches_filter_is_case_insensitive() {
        assert!(matches_filter("OMP", &["omp-worker"]));
        assert!(matches_filter("", &["anything"]));
        assert!(!matches_filter("claude", &["omp"]));
    }

    #[test]
    fn req_0012_049_note_input_is_capped() {
        let mut app = App::new(false);
        app.mode = Mode::Note {
            request_id: "r".to_owned(),
        };
        for _ in 0..(redact::NOTE_MAX_CHARS + 20) {
            app.key(Key::Char('x'), 0);
        }
        assert_eq!(app.input.chars().count(), redact::NOTE_MAX_CHARS);
        app.key(Key::Esc, 0);
        assert_eq!(app.mode, Mode::Normal);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod gate_tests {
    use super::*;
    use crate::model::approvals::fixtures::{pending, pending_from, request};
    use a2a_core::approval::ALLOW_ONCE_ID;
    use a2a_core::approval::REJECT_ONCE_ID;

    const EXPIRES_SECS: i64 = 1_000_000;

    fn app_with_open(read_only: bool, tier: a2abus::ApprovalTier) -> App {
        let mut app = App::new(read_only);
        app.board.link.ok(0);
        let mut req = request("r1", EXPIRES_SECS);
        req.tier = tier as i32;
        app.board.approvals.observe("omp", pending_from(&req), 0);
        app.key(Key::Char('a'), 0);
        app
    }

    fn decide_option(commands: &[Command]) -> Option<String> {
        match commands {
            [Command::Decide { option_id, .. }] => Some(option_id.clone()),
            _ => None,
        }
    }

    #[test]
    fn req_0012_046_approve_waits_for_dwell_then_sends_allow_once() {
        let mut app = app_with_open(false, a2abus::ApprovalTier::Write);
        app.key(Key::Enter, 100);
        app.key(Key::Char('y'), 500);
        assert!(app.take_commands().is_empty());
        app.key(Key::Char('y'), 100 + DWELL_MS);
        let commands = app.take_commands();
        assert_eq!(decide_option(&commands).as_deref(), Some(ALLOW_ONCE_ID));
        assert_eq!(
            app.board.approvals.get("r1").map(|e| e.phase.clone()),
            Some(Phase::Sending {
                decision_id: match commands.as_slice() {
                    [Command::Decide { decision_id, .. }] => decision_id.clone(),
                    _ => String::new(),
                },
                option_id: ALLOW_ONCE_ID.to_owned(),
            })
        );
    }

    #[test]
    fn req_0012_046_deny_has_no_dwell_and_sends_reject_once() {
        let mut app = app_with_open(false, a2abus::ApprovalTier::Write);
        app.key(Key::Char('n'), 0);
        let commands = app.take_commands();
        assert_eq!(decide_option(&commands).as_deref(), Some(REJECT_ONCE_ID));
    }

    #[test]
    fn req_0012_043_read_only_operator_sends_no_decision() {
        let mut app = app_with_open(true, a2abus::ApprovalTier::Write);
        app.key(Key::Enter, 0);
        app.key(Key::Char('y'), 10 * DWELL_MS);
        app.key(Key::Char('n'), 10 * DWELL_MS);
        assert!(app.take_commands().is_empty());
        assert!(app.notice.as_deref().unwrap_or("").contains("read-only"));
    }

    #[test]
    fn req_0012_050_disconnected_link_disables_decisions() {
        let mut app = app_with_open(false, a2abus::ApprovalTier::Write);
        app.tick(6_000);
        app.key(Key::Char('n'), 6_000);
        assert!(app.take_commands().is_empty());
        assert!(app.notice.as_deref().unwrap_or("").contains("disconnected"));
    }

    #[test]
    fn req_0006_004_exec_approval_needs_a_second_confirm() {
        let mut app = app_with_open(false, a2abus::ApprovalTier::Exec);
        app.key(Key::Enter, 0);
        app.key(Key::Char('y'), DWELL_MS);
        assert!(app.take_commands().is_empty());
        assert!(matches!(app.mode, Mode::Confirm(Confirm::Approve { .. })));
        app.key(Key::Char('y'), DWELL_MS + 1);
        assert_eq!(
            decide_option(&app.take_commands()).as_deref(),
            Some(ALLOW_ONCE_ID)
        );
    }

    #[test]
    fn req_0006_004_confirm_other_key_cancels_without_sending() {
        let mut app = app_with_open(false, a2abus::ApprovalTier::DeleteOrMove);
        app.key(Key::Enter, 0);
        app.key(Key::Char('y'), DWELL_MS);
        app.key(Key::Esc, DWELL_MS);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn req_0006_057_expired_approval_is_refused_locally() {
        let mut app = app_with_open(false, a2abus::ApprovalTier::Write);
        app.key(Key::Enter, 0);
        let expired = EXPIRES_SECS as u64 * 1_000;
        app.board.link.ok(expired);
        app.tick(expired);
        app.key(Key::Char('y'), expired + DWELL_MS);
        assert!(app.take_commands().is_empty());
        assert!(app.notice.as_deref().unwrap_or("").contains("expired"));
    }

    #[test]
    fn req_0012_049_note_is_stored_locally_and_redacted() {
        let mut app = app_with_open(false, a2abus::ApprovalTier::Write);
        app.key(Key::Char('N'), 0);
        for c in "a\u{1b}b".chars() {
            app.key(Key::Char(c), 0);
        }
        app.key(Key::Enter, 0);
        let note = app.board.approvals.get("r1").and_then(|e| e.note.clone());
        assert!(note.is_some());
        assert!(!note.unwrap_or_default().contains('\u{1b}'));
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn req_0012_046_pending_fixture_is_usable_for_decisions() {
        let mut app = App::new(false);
        app.board.link.ok(0);
        app.board
            .approvals
            .observe("omp", pending("r2", EXPIRES_SECS), 0);
        app.key(Key::Char('a'), 0);
        app.key(Key::Char('n'), 0);
        assert_eq!(app.take_commands().len(), 1);
    }
}
