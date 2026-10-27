//! Permission denials (RFC-0010 section 4.8, RFC-0011 0011-041 and 0011-050).
//!
//! The NATS server drops a publish that the identity may not make and reports it
//! asynchronously as `-ERR 'Permissions Violation for Publish to "<subject>"'`. A
//! dropped request never gets a reply, so without this module it would surface as
//! `Timeout`. The connection's event callback forwards the report text here, and
//! [`guard`] races an operation against the matching report: a denied operation
//! fails with `PermissionDenied`.

use std::future::Future;
use std::pin::pin;
use std::time::Duration;

use bus_api::error::BusError;
use tokio::sync::broadcast;

/// Denial reports buffered per connection before the oldest are dropped.
pub(crate) const CHANNEL_CAPACITY: usize = 64;

/// The sender side of a connection's denial reports (server error text).
pub(crate) type Denials = broadcast::Sender<String>;

/// The server refused an operation: the identity has no permission for its subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Denied;

impl From<Denied> for BusError {
    fn from(_: Denied) -> Self {
        BusError::PermissionDenied
    }
}

/// The subject of a publish-permission report, or `None` for any other server error.
pub(crate) fn publish_denied_subject(text: &str) -> Option<&str> {
    text.trim_matches('\'')
        .strip_prefix("Permissions Violation for Publish to \"")?
        .strip_suffix('"')
}

/// Runs `op` until it completes or a publish denial for a subject accepted by
/// `covers` is reported first. The subscription is taken before `op` is polled, so
/// no report sent after the call starts is missed.
pub(crate) async fn guard<T>(
    denials: &Denials,
    covers: impl Fn(&str) -> bool,
    op: impl Future<Output = T>,
) -> Result<T, Denied> {
    // The clone keeps the channel open while the operation runs, so `recv` cannot
    // report `Closed` and the loop below always waits on the operation or a report.
    let _keepalive = denials.clone();
    let mut reports = denials.subscribe();
    let mut op = pin!(op);
    loop {
        tokio::select! {
            out = &mut op => return Ok(out),
            report = reports.recv() => match report {
                Ok(text) if publish_denied_subject(&text).is_some_and(&covers) => {
                    return Err(Denied);
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {}
            },
        }
    }
}

/// Waits up to `window` for a publish denial that `covers`, among reports sent after
/// `reports` was subscribed. Used after a core publish is flushed: the server reports
/// a dropped publish before it answers the flush, but the event task delivers the
/// report a moment later.
pub(crate) async fn denied_within(
    reports: &mut broadcast::Receiver<String>,
    covers: impl Fn(&str) -> bool,
    window: Duration,
) -> bool {
    let wait = async {
        loop {
            match reports.recv().await {
                Ok(text) if publish_denied_subject(&text).is_some_and(&covers) => return true,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    };
    tokio::time::timeout(window, wait).await.unwrap_or(false)
}

/// How long a flushed core publish waits for its denial report (see [`denied_within`]).
pub(crate) const CORE_PUBLISH_REPORT_WINDOW: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_publish_violation_with_and_without_server_quotes() {
        assert_eq!(
            publish_denied_subject("Permissions Violation for Publish to \"suite.x\""),
            Some("suite.x")
        );
        assert_eq!(
            publish_denied_subject("'Permissions Violation for Publish to \"a.b\"'"),
            Some("a.b")
        );
    }

    #[test]
    fn ignores_other_server_errors() {
        assert_eq!(publish_denied_subject("Maximum Payload Violation"), None);
        assert_eq!(
            publish_denied_subject("Permissions Violation for Subscription to \"x\""),
            None
        );
    }

    #[tokio::test]
    async fn guard_reports_denial_for_covered_subject() {
        let (denials, _) = broadcast::channel(CHANNEL_CAPACITY);
        let sender = denials.clone();
        let never = std::future::pending::<()>();
        let report = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = sender.send("Permissions Violation for Publish to \"forbidden.x\"".to_owned());
        });
        let outcome = guard(&denials, |s| s == "forbidden.x", never).await;
        report.await.ok();
        assert_eq!(outcome, Err(Denied));
    }

    #[tokio::test]
    async fn guard_ignores_denials_for_other_subjects() {
        let (denials, _) = broadcast::channel(CHANNEL_CAPACITY);
        let sender = denials.clone();
        let op = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            7_u8
        };
        let report = tokio::spawn(async move {
            let _ = sender.send("Permissions Violation for Publish to \"other\"".to_owned());
        });
        let outcome = guard(&denials, |s| s == "forbidden.x", op).await;
        report.await.ok();
        assert_eq!(outcome, Ok(7));
    }
}
