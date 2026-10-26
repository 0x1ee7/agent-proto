//! Stdio MCP server loop (feature `runtime`).
//!
//! Stdin is read by one task with a bounded line reader. The main loop
//! answers protocol methods directly and starts one task per `tools/call`.
//! A single writer task owns stdout, so only MCP frames reach stdout
//! (RFC-0015 [0015-013]). On stdin EOF pending calls are cancelled and the loop
//! returns within one second (RFC-0015 [0015-067]).

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::client::{CallOutcome, IpcClient};
use crate::ipc::{
    CancelParams, CancelResult, ClientFrame, DiscoverResult, GetParams, IpcError, MAX_DEADLINE_MS,
    Response, TaskResult,
};
use crate::mcp::{
    self, INVALID_PARAMS, INVALID_REQUEST, Inbound, MAX_LINE_BYTES, METHOD_NOT_FOUND, PARSE_ERROR,
};
use crate::redact::Redactor;
use crate::tools::{self, ToolCall, ToolOutput};

/// Deadline for `discover`, `get`, and `cancel` requests (RFC-0015 [0015-062]).
const SIMPLE_DEADLINE_MS: u64 = 10_000;
/// Grace added to a `send` deadline on top of `timeoutMs`.
const SEND_GRACE_MS: u64 = 5_000;
/// Time allowed for in-flight calls to observe cancellation at EOF.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const OUTPUT_CAPACITY: usize = 64;
const IDEMPOTENCY_ID_CHARS: usize = 64;

/// Per-process options for the server.
pub struct Options {
    /// Cap on `content[0].text` bytes (RFC-0015 [0015-032]).
    pub max_text_bytes: usize,
    pub redactor: Arc<dyn Redactor>,
    /// Session slot as given to the IPC client; used in idempotency keys.
    pub session_slot: String,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("max_text_bytes", &self.max_text_bytes)
            .field("redactor", &"dyn Redactor")
            .finish_non_exhaustive()
    }
}

enum Input {
    Line(Vec<u8>),
    TooLong,
    Eof,
}

/// Runs the MCP stdio loop until `input` ends.
///
/// Returns `Ok(())` on EOF after pending calls are cancelled. Errors only when
/// stdout cannot be written.
pub async fn run<R, W>(input: R, output: W, client: IpcClient, options: Options) -> io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, out_rx) = mpsc::channel::<Value>(OUTPUT_CAPACITY);
    let writer = tokio::spawn(write_output(output, out_rx));

    let (lines_tx, mut lines) = mpsc::channel::<io::Result<Input>>(4);
    tokio::spawn(read_lines(input, lines_tx));

    let (done_tx, mut done) = mpsc::unbounded_channel::<String>();
    let mut session = Session {
        client: Arc::new(client),
        options: Arc::new(options),
        out,
        done: done_tx,
        calls: HashMap::new(),
    };

    loop {
        tokio::select! {
            input = lines.recv() => {
                match input {
                    None | Some(Err(_)) | Some(Ok(Input::Eof)) => break,
                    Some(Ok(Input::TooLong)) => {
                        session.emit(mcp::error_message(
                            &Value::Null,
                            INVALID_REQUEST,
                            "line exceeds 1 MiB limit",
                        )).await?;
                    }
                    Some(Ok(Input::Line(bytes))) => {
                        if bytes.iter().all(u8::is_ascii_whitespace) {
                            continue;
                        }
                        session.handle_line(&bytes).await?;
                    }
                }
            }
            Some(key) = done.recv() => {
                session.calls.remove(&key);
            }
        }
    }

    for (_, cancel) in session.calls.drain() {
        let _ = cancel.send(());
    }
    let grace_end = Instant::now() + SHUTDOWN_GRACE;
    while !session.calls.is_empty() {
        tokio::select! {
            Some(key) = done.recv() => {
                session.calls.remove(&key);
            }
            () = tokio::time::sleep_until(grace_end) => break,
        }
    }
    let Session { out, .. } = session;
    drop(out);
    let _ = tokio::time::timeout_at(grace_end, writer).await;
    Ok(())
}

struct Session {
    client: Arc<IpcClient>,
    options: Arc<Options>,
    out: mpsc::Sender<Value>,
    done: mpsc::UnboundedSender<String>,
    calls: HashMap<String, oneshot::Sender<()>>,
}

#[derive(Clone, Copy)]
enum Kind {
    Discover,
    Task,
    Cancel,
}

impl Session {
    async fn emit(&self, message: Value) -> io::Result<()> {
        self.out
            .send(message)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "stdout writer stopped"))
    }

    async fn handle_line(&mut self, bytes: &[u8]) -> io::Result<()> {
        match mcp::parse_line(bytes) {
            Inbound::Request { id, method, params } => self.dispatch(id, &method, params).await,
            Inbound::Notification { method, params } => {
                if method == "notifications/cancelled" {
                    if let Some(request_id) = params.get("requestId") {
                        if let Some(cancel) = self.calls.remove(&request_id.to_string()) {
                            let _ = cancel.send(());
                        }
                    }
                }
                Ok(())
            }
            Inbound::Response => Ok(()),
            Inbound::Invalid { id } => {
                self.emit(mcp::error_message(&id, INVALID_REQUEST, "invalid request"))
                    .await
            }
            Inbound::Unparseable => {
                self.emit(mcp::error_message(&Value::Null, PARSE_ERROR, "parse error"))
                    .await
            }
        }
    }

    async fn dispatch(&mut self, id: Value, method: &str, params: Value) -> io::Result<()> {
        match method {
            "initialize" => {
                self.emit(mcp::result_message(&id, mcp::initialize_result()))
                    .await
            }
            "ping" => {
                self.emit(mcp::result_message(&id, serde_json::json!({})))
                    .await
            }
            "tools/list" => {
                self.emit(mcp::result_message(&id, mcp::tools_list_result()))
                    .await
            }
            "tools/call" => self.start_call(id, params).await,
            _ => {
                self.emit(mcp::error_message(
                    &id,
                    METHOD_NOT_FOUND,
                    "method not found",
                ))
                .await
            }
        }
    }

    async fn start_call(&mut self, id: Value, params: Value) -> io::Result<()> {
        let Some((name, arguments)) = mcp::tools_call_params(&params) else {
            return self
                .emit(mcp::error_message(&id, INVALID_PARAMS, "invalid params"))
                .await;
        };
        let call = match ToolCall::parse(&name, arguments) {
            Ok(call) => call,
            Err(None) => {
                return self
                    .emit(mcp::error_message(&id, INVALID_PARAMS, "unknown tool"))
                    .await;
            }
            Err(Some(error)) => {
                let output = tools::error_output(&error);
                return self
                    .emit(mcp::result_message(&id, mcp::tool_call_result(&output)))
                    .await;
            }
        };
        let (kind, frame, budget_ms) = build_frame(&call, &id, &self.options.session_slot);
        let key = id.to_string();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.calls.insert(key.clone(), cancel_tx);

        let client = Arc::clone(&self.client);
        let options = Arc::clone(&self.options);
        let out = self.out.clone();
        let done = self.done.clone();
        let deadline = Instant::now() + Duration::from_millis(budget_ms);
        tokio::spawn(async move {
            let outcome = client.call(frame, deadline, cancel_rx).await;
            if let CallOutcome::Done(result) = outcome {
                let output = shape(kind, result, &options);
                let _ = out
                    .send(mcp::result_message(&id, mcp::tool_call_result(&output)))
                    .await;
            }
            let _ = done.send(key);
        });
        Ok(())
    }
}

/// Builds the IPC frame, its kind, and the local budget in milliseconds.
fn build_frame(call: &ToolCall, id: &Value, slot: &str) -> (Kind, ClientFrame, u64) {
    match call {
        ToolCall::Discover(params) => (
            Kind::Discover,
            ClientFrame::Discover {
                id: 0,
                deadline_ms: SIMPLE_DEADLINE_MS,
                params: params.clone(),
            },
            SIMPLE_DEADLINE_MS,
        ),
        ToolCall::Send(args) => {
            let budget = args
                .timeout_ms
                .unwrap_or(MAX_DEADLINE_MS)
                .saturating_add(SEND_GRACE_MS)
                .min(MAX_DEADLINE_MS);
            let params = args.clone().into_params(idempotency_key(id, slot));
            (
                Kind::Task,
                ClientFrame::Send {
                    id: 0,
                    deadline_ms: budget,
                    params,
                },
                budget,
            )
        }
        ToolCall::Get(params) => get_frame(params.clone()),
        ToolCall::Cancel(params) => cancel_frame(params.clone()),
    }
}

fn get_frame(params: GetParams) -> (Kind, ClientFrame, u64) {
    (
        Kind::Task,
        ClientFrame::Get {
            id: 0,
            deadline_ms: SIMPLE_DEADLINE_MS,
            params,
        },
        SIMPLE_DEADLINE_MS,
    )
}

fn cancel_frame(params: CancelParams) -> (Kind, ClientFrame, u64) {
    (
        Kind::Cancel,
        ClientFrame::Cancel {
            id: 0,
            deadline_ms: SIMPLE_DEADLINE_MS,
            params,
        },
        SIMPLE_DEADLINE_MS,
    )
}

/// Maps the MCP request id and session slot to the IPC idempotency key
/// (RFC-0015 [0015-047]).
fn idempotency_key(id: &Value, slot: &str) -> String {
    let raw = match id {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let sanitized: String = raw
        .chars()
        .map(|c| if c.is_ascii_graphic() { c } else { '_' })
        .take(IDEMPOTENCY_ID_CHARS)
        .collect();
    format!("{sanitized}:{slot}")
}

fn shape(kind: Kind, result: Result<Response, IpcError>, options: &Options) -> ToolOutput {
    let response = match result {
        Ok(response) => response,
        Err(error) => return tools::error_output(&error),
    };
    let redactor: &dyn Redactor = options.redactor.as_ref();
    match kind {
        Kind::Discover => response
            .into_result::<DiscoverResult>()
            .map(|r| tools::discover_output(r, redactor)),
        Kind::Task => response
            .into_result::<TaskResult>()
            .map(|r| tools::task_output(r, redactor, options.max_text_bytes)),
        Kind::Cancel => response
            .into_result::<CancelResult>()
            .map(tools::cancel_output),
    }
    .unwrap_or_else(|error| tools::error_output(&error))
}

async fn write_output<W>(mut output: W, mut rx: mpsc::Receiver<Value>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(message) = rx.recv().await {
        output.write_all(&mcp::to_line(&message)).await?;
        output.flush().await?;
    }
    Ok(())
}

async fn read_lines<R>(input: R, tx: mpsc::Sender<io::Result<Input>>)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(input);
    loop {
        let item = read_bounded_line(&mut reader, MAX_LINE_BYTES).await;
        let stop = !matches!(item, Ok(Input::Line(_) | Input::TooLong));
        if tx.send(item).await.is_err() || stop {
            break;
        }
    }
}

/// Reads one line of at most `max` bytes (excluding the newline). Longer lines
/// are consumed and reported as [`Input::TooLong`].
async fn read_bounded_line<R>(reader: &mut R, max: usize) -> io::Result<Input>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    let mut overflow = false;
    loop {
        let (consumed, done) = {
            let buf = reader.fill_buf().await?;
            if buf.is_empty() {
                return Ok(if overflow {
                    Input::TooLong
                } else if line.is_empty() {
                    Input::Eof
                } else {
                    Input::Line(line)
                });
            }
            let (chunk, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(pos) => (&buf[..pos], true),
                None => (buf, false),
            };
            if !overflow {
                if line.len() + chunk.len() > max {
                    overflow = true;
                    line.clear();
                } else {
                    line.extend_from_slice(chunk);
                }
            }
            (chunk.len() + usize::from(done), done)
        };
        reader.consume(consumed);
        if done {
            return Ok(if overflow {
                Input::TooLong
            } else {
                Input::Line(line)
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // RFC-0015 [0015-047]: idempotency key combines the request id and slot.
    #[test]
    fn idempotency_key_shape_0015_047() {
        assert_eq!(idempotency_key(&Value::from(7), "slot"), "7:slot");
        assert_eq!(idempotency_key(&Value::from("a b"), "slot"), "a_b:slot");
    }

    // RFC-0015 [0015-013]: bounded reader refuses oversized lines and
    // resynchronises on the next line.
    #[tokio::test]
    async fn oversized_line_is_discarded_0015_013() {
        let data = b"0123456789\nok\n".to_vec();
        let mut reader = BufReader::new(&data[..]);
        assert!(matches!(
            read_bounded_line(&mut reader, 4)
                .await
                .unwrap_or(Input::Eof),
            Input::TooLong
        ));
        match read_bounded_line(&mut reader, 4).await {
            Ok(Input::Line(bytes)) => assert_eq!(bytes, b"ok"),
            _ => panic!("expected line"),
        }
        assert!(matches!(
            read_bounded_line(&mut reader, 4).await,
            Ok(Input::Eof)
        ));
    }
}
