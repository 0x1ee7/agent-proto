//! Audit tail (RFC-0013): the newest audit events the console has seen.

use std::collections::VecDeque;

use a2a_types::proto::a2abus;

use crate::redact;

/// Rows kept in memory; older rows are dropped.
pub const TAIL_CAPACITY: usize = 1_000;

/// One display row. Every field is already escaped for the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub time: String,
    pub time_valid: bool,
    pub seq: u64,
    pub emitter: String,
    pub kind: String,
    pub agent: String,
    pub task: String,
    pub reason: String,
}

impl Row {
    /// One plain line for a log stream: `time seq=N emitter KIND agent=A task=T reason=R`.
    /// Empty fields are left out. Fields are already escaped, so the line has no control
    /// characters and no line break.
    #[must_use]
    pub fn log_line(&self) -> String {
        let mut line = format!(
            "{} seq={} {} {}",
            self.time, self.seq, self.emitter, self.kind
        );
        for (label, value) in [
            ("agent", &self.agent),
            ("task", &self.task),
            ("reason", &self.reason),
        ] {
            if !value.is_empty() {
                line.push_str(&format!(" {label}={value}"));
            }
        }
        line
    }
}

#[derive(Debug, Default)]
pub struct Tail {
    rows: VecDeque<Row>,
}

impl Tail {
    pub fn push(&mut self, row: Row) {
        if self.rows.len() == TAIL_CAPACITY {
            self.rows.pop_front();
        }
        self.rows.push_back(row);
    }

    /// Rows newest first.
    pub fn newest_first(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().rev()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Builds the display row for an audit event.
#[must_use]
pub fn row_from(event: &a2abus::AuditEvent) -> Row {
    let kind = a2abus::AuditEventType::try_from(event.r#type)
        .map_or_else(|_| "UNKNOWN".to_owned(), |t| t.as_str_name().to_owned());
    Row {
        time: display(&event.time),
        time_valid: a2a_audit::clock::is_audit_timestamp(&event.time),
        seq: event.seq,
        emitter: display(&event.emitter),
        kind: display(&kind),
        agent: display(&event.agent_id),
        task: display(&event.task_id),
        reason: redact::summary(&event.reason),
    }
}

fn display(text: &str) -> String {
    redact::truncate_chars(&redact::escape(text), 128)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn row(seq: u64) -> Row {
        Row {
            time: String::new(),
            time_valid: true,
            seq,
            emitter: String::new(),
            kind: String::new(),
            agent: String::new(),
            task: String::new(),
            reason: String::new(),
        }
    }

    #[test]
    fn req_0013_tail_keeps_newest_rows_within_capacity() {
        let mut tail = Tail::default();
        for seq in 0..(TAIL_CAPACITY as u64 + 5) {
            tail.push(row(seq));
        }
        assert_eq!(tail.len(), TAIL_CAPACITY);
        let first = tail.newest_first().next().expect("row");
        assert_eq!(first.seq, TAIL_CAPACITY as u64 + 4);
        let last = tail.newest_first().last().expect("row");
        assert_eq!(last.seq, 5);
    }

    #[test]
    fn req_0013_tail_rows_escape_control_characters() {
        let event = a2abus::AuditEvent {
            agent_id: "omp\u{1b}[2J".to_owned(),
            time: "not-a-time".to_owned(),
            ..Default::default()
        };
        let built = row_from(&event);
        assert!(!built.agent.chars().any(char::is_control));
        assert!(!built.time_valid);
        assert_eq!(built.kind, "AUDIT_EVENT_TYPE_UNSPECIFIED");
    }

    #[test]
    fn req_0013_log_line_is_one_line_and_omits_empty_fields() {
        let event = a2abus::AuditEvent {
            time: "2026-10-28T21:05:00.000Z".to_owned(),
            seq: 7,
            emitter: "omp".to_owned(),
            agent_id: "omp".to_owned(),
            task_id: String::new(),
            reason: "line one\nline\u{1b}[2J two".to_owned(),
            ..Default::default()
        };
        let line = row_from(&event).log_line();
        assert!(!line.chars().any(char::is_control), "{line:?}");
        assert!(
            line.starts_with("2026-10-28T21:05:00.000Z seq=7 omp "),
            "{line}"
        );
        assert!(line.contains(" agent=omp"), "{line}");
        assert!(!line.contains("task="), "{line}");
        assert!(line.contains(" reason=line one"), "{line}");
    }
}
