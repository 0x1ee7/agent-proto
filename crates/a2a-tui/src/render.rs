//! Rendering of the operator console (RFC-0012 sec 4.10).
//!
//! Every screen is built as plain text lines from `&App` so the content can be tested without a
//! terminal. Remote text is escaped before it is shown (RFC-0013 redaction, 0012-049).

use a2a_types::proto::a2abus::{ApprovalOptionKind, ApprovalTier};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::{App, Confirm, Mode, Screen, phase_label};
use crate::model::agents::Presence;
use crate::model::approvals::DWELL_MS;
use crate::model::link::LinkState;
use crate::model::tasks::Budget;
use crate::redact;

/// Draws the whole console for the current app state.
pub fn draw(frame: &mut Frame, app: &App, now: u64) {
    let banners = banners(app, now);
    let banner_rows = u16::try_from(banners.len()).unwrap_or(u16::MAX);
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(banner_rows),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    frame.render_widget(Paragraph::new(text(&[header(app, now)])), chunks[0]);
    frame.render_widget(Paragraph::new(text(&banners)), chunks[1]);

    let body = if matches!(app.mode, Mode::Help) {
        help_lines()
    } else {
        body_lines(app, now)
    };
    let block = Block::bordered().title(format!(" {} ", app.screen.title()));
    frame.render_widget(
        Paragraph::new(text(&body))
            .block(block)
            .wrap(Wrap { trim: false }),
        chunks[2],
    );

    let status = status_line(app).unwrap_or_default();
    frame.render_widget(Paragraph::new(text(&[status])), chunks[3]);
    frame.render_widget(
        Paragraph::new(text(&[footer_hint(app).to_owned()])),
        chunks[4],
    );
}

fn text(lines: &[String]) -> Text<'static> {
    Text::from(
        lines
            .iter()
            .map(|line| Line::raw(line.clone()))
            .collect::<Vec<_>>(),
    )
}

/// Human label for the link state shown in the header.
#[must_use]
pub const fn link_label(state: LinkState) -> &'static str {
    match state {
        LinkState::Connecting => "connecting",
        LinkState::Healthy => "healthy",
        LinkState::Degraded => "degraded",
        LinkState::Disconnected => "disconnected",
    }
}

fn header(app: &App, now: u64) -> String {
    let mode = if app.read_only {
        "read-only"
    } else {
        "operator"
    };
    format!(
        "a2a-tui  [{}]  link: {}  mode: {}",
        app.screen.title(),
        link_label(app.board.link.state_at(now)),
        mode
    )
}

/// Banners stacked under the header: link state, read-only reason, burst and flood notices.
#[must_use]
pub fn banners(app: &App, now: u64) -> Vec<String> {
    let mut out = Vec::new();
    let state = app.board.link.state_at(now);
    let disabled = if app.board.link.decisions_enabled(now) {
        ""
    } else {
        "; decisions disabled"
    };
    match state {
        LinkState::Connecting => {
            out.push("CONNECTING: waiting for the first directory sync".to_owned());
        }
        LinkState::Healthy => {}
        LinkState::Degraded => {
            out.push(format!(
                "DEGRADED: heartbeat late, data may be stale{disabled}"
            ));
        }
        LinkState::Disconnected => {
            out.push(format!(
                "DISCONNECTED: bus unreachable, data is stale{disabled}"
            ));
        }
    }
    if state != LinkState::Healthy {
        if let Some(reason) = app.board.link.failure() {
            out.push(format!("last link error: {}", redact::note(reason)));
        }
    }
    if app.read_only {
        let reason = app
            .read_only_reason
            .as_deref()
            .unwrap_or("no operator identity");
        out.push(format!("READ-ONLY: {}", redact::note(reason)));
    }
    for row in app.board.agents.iter() {
        if let Some(count) = app.board.approvals.burst(&row.agent) {
            out.push(format!(
                "BURST: {count} approvals pending from {}; review before approving",
                redact::escape(&row.agent)
            ));
        }
    }
    if let Some((notice, suppressed)) = app.board.approvals.notice() {
        out.push(format!(
            "{} (+{suppressed} suppressed)",
            redact::escape(notice)
        ));
    }
    out
}

fn status_line(app: &App) -> Option<String> {
    match &app.mode {
        Mode::Filter => Some(format!("filter: {}_", app.input)),
        Mode::Note { request_id } => Some(format!(
            "note for {}: {}_",
            redact::escape(request_id),
            app.input
        )),
        Mode::Confirm(Confirm::Approve { request_id }) => Some(format!(
            "approve {}? y confirms, Esc cancels",
            redact::escape(request_id)
        )),
        Mode::Confirm(Confirm::Cancel { agent, task }) => Some(format!(
            "cancel task {} on {}? y confirms, Esc cancels",
            redact::escape(task),
            redact::escape(agent)
        )),
        Mode::Normal | Mode::Help => app
            .notice
            .clone()
            .or_else(|| (!app.filter.is_empty()).then(|| format!("filter: {}", app.filter))),
    }
}

const fn footer_hint(app: &App) -> &'static str {
    match app.mode {
        Mode::Help => "any key closes help",
        Mode::Filter => "Enter apply  Esc cancel",
        Mode::Note { .. } => "Enter save  Esc cancel",
        Mode::Confirm(_) => "y confirm  Esc cancel",
        Mode::Normal => {
            "1-6 screens  Tab next  j/k move  Enter open  / filter  a approvals  r refresh  ? help  q quit"
        }
    }
}

fn body_lines(app: &App, now: u64) -> Vec<String> {
    match app.screen {
        Screen::Agents => agents_lines(app),
        Screen::Tasks => tasks_lines(app),
        Screen::Task => task_lines(app),
        Screen::Approvals => match &app.detail {
            Some(request_id) => card_lines(app, request_id, now),
            None => approval_lines(app),
        },
        Screen::Budget => budget_lines(app),
        Screen::Audit => audit_lines(app),
    }
}

const fn marker(selected: bool) -> &'static str {
    if selected { ">" } else { " " }
}

fn field(label: &str, value: &str) -> String {
    format!("{label:<20} {value}")
}

fn agents_lines(app: &App) -> Vec<String> {
    let mut out = vec![format!(
        "  {:<20} {:<16} {:<9} {:<6} {:<9} {}",
        "AGENT", "NAME", "PRESENCE", "TASKS", "APPROVALS", "APPROVAL MODE"
    )];
    let visible = app.visible_agents();
    if visible.is_empty() {
        out.push("  no agents discovered".to_owned());
    }
    for (index, id) in visible.iter().enumerate() {
        let Some(row) = app.board.agents.get(id) else {
            continue;
        };
        let presence = row.presence.map_or("absent", Presence::label);
        let open = app.board.approvals.open_count(id);
        let mode = if row.approval_mode.is_empty() {
            "-".to_owned()
        } else {
            redact::escape(&row.approval_mode)
        };
        let danger = if row.dangerous {
            "  DANGEROUS MODE"
        } else {
            ""
        };
        out.push(format!(
            "{} {:<20} {:<16} {:<9} {:<6} {:<9} {}{}",
            marker(index == app.selected),
            redact::escape(&row.agent),
            redact::escape(&row.card_name),
            presence,
            row.active_tasks,
            open,
            mode,
            danger
        ));
    }
    out
}

fn tasks_lines(app: &App) -> Vec<String> {
    let mut out = vec![format!(
        "  {:<22} {:<14} {:<16} {:<8} {:<8} {}",
        "TASK", "AGENT", "STATE", "SEQ", "MISSING", "STATUS"
    )];
    let visible = app.visible_tasks();
    if visible.is_empty() {
        out.push("  no tasks".to_owned());
    }
    for (index, id) in visible.iter().enumerate() {
        let Some(row) = app.board.tasks.get(id) else {
            continue;
        };
        let indent = "  ".repeat(app.board.tasks.depth(id).min(4));
        out.push(format!(
            "{} {indent}{:<22} {:<14} {:<16} {:<8} {:<8} {}",
            marker(index == app.selected),
            redact::escape(&row.id),
            redact::escape(&row.agent),
            row.state_label(),
            row.last_seq,
            row.missing_seqs,
            redact::escape(&row.status_text)
        ));
    }
    out
}

fn task_lines(app: &App) -> Vec<String> {
    let Some(id) = app.task_focus.as_deref() else {
        return vec!["no task selected (press Enter on the Tasks screen)".to_owned()];
    };
    let Some(row) = app.board.tasks.get(id) else {
        return vec![format!("task {} is not known", redact::escape(id))];
    };
    let tasks = &app.board.tasks;
    let parent = tasks
        .parent_of(id)
        .map_or_else(|| "none".to_owned(), redact::escape);
    let last_trip = row
        .last_trip
        .as_deref()
        .map_or_else(|| "none".to_owned(), redact::note);
    vec![
        field("Task", &redact::escape(&row.id)),
        field("Agent", &redact::escape(&row.agent)),
        field("Context", &redact::escape(&row.context_id)),
        field("State", row.state_label()),
        field("Status", &redact::escape(&row.status_text)),
        field("Parent", &parent),
        field("Depth", &tasks.depth(id).to_string()),
        field("Last seq", &row.last_seq.to_string()),
        field("Missing seqs", &row.missing_seqs.to_string()),
        field("Artifacts", &row.artifacts.to_string()),
        field(
            "Tokens used/max",
            &pair(row.budget.tokens_used, row.budget.max_tokens),
        ),
        field(
            "Wall ms used/max",
            &pair(row.budget.wall_ms_used, row.budget.max_wall_ms),
        ),
        field("Last trip", &last_trip),
        field("Updated ms", &row.updated_ms.to_string()),
    ]
}

fn approval_lines(app: &App) -> Vec<String> {
    let entries = app.board.approvals.entries();
    if entries.is_empty() {
        return vec!["no approval requests".to_owned()];
    }
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let request = entry.request();
            format!(
                "{} {:<24} agent={:<12} tool={:<14} tier={:<16} {}",
                marker(index == app.selected),
                redact::escape(&request.request_id),
                redact::escape(&entry.agent),
                redact::escape(&request.tool),
                tier_label(request.tier),
                phase_label(&entry.phase)
            )
        })
        .collect()
}

/// The approval card in RFC-0012 4.10.1 order. Missing fields render as `missing`.
fn card_lines(app: &App, request_id: &str, now: u64) -> Vec<String> {
    let Some(entry) = app.board.approvals.get(request_id) else {
        return vec![format!("request {} is closed", redact::escape(request_id))];
    };
    let request = entry.request();
    let chain = if request.chain.is_empty() {
        "missing".to_owned()
    } else {
        request
            .chain
            .iter()
            .map(|agent| redact::escape(agent))
            .collect::<Vec<_>>()
            .join(" > ")
    };
    let budget = app
        .board
        .tasks
        .get(&request.task_id)
        .map_or_else(|| "missing".to_owned(), |row| budget_remaining(&row.budget));
    let expiry = match entry.expires_ms() {
        Some(at) if at > now => format!("in {}s", (at - now).div_ceil(1000)),
        Some(_) => "expired".to_owned(),
        None => "missing".to_owned(),
    };
    let scope = if request.options.is_empty() {
        "missing".to_owned()
    } else {
        request
            .options
            .iter()
            .map(|option| {
                format!(
                    "{} ({})",
                    scope_label(option.kind),
                    redact::escape(&option.option_id)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let dwell = (entry.seen_ms + DWELL_MS).saturating_sub(now);
    let approve_hint = if dwell == 0 {
        "approve enabled".to_owned()
    } else {
        format!("approve enabled in {dwell}ms")
    };

    let mut out = vec![
        field("Request id", &redact::escape(&request.request_id)),
        field("Requesting agent", &redact::escape(&request.requester)),
        field(
            "Task chain",
            &format!("{chain} -> task {}", redact::escape(&request.task_id)),
        ),
        field("Tool", &redact::escape(&request.tool)),
        field("Argument summary", &redact::summary(&request.summary)),
        field("Working directory", &or_missing(&request.working_directory)),
        field("Workspace mode", &or_missing(&request.workspace_mode)),
        field("Risk tier", tier_label(request.tier)),
        field("Budget remaining", &budget),
        field("Requested scope", &scope),
        field("Expiry", &expiry),
        field("Signature", "not verified"),
        field("Phase", phase_label(&entry.phase)),
    ];
    if let Some(note) = &entry.note {
        out.push(field("Note (local)", &redact::note(note)));
    }
    out.push(String::new());
    out.push(format!(
        "{approve_hint}   y approve   n deny   N note   Esc back"
    ));
    out
}

fn budget_lines(app: &App) -> Vec<String> {
    let rows = app.board.tasks.ordered();
    if rows.is_empty() {
        return vec!["no tasks with budget data".to_owned()];
    }
    rows.iter()
        .map(|row| {
            format!(
                "  {:<22} tokens {} remaining {}   wall ms {} remaining {}",
                redact::escape(&row.id),
                pair(row.budget.tokens_used, row.budget.max_tokens),
                opt(row.budget.tokens_remaining()),
                pair(row.budget.wall_ms_used, row.budget.max_wall_ms),
                opt(row.budget.wall_ms_remaining())
            )
        })
        .collect()
}

fn audit_lines(app: &App) -> Vec<String> {
    if app.board.audit.is_empty() {
        return vec!["no audit records yet".to_owned()];
    }
    app.board
        .audit
        .newest_first()
        .map(|row| {
            let time = if row.time_valid {
                redact::escape(&row.time)
            } else {
                format!("{} (unverified)", redact::escape(&row.time))
            };
            format!(
                "{time} #{} {} {} agent={} task={} {}",
                row.seq,
                redact::escape(&row.emitter),
                redact::escape(&row.kind),
                redact::escape(&row.agent),
                redact::escape(&row.task),
                redact::summary(&row.reason)
            )
        })
        .collect()
}

fn help_lines() -> Vec<String> {
    [
        "Keys",
        "1-6         screens: Agents, Tasks, Task, Approvals, Budget, Audit",
        "Tab/S-Tab   next or previous screen",
        "j/k, arrows move the selection",
        "Enter       open the selected approval or task; on Agents, filter Tasks by agent",
        "Esc         back",
        "/           filter the current list",
        "a           Approvals screen",
        "y           approve (card only, after dwell; exec and delete need a second y)",
        "n           deny (list or card, no dwell)",
        "N           local note on the open approval (280 chars, never sent)",
        "c           cancel the selected task (asks for y; needs the link)",
        "r           refresh now",
        "?           this help",
        "q, Ctrl+C   quit the console (agents keep running)",
    ]
    .iter()
    .map(|line| (*line).to_owned())
    .collect()
}

pub(crate) fn tier_label(raw: i32) -> &'static str {
    match ApprovalTier::try_from(raw) {
        Ok(ApprovalTier::Read) => "read",
        Ok(ApprovalTier::Write) => "write",
        Ok(ApprovalTier::Exec) => "exec",
        Ok(ApprovalTier::DeleteOrMove) => "delete-or-move",
        Ok(ApprovalTier::Unspecified) | Err(_) => "missing",
    }
}

fn or_missing(value: &str) -> String {
    if value.is_empty() {
        "missing".to_owned()
    } else {
        redact::escape(value)
    }
}

fn scope_label(raw: i32) -> &'static str {
    match ApprovalOptionKind::try_from(raw) {
        Ok(ApprovalOptionKind::AllowOnce) => "once",
        Ok(ApprovalOptionKind::AllowAlways) => "task",
        Ok(ApprovalOptionKind::RejectOnce) => "reject once",
        Ok(ApprovalOptionKind::RejectAlways) => "reject task",
        Ok(ApprovalOptionKind::Unspecified) | Err(_) => "unknown",
    }
}

fn budget_remaining(budget: &Budget) -> String {
    let tokens = budget
        .tokens_remaining()
        .map_or_else(|| "tokens missing".to_owned(), |n| format!("{n} tokens"));
    let wall = budget
        .wall_ms_remaining()
        .map_or_else(|| "wall missing".to_owned(), |n| format!("{n} ms"));
    format!("{tokens}, {wall}")
}

fn pair(used: Option<u64>, max: Option<u64>) -> String {
    format!("{}/{}", opt(used), opt(max))
}

fn opt(value: Option<u64>) -> String {
    value.map_or_else(|| "missing".to_owned(), |v| v.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::model::agents::{Observed, Presence};
    use crate::model::approvals::fixtures::{pending, pending_from, request};
    use crate::model::audit::Row;

    const EXPIRES_SECS: i64 = 1_000_000;

    /// Renders the whole console into a 120x40 test buffer, one string per row.
    fn screen_text(app: &App, now: u64) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
        terminal.draw(|frame| draw(frame, app, now)).expect("draw");
        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area.width);
        buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn observed(agent: &str, presence: Option<Presence>, mode: Option<&str>) -> Observed {
        Observed {
            agent: agent.to_owned(),
            card_name: Some(format!("{agent} card")),
            card_version: Some("1.0".to_owned()),
            presence,
            active_tasks: 2,
            approval_mode: mode.map(str::to_owned),
        }
    }

    #[test]
    fn req_0012_044_draw_shows_header_banner_and_agent_columns() {
        let app = App::new(true);
        let text = screen_text(&app, 0);
        assert!(text.contains("a2a-tui"));
        assert!(text.contains("read-only"));
        assert!(text.contains("AGENT"));
        assert!(text.contains("CONNECTING"));
    }

    #[test]
    fn req_0012_043_read_only_banner_names_the_reason() {
        let mut app = App::new(true);
        app.read_only_reason = Some("no operator key given (--key)".to_owned());
        let lines = banners(&app, 0);
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("READ-ONLY") && line.contains("--key"))
        );
    }

    #[test]
    fn req_0012_050_link_labels_name_every_state() {
        assert_eq!(link_label(LinkState::Connecting), "connecting");
        assert_eq!(link_label(LinkState::Healthy), "healthy");
        assert_eq!(link_label(LinkState::Degraded), "degraded");
        assert_eq!(link_label(LinkState::Disconnected), "disconnected");
    }

    #[test]
    fn req_0004_013_presence_labels_and_offline_agents_are_shown() {
        let mut app = App::new(false);
        app.board.apply_agents(
            0,
            vec![
                observed("omp", Some(Presence::Draining), None),
                observed("claude", None, None),
            ],
        );
        let lines = agents_lines(&app).join("\n");
        assert!(lines.contains("DRAINING"));
        assert!(lines.contains("absent"));
    }

    #[test]
    fn req_0012_051_dangerous_mode_is_flagged_on_the_agent_row() {
        let mut app = App::new(false);
        app.board.apply_agents(
            0,
            vec![
                observed("omp", Some(Presence::Ready), Some("yolo")),
                observed("safe", Some(Presence::Ready), Some("ask")),
            ],
        );
        let lines = agents_lines(&app);
        let omp = lines
            .iter()
            .find(|line| line.contains("omp card"))
            .expect("omp row");
        assert!(omp.contains("DANGEROUS MODE"));
        let safe = lines
            .iter()
            .find(|line| line.contains("safe card"))
            .expect("safe row");
        assert!(!safe.contains("DANGEROUS MODE"));
    }

    #[test]
    fn req_0006_004_card_lists_fields_in_rfc_order() {
        let mut app = App::new(false);
        app.board
            .approvals
            .observe("omp", pending("r1", EXPIRES_SECS), 0);
        let card = card_lines(&app, "r1", 0);
        let labels = [
            "Request id",
            "Requesting agent",
            "Task chain",
            "Tool",
            "Argument summary",
            "Working directory",
            "Workspace mode",
            "Risk tier",
            "Budget remaining",
            "Requested scope",
            "Expiry",
            "Signature",
        ];
        let mut last = 0;
        for label in labels {
            let index = card
                .iter()
                .position(|line| line.starts_with(label))
                .expect(label);
            assert!(index >= last, "{label} is out of order");
            last = index;
        }
    }

    #[test]
    fn req_0006_004_fields_the_request_lacks_read_missing() {
        let mut app = App::new(false);
        app.board
            .approvals
            .observe("omp", pending("r1", EXPIRES_SECS), 0);
        let card = card_lines(&app, "r1", 0);
        let field_line = |label: &str| {
            card.iter()
                .find(|line| line.starts_with(label))
                .expect(label)
                .clone()
        };
        assert!(field_line("Task chain").contains("missing"));
        assert!(field_line("Working directory").contains("/work/app"));
        assert!(field_line("Workspace mode").contains("per-task"));
        assert!(field_line("Budget remaining").contains("missing"));
        assert!(field_line("Signature").contains("not verified"));
    }

    #[test]
    fn req_0006_006_empty_directory_and_mode_read_missing() {
        let mut app = App::new(false);
        let mut req = request("r1", EXPIRES_SECS);
        req.working_directory.clear();
        req.workspace_mode.clear();
        app.board.approvals.observe("omp", pending_from(&req), 0);
        let card = card_lines(&app, "r1", 0);
        for label in ["Working directory", "Workspace mode"] {
            let line = card
                .iter()
                .find(|line| line.starts_with(label))
                .expect(label);
            assert!(line.contains("missing"), "{line}");
        }
    }

    #[test]
    fn req_0012_046_card_counts_down_the_dwell_before_approve() {
        let mut app = App::new(false);
        app.board
            .approvals
            .observe("omp", pending("r1", EXPIRES_SECS), 1000);
        let early = card_lines(&app, "r1", 1000).join("\n");
        assert!(early.contains("approve enabled in 2000ms"));
        let later = card_lines(&app, "r1", 3000).join("\n");
        assert!(later.contains("approve enabled   y approve"));
    }

    #[test]
    fn req_0013_022_card_escapes_remote_text() {
        let mut app = App::new(false);
        let mut req = request("r1", EXPIRES_SECS);
        req.summary = "rm -rf /\u{1b}[2J\nTOKEN=abc".to_owned();
        req.requester = "omp\u{1b}".to_owned();
        app.board.approvals.observe("omp", pending_from(&req), 0);
        let card = card_lines(&app, "r1", 0);
        assert!(card.iter().all(|line| !line.contains('\u{1b}')));
    }

    #[test]
    fn req_0013_005_unverified_time_is_marked_on_the_audit_row() {
        let mut app = App::new(false);
        app.board.audit.push(Row {
            time: "2026-10-26T09:14:03.218Z".to_owned(),
            time_valid: false,
            seq: 7,
            emitter: "omp".to_owned(),
            kind: "task.created".to_owned(),
            agent: "omp".to_owned(),
            task: "t1".to_owned(),
            reason: "ok".to_owned(),
        });
        let lines = audit_lines(&app);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("(unverified)"));
        assert!(lines[0].contains("#7"));
    }

    #[test]
    fn req_0013_046_audit_reason_renders_as_one_clean_line() {
        let mut app = App::new(false);
        app.board.audit.push(Row {
            time: "2026-10-26T09:14:03.218Z".to_owned(),
            time_valid: true,
            seq: 1,
            emitter: "gateway".to_owned(),
            kind: "approval.denied".to_owned(),
            agent: "omp".to_owned(),
            task: "t1".to_owned(),
            reason: "failed\n\u{1b}[31mbad\u{1b}[0m".to_owned(),
        });
        let lines = audit_lines(&app);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains('\u{1b}'));
        assert!(!lines[0].contains('\n'));
    }

    #[test]
    fn req_0012_044_help_lists_the_navigation_keys() {
        let help = help_lines().join("\n");
        for key in ["1-6", "Tab", "Enter", "Esc", "y", "n", "q", "?"] {
            assert!(help.contains(key), "help is missing {key}");
        }
    }
}
