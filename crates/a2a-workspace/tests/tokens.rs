#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Token parity between `ErrorKind` and the wire vocabulary of `a2a-types` (RFC-0012 sec 4.8).

use std::collections::BTreeSet;

use a2a_types::failure::WorkspaceToken;
use a2a_workspace::ErrorKind;

// [0012-067]
#[test]
fn req_0012_067_error_tokens_match_a2a_types_table() {
    let ours: BTreeSet<&str> = ErrorKind::ALL.iter().map(|kind| kind.token()).collect();
    let wire: BTreeSet<&str> = WorkspaceToken::ALL
        .iter()
        .map(|token| token.as_str())
        .collect();
    assert_eq!(ours.len(), 14, "every kind has a distinct token");
    assert_eq!(ours, wire);
}
