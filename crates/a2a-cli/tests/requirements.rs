//! Requirement tests for a2a-cli: command behavior on bus-mem with a fake worker.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::time::Duration;

use a2a_cli::app;
use a2a_cli::approvals;
use a2a_cli::cli::{Command, DecideArgs, DiscoverArgs, Scope, SendArgs};
use a2a_cli::commands::{self, print_message, print_task};
use a2a_cli::error::{CliError, Exit, classify};
use a2a_client::{ApprovalError, ClientError};
use a2a_core::approval::{ALLOW_ONCE_ID, REJECT_ONCE_ID};
use a2a_core::envelope::RequestBody;
use a2a_core::errors::WireError;
use a2a_core::subject::Op;
use a2a_types::proto::a2a as pb;
use a2a_types::vocab::ext::APPROVAL;
use bus_api::bus::RequestStream;
use bus_api::error::BusError;
use common::{Fixture, NOW_MS, Streams};
use pbjson_types::{Struct, Value as PbValue};
use serde_json::json;

/// Expiry 300 s after NOW_MS, as RFC 3339 (2023-11-14T22:18:20Z).
const EXPIRES_AT: &str = "2023-11-14T22:18:20Z";
/// Expiry before NOW_MS (2023-11-14T22:00:00Z).
const EXPIRED_AT: &str = "2023-11-14T22:00:00Z";

fn task(id: &str, state: pb::TaskState) -> pb::Task {
    pb::Task {
        id: id.to_owned(),
        context_id: "ctx-1".to_owned(),
        status: Some(pb::TaskStatus {
            state: state as i32,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `SendMessage` answer carrying `task`.
fn send_answer(task: pb::Task) -> pb::SendMessageResponse {
    pb::SendMessageResponse {
        payload: Some(pb::send_message_response::Payload::Task(task)),
    }
}

/// `ListTasks` answer with the given tasks.
fn list_answer(tasks: Vec<pb::Task>) -> pb::ListTasksResponse {
    pb::ListTasksResponse {
        tasks,
        ..Default::default()
    }
}

/// Worker waits are bounded so a request that never arrives fails the test instead of hanging.
const WORKER_WAIT: Duration = Duration::from_secs(5);

/// Input-required task whose status message carries an approval request.
fn approval_task(task_id: &str, expires_at: &str) -> pb::Task {
    let request = json!({
        "requestId": "req-1",
        "taskId": task_id,
        "tool": "shell",
        "summary": "remove build directory",
        "requester": "worker-a",
        "nonce": "0123456789abcdef0123",
        "options": [
            {"optionId": ALLOW_ONCE_ID, "name": "Allow once", "kind": "APPROVAL_OPTION_KIND_ALLOW_ONCE"},
            {"optionId": REJECT_ONCE_ID, "name": "Reject once", "kind": "APPROVAL_OPTION_KIND_REJECT_ONCE"}
        ],
        "expiresAt": expires_at
    });
    let approval: Struct = serde_json::from_value(request).expect("approval struct");
    let mut metadata = Struct::default();
    metadata.fields.insert(
        APPROVAL.to_owned(),
        PbValue {
            kind: Some(pbjson_types::value::Kind::StructValue(approval)),
        },
    );
    pb::Task {
        id: task_id.to_owned(),
        context_id: "ctx-1".to_owned(),
        status: Some(pb::TaskStatus {
            state: pb::TaskState::InputRequired as i32,
            message: Some(pb::Message {
                metadata: Some(metadata),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Pulls the approval decision out of a received `SendMessage` request.
fn decision_of(request: &RequestBody) -> serde_json::Value {
    let RequestBody::SendMessage(send) = request else {
        panic!("expected SendMessage");
    };
    let message = send.message.as_ref().expect("message");
    let part = message.parts.first().expect("decision part");
    let Some(pb::part::Content::Data(value)) = &part.content else {
        panic!("decision is not a data part");
    };
    serde_json::to_value(value).expect("decision json")
}

fn discover_args(agent: Option<&str>) -> DiscoverArgs {
    DiscoverArgs {
        agent: agent.map(str::to_owned),
        skill: None,
        tag: None,
        extension: None,
        input_mode: None,
        output_mode: None,
        include_offline: false,
    }
}

fn decide_args(agent: Option<&str>, yes: bool, note: Option<&str>) -> DecideArgs {
    DecideArgs {
        request_id: "req-1".to_owned(),
        scope: Scope::Once,
        yes,
        agent: agent.map(str::to_owned),
        note: note.map(str::to_owned),
    }
}

// [0012-054] An invalid agent id is a usage error, decided before any bus traffic.
#[tokio::test]
async fn req_0012_054_invalid_agent_is_usage_before_bus_traffic() {
    let fx = Fixture::new();
    let session = fx.session(Vec::new(), true);
    let mut streams = Streams::new("");
    let mut io = streams.io(false, false);
    let args = SendArgs {
        agent: "Bad Id!".to_owned(),
        text: "hello".to_owned(),
        context: None,
        wait: false,
        follow: false,
    };
    let error = commands::send(&session, &args, &mut io)
        .await
        .expect_err("usage error");
    assert_eq!(error.exit(), Exit::Usage);
    assert!(matches!(error, CliError::Usage(_)));
    assert!(
        streams.out.is_empty(),
        "nothing printed before the usage error"
    );
}

// [0012-054] `discover --agent` validates the id before a query is built.
#[test]
fn req_0012_054_discover_rejects_invalid_agent_id() {
    let error = commands::discover_query(&discover_args(Some("Bad Id!"))).expect_err("usage");
    assert_eq!(error.exit(), Exit::Usage);
    assert!(commands::discover_query(&discover_args(Some("worker-a"))).is_ok());
}

// [0012-054] `start` and `stop` validate their agent id and never run without a control channel.
#[test]
fn req_0012_054_start_stop_validate_then_fail_without_control_channel() {
    let fx = Fixture::new();
    let settings = fx.settings(Vec::new());
    let mut streams = Streams::new("");
    let mut io = streams.io(false, false);

    let bad = app::offline(
        &Command::Start {
            agent: "Bad Id!".to_owned(),
        },
        &settings,
        &mut io,
    )
    .expect_err("usage");
    assert_eq!(bad.exit(), Exit::Usage);

    let start = app::offline(
        &Command::Start {
            agent: "worker-a".to_owned(),
        },
        &settings,
        &mut io,
    )
    .expect_err("no control channel");
    assert_eq!(start.exit(), Exit::Failure);

    let stop = app::offline(&Command::Stop { agent: None }, &settings, &mut io)
        .expect_err("no control channel");
    assert_eq!(stop.exit(), Exit::Failure);
    assert!(streams.out.is_empty());
}

// [0012-055] Every failure class maps to its documented exit code.
#[test]
fn req_0012_055_exit_codes_are_stable() {
    assert_eq!(Exit::Success.code(), 0);
    assert_eq!(Exit::Failure.code(), 1);
    assert_eq!(Exit::Usage.code(), 2);
    assert_eq!(Exit::NotFound.code(), 3);
    assert_eq!(Exit::Rejected.code(), 4);
    assert_eq!(Exit::Timeout.code(), 5);
    assert_eq!(Exit::Unavailable.code(), 6);

    assert_eq!(CliError::Usage("x".to_owned()).exit(), Exit::Usage);
    assert_eq!(CliError::NotFound("x".to_owned()).exit(), Exit::NotFound);
    assert_eq!(CliError::Rejected("x".to_owned()).exit(), Exit::Rejected);
    assert_eq!(CliError::Timeout("x".to_owned()).exit(), Exit::Timeout);
    assert_eq!(
        CliError::Unavailable("x".to_owned()).exit(),
        Exit::Unavailable
    );
    assert_eq!(CliError::Failed("x".to_owned()).exit(), Exit::Failure);
}

// [0012-055] Requester-runtime failures are classified by cause.
#[test]
fn req_0012_055_client_errors_are_classified() {
    assert_eq!(
        classify(&ClientError::Bus(BusError::Timeout)),
        Exit::Timeout
    );
    assert_eq!(
        classify(&ClientError::Bus(BusError::NoResponder)),
        Exit::Unavailable
    );
    assert_eq!(
        classify(&ClientError::Bus(BusError::PermissionDenied)),
        Exit::Rejected
    );
    assert_eq!(
        classify(&ClientError::Wire(WireError::TaskNotFound, None)),
        Exit::NotFound
    );
    assert_eq!(
        classify(&ClientError::Approval(ApprovalError::Expired)),
        Exit::Rejected
    );
    assert_eq!(
        CliError::from(ClientError::Wire(WireError::TaskNotFound, None)).exit(),
        Exit::NotFound
    );
}

// [0012-055] `send --wait` on a rejected task exits with the rejected class, not success.
#[tokio::test]
async fn req_0012_055_send_wait_rejection_exits_rejected() {
    let fx = Fixture::new();
    let session = fx.session(vec![fx.worker_id.clone()], true);
    let mut sends = fx.serve(Op::Send).await;
    let mut streams = Streams::new("");
    let mut io = streams.io(false, false);
    let args = SendArgs {
        agent: "worker-a".to_owned(),
        text: "do it".to_owned(),
        context: None,
        wait: true,
        follow: false,
    };
    let worker = async {
        let inbound = tokio::time::timeout(WORKER_WAIT, fx.next_inbound(&mut sends))
            .await
            .expect("send reached the worker");
        fx.reply(
            inbound,
            &send_answer(task("t-rej", pb::TaskState::Rejected)),
        );
    };
    let (outcome, ()) = tokio::join!(commands::send(&session, &args, &mut io), worker);
    let exit = match outcome {
        Ok(exit) => exit,
        Err(error) => error.exit(),
    };
    assert_eq!(exit, Exit::Rejected);
}

// [0012-056] `--json` prints proto-JSON (camelCase, enum names); text mode prints a summary line.
#[test]
fn req_0012_056_json_output_is_proto_json() {
    let mut streams = Streams::new("");
    {
        let mut io = streams.io(false, true);
        print_task(&mut io, &task("t-1", pb::TaskState::Completed)).expect("print task");
    }
    let value: serde_json::Value = serde_json::from_str(&streams.out_text()).expect("json");
    assert_eq!(value["id"], "t-1");
    assert_eq!(value["contextId"], "ctx-1");
    assert_eq!(value["status"]["state"], "TASK_STATE_COMPLETED");

    let mut streams = Streams::new("");
    {
        let mut io = streams.io(false, true);
        let message = pb::Message {
            message_id: "m-1".to_owned(),
            ..Default::default()
        };
        print_message(&mut io, &message).expect("print message");
    }
    let value: serde_json::Value = serde_json::from_str(&streams.out_text()).expect("json");
    assert_eq!(value["messageId"], "m-1");

    let mut streams = Streams::new("");
    {
        let mut io = streams.io(false, false);
        print_task(&mut io, &task("t-2", pb::TaskState::Working)).expect("print task");
    }
    assert_eq!(streams.out_text(), "task t-2 TASK_STATE_WORKING\n");
}

// [0012-057] Without a terminal, a decision needs `--yes`: the card is shown, nothing is decided.
#[tokio::test]
async fn req_0012_057_non_tty_decision_needs_yes_and_sends_nothing() {
    let fx = Fixture::new();
    let session = fx.session(vec![fx.worker_id.clone()], true);
    let mut lists = fx.serve(Op::List).await;
    let mut sends = fx.serve(Op::Send).await;
    let mut streams = Streams::new("");
    let args = decide_args(Some("worker-a"), false, None);
    let worker = async {
        let inbound = tokio::time::timeout(WORKER_WAIT, fx.next_inbound(&mut lists))
            .await
            .expect("list reached the worker");
        fx.reply(
            inbound,
            &list_answer(vec![approval_task("t-9", EXPIRES_AT)]),
        );
    };
    let (result, ()) = {
        let mut io = streams.io(false, false);
        tokio::join!(
            approvals::decide(&session, &args, true, &mut io, NOW_MS),
            worker
        )
    };
    let error = result.expect_err("confirmation needed");
    assert_eq!(error.exit(), Exit::Usage);
    assert!(
        streams.err_text().contains("remove build directory"),
        "card shown on stderr before the decision"
    );
    assert!(streams.out.is_empty(), "no decision output");
    let nothing = tokio::time::timeout(Duration::from_millis(100), sends.next()).await;
    assert!(nothing.is_err(), "no decision reached the worker");
}

// [0012-057] With `--yes` the decision is sent as a SendMessage naming the offered option, and
// the resulting task is reported. Signing itself is checked by req_0012_058.
#[tokio::test]
async fn req_0012_057_yes_sends_the_decision_once() {
    let fx = Fixture::new();
    let session = fx.session(vec![fx.worker_id.clone()], true);
    let mut lists = fx.serve(Op::List).await;
    let mut sends = fx.serve(Op::Send).await;
    let mut streams = Streams::new("");
    let args = decide_args(Some("worker-a"), true, None);
    let worker = async {
        let inbound = tokio::time::timeout(WORKER_WAIT, fx.next_inbound(&mut lists))
            .await
            .expect("list reached the worker");
        fx.reply(
            inbound,
            &list_answer(vec![approval_task("t-9", EXPIRES_AT)]),
        );
        let inbound = tokio::time::timeout(WORKER_WAIT, fx.next_inbound(&mut sends))
            .await
            .expect("decision reached the worker");
        let decision = decision_of(&inbound.body);
        fx.reply(inbound, &send_answer(task("t-9", pb::TaskState::Working)));
        decision
    };
    let (result, decision) = {
        let mut io = streams.io(false, false);
        tokio::join!(
            approvals::decide(&session, &args, true, &mut io, NOW_MS),
            worker
        )
    };
    assert_eq!(result.expect("decided"), Exit::Success);
    assert_eq!(decision["optionId"], ALLOW_ONCE_ID);
    assert_eq!(decision["taskId"], "t-9");
    assert!(streams.out_text().contains("decided req-1"));
    assert!(streams.out_text().contains("TASK_STATE_WORKING"));
}

// [0012-058] Decisions are never sent without an operator signing hook.
#[tokio::test]
async fn req_0012_058_unsigned_session_never_decides() {
    let fx = Fixture::new();
    let session = fx.session(vec![fx.worker_id.clone()], false);
    let mut lists = fx.serve(Op::List).await;
    let mut sends = fx.serve(Op::Send).await;
    let mut streams = Streams::new("");
    let args = decide_args(Some("worker-a"), true, None);
    let mut io = streams.io(false, false);
    let error = approvals::decide(&session, &args, true, &mut io, NOW_MS)
        .await
        .expect_err("unsigned decision refused");
    assert_eq!(error.exit(), Exit::Failure);
    assert!(streams.err.is_empty());
    let nothing_listed = tokio::time::timeout(Duration::from_millis(100), lists.next()).await;
    assert!(
        nothing_listed.is_err(),
        "no approval lookup without a signing hook"
    );
    let nothing_sent = tokio::time::timeout(Duration::from_millis(100), sends.next()).await;
    assert!(nothing_sent.is_err(), "no decision without a signing hook");
}

// [0012-057] `--note` on a denial is refused: ApprovalDecision carries no note field.
#[tokio::test]
async fn req_0012_057_deny_note_is_refused_before_bus_traffic() {
    let fx = Fixture::new();
    let session = fx.session(vec![fx.worker_id.clone()], true);
    let mut streams = Streams::new("");
    let args = decide_args(Some("worker-a"), true, Some("reason"));
    let mut io = streams.io(false, false);
    let error = approvals::decide(&session, &args, false, &mut io, NOW_MS)
        .await
        .expect_err("note refused");
    assert_eq!(error.exit(), Exit::Usage);
}

// [0012-057] An expired approval request is refused before the card is shown.
#[tokio::test]
async fn req_0012_057_expired_request_is_rejected_without_card() {
    let fx = Fixture::new();
    let session = fx.session(vec![fx.worker_id.clone()], true);
    let mut lists = fx.serve(Op::List).await;
    let mut streams = Streams::new("");
    let args = decide_args(Some("worker-a"), true, None);
    let worker = async {
        let inbound = tokio::time::timeout(WORKER_WAIT, fx.next_inbound(&mut lists))
            .await
            .expect("list reached the worker");
        fx.reply(
            inbound,
            &list_answer(vec![approval_task("t-old", EXPIRED_AT)]),
        );
    };
    let (result, ()) = {
        let mut io = streams.io(false, false);
        tokio::join!(
            approvals::decide(&session, &args, true, &mut io, NOW_MS),
            worker
        )
    };
    let error = result.expect_err("expired");
    assert_eq!(error.exit(), Exit::Rejected);
    assert!(streams.err.is_empty(), "no card for an expired request");
}

// [0012-058] No provider API key is named anywhere in the crate sources.
#[test]
fn req_0012_058_no_provider_api_keys_in_sources() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut scanned = 0_usize;
    for entry in std::fs::read_dir(&dir).expect("src dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path)
                .expect("source")
                .to_ascii_uppercase();
            for needle in ["API_KEY", "APIKEY", "ANTHROPIC", "OPENAI"] {
                assert!(
                    !text.contains(needle),
                    "{} names a provider key ({needle})",
                    path.display()
                );
            }
            scanned += 1;
        }
    }
    assert!(scanned > 0, "no sources scanned");
}
