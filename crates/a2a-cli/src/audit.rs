//! `a2a audit verify` ([0013-028]): checkpoint keys come from the trust store.

use std::io;
use std::path::PathBuf;

use a2a_audit::chain::CheckpointVerifier;
use a2a_audit::verify::{EXIT_VERIFIED, VerifyReport, verify_paths};
use a2a_security::identity::Verifier;
use a2a_security::trust::TrustStore;

use crate::error::{CliError, Exit};

/// Checkpoint verifier backed by the trust store: a checkpoint verifies when its key id is in
/// the store and the signature verifies under that key.
#[derive(Debug)]
pub struct TrustCheckpoints<'a>(pub &'a TrustStore);

impl CheckpointVerifier for TrustCheckpoints<'_> {
    fn verify(&self, key_id: &str, message: &[u8], signature: &[u8]) -> bool {
        self.0
            .entry(key_id)
            .is_some_and(|entry| entry.verify_bytes(message, signature))
    }
}

/// Verifies audit files in the given order.
///
/// # Errors
/// Returns [`CliError::NotFound`] for a missing file and [`CliError::Failed`] for other I/O errors.
pub fn verify_files(paths: &[PathBuf], trust: &TrustStore) -> Result<VerifyReport, CliError> {
    verify_paths(paths, &TrustCheckpoints(trust)).map_err(map_io)
}

fn map_io(error: io::Error) -> CliError {
    if error.kind() == io::ErrorKind::NotFound {
        CliError::NotFound(error.to_string())
    } else {
        CliError::Failed(format!("cannot read audit file: {error}"))
    }
}

/// Exit class of a report: success when no break was found, otherwise generic failure.
#[must_use]
pub fn exit_of(report: &VerifyReport) -> Exit {
    if report.exit_code() == EXIT_VERIFIED {
        Exit::Success
    } else {
        Exit::Failure
    }
}

/// Human-readable report naming the first broken line.
#[must_use]
pub fn render(report: &VerifyReport) -> String {
    match &report.first_break {
        None => format!(
            "ok: {} lines, {} checkpoints verified",
            report.lines, report.checkpoints
        ),
        Some(found) => format!(
            "broken: {}:{}: {:?} (after {} lines, {} checkpoints)",
            found.file.display(),
            found.line,
            found.kind,
            report.lines,
            report.checkpoints
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use a2a_audit::verify::{Break, BreakKind};

    // req_0013_028_report_names_first_broken_line
    #[test]
    fn req_0013_028_report_names_first_broken_line() {
        let report = VerifyReport {
            lines: 9,
            checkpoints: 1,
            first_break: Some(Break {
                file: PathBuf::from("audit-1.jsonl"),
                line: 4,
                kind: BreakKind::ChainHash,
            }),
        };
        let text = render(&report);
        assert!(text.starts_with("broken: audit-1.jsonl:4:"));
        assert_eq!(exit_of(&report), Exit::Failure);
        assert_eq!(report.exit_code(), 1);
    }

    // req_0013_028_clean_chain_exits_zero
    #[test]
    fn req_0013_028_clean_chain_exits_zero() {
        let report = VerifyReport {
            lines: 3,
            checkpoints: 0,
            first_break: None,
        };
        assert_eq!(exit_of(&report), Exit::Success);
        assert_eq!(render(&report), "ok: 3 lines, 0 checkpoints verified");
    }

    // req_0013_028_missing_file_is_not_found
    #[test]
    fn req_0013_028_missing_file_is_not_found() {
        let store = TrustStore::default();
        let error = verify_files(&[PathBuf::from("definitely/missing-audit.jsonl")], &store)
            .expect_err("missing file");
        assert!(matches!(error, CliError::NotFound(_)));
        assert_eq!(error.exit(), Exit::NotFound);
    }

    // req_0013_028_checkpoint_unknown_key_fails
    #[test]
    fn req_0013_028_checkpoint_unknown_key_fails() {
        let store = TrustStore::default();
        assert!(!TrustCheckpoints(&store).verify("nope", b"msg", b"sig"));
    }
}
