//! `a2abus-mcp-shim`: stdio MCP server that forwards tool calls to the worker
//! over the delegation IPC endpoint (RFC-0015 [0015-080..090]).
//!
//! Arguments: `--grant <b64url>`, `--endpoint <path>`, `--slot <b64url>`. The
//! grant credential is read only from the environment variable
//! [`a2a_mcp::GRANT_ENV`]; it is never accepted on the command line.

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;

use a2a_mcp::GRANT_ENV;
use a2a_mcp::client::{self, ClientConfig};
use a2a_mcp::ipc::{self, GRANT_CREDENTIAL_BYTES, GRANT_ID_BYTES, MAX_RESULT_TEXT_BYTES, Secret};
use a2a_mcp::redact::NoRedaction;
use a2a_mcp::server::{self, Options};

struct Args {
    grant: String,
    endpoint: String,
    slot: String,
}

fn parse_args<I>(mut args: I) -> Result<Args, String>
where
    I: Iterator<Item = String>,
{
    let mut grant = None;
    let mut endpoint = None;
    let mut slot = None;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--grant" => grant = Some(value),
            "--endpoint" => endpoint = Some(value),
            "--slot" => slot = Some(value),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let grant = grant.ok_or_else(|| "missing --grant".to_owned())?;
    let endpoint = endpoint.ok_or_else(|| "missing --endpoint".to_owned())?;
    let slot = slot.ok_or_else(|| "missing --slot".to_owned())?;
    if ipc::decode_b64url(&grant, GRANT_ID_BYTES).is_none() {
        return Err("--grant must be 16 bytes, base64url".to_owned());
    }
    if ipc::decode_b64url(&slot, ipc::SESSION_SLOT_BYTES).is_none() {
        return Err("--slot must be 16 bytes, base64url".to_owned());
    }
    if endpoint.is_empty() {
        return Err("--endpoint must not be empty".to_owned());
    }
    Ok(Args {
        grant,
        endpoint,
        slot,
    })
}

/// Reads the grant credential from the environment. An absent or malformed
/// value yields `None`; the client then reports UNAUTHENTICATED to each call.
fn credential_from_env() -> Option<Secret> {
    std::env::var(GRANT_ENV)
        .ok()
        .filter(|value| ipc::decode_b64url(value, GRANT_CREDENTIAL_BYTES).is_some())
        .map(Secret::new)
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("a2abus-mcp-shim: {message}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("a2abus-mcp-shim: cannot start runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let credential = credential_from_env();
    let session_slot = args.slot.clone();
    let result = runtime.block_on(async move {
        let client = client::spawn(ClientConfig {
            endpoint: args.endpoint,
            grant_id: args.grant,
            credential,
            session_slot: args.slot,
        });
        server::run(
            tokio::io::stdin(),
            tokio::io::stdout(),
            client,
            Options {
                max_text_bytes: MAX_RESULT_TEXT_BYTES,
                redactor: Arc::new(NoRedaction),
                session_slot,
            },
        )
        .await
    });
    // A blocked stdin read must not keep the process alive after stdout fails.
    runtime.shutdown_background();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("a2abus-mcp-shim: {err}");
            ExitCode::FAILURE
        }
    }
}
