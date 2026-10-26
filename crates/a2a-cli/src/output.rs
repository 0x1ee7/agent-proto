//! Terminal output: ANSI color gating ([0012-059]) and proto-JSON printing ([0012-056]).

use std::io::{BufRead, Write};

use serde::Serialize;

use crate::error::CliError;

/// Standard streams and output mode for one command run.
pub struct Io<'a> {
    /// Standard output: command results.
    pub out: &'a mut dyn Write,
    /// Diagnostics: approval cards and confirmation prompts.
    pub err: &'a mut dyn Write,
    /// Standard input: `-` message bodies and confirmations.
    pub input: &'a mut dyn BufRead,
    /// True when standard input is a terminal.
    pub interactive: bool,
    /// Emit proto-JSON instead of text.
    pub json: bool,
    /// Whether ANSI color is enabled for this run.
    pub color: bool,
}

impl std::fmt::Debug for Io<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Io")
            .field("interactive", &self.interactive)
            .field("json", &self.json)
            .field("color", &self.color)
            .finish_non_exhaustive()
    }
}

/// Reads an environment variable; `None` when unset or not valid Unicode.
pub type EnvFn<'a> = &'a dyn Fn(&str) -> Option<String>;

/// True when the console is judged to understand VT escape sequences.
///
/// Windows consoles are detected by the environment markers of terminals that enable VT
/// (Windows Terminal, ConEmu with ANSI, or a `TERM` set by a shell); this crate forbids `unsafe`,
/// so it cannot query `ENABLE_VIRTUAL_TERMINAL_PROCESSING` directly. Other platforms require a
/// `TERM` other than `dumb`.
#[must_use]
pub fn vt_supported(env: EnvFn<'_>, windows: bool) -> bool {
    let term_ok = env("TERM").is_some_and(|term| !term.is_empty() && term != "dumb");
    if !windows {
        return term_ok;
    }
    term_ok
        || env("WT_SESSION").is_some()
        || env("TERM_PROGRAM").is_some()
        || env("ConEmuANSI").is_some_and(|value| value.eq_ignore_ascii_case("on"))
}

/// Whether ANSI color is used: never with `--no-color`, `NO_COLOR`, a non-terminal stdout, or a
/// console without VT support ([0012-059]).
#[must_use]
pub fn color_enabled(
    no_color_flag: bool,
    env: EnvFn<'_>,
    is_terminal: bool,
    windows: bool,
) -> bool {
    !no_color_flag && env("NO_COLOR").is_none() && is_terminal && vt_supported(env, windows)
}

/// Wraps `text` in an SGR sequence when `enabled`; otherwise returns it unchanged.
#[must_use]
pub fn paint(text: &str, sgr: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{sgr}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Writes `value` as pretty proto-JSON. Callers pass only existing A2A and A2A-Bus messages.
///
/// # Errors
/// Returns [`CliError::Failed`] when serialization or the write fails.
pub fn write_json<T: Serialize + ?Sized>(out: &mut dyn Write, value: &T) -> Result<(), CliError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| CliError::Failed(format!("cannot encode output: {error}")))?;
    writeln!(out, "{text}")
        .map_err(|error| CliError::Failed(format!("cannot write output: {error}")))
}

/// Writes one human-readable line.
///
/// # Errors
/// Returns [`CliError::Failed`] when the write fails.
pub fn line(out: &mut dyn Write, text: &str) -> Result<(), CliError> {
    writeln!(out, "{text}")
        .map_err(|error| CliError::Failed(format!("cannot write output: {error}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    // req_0012_059_no_color_removes_ansi
    #[test]
    fn req_0012_059_no_color_removes_ansi() {
        let env = env_of(&[("TERM", "xterm-256color")]);
        assert!(!color_enabled(true, &env, true, false));
        assert_eq!(paint("ok", "32", false), "ok");
        assert!(color_enabled(false, &env, true, false));
        assert_eq!(paint("ok", "32", true), "\x1b[32mok\x1b[0m");
    }

    // req_0012_059_no_color_env_and_non_terminal
    #[test]
    fn req_0012_059_no_color_env_and_non_terminal() {
        let no_color = env_of(&[("TERM", "xterm"), ("NO_COLOR", "1")]);
        assert!(!color_enabled(false, &no_color, true, false));
        let term = env_of(&[("TERM", "xterm")]);
        assert!(!color_enabled(false, &term, false, false));
    }

    // req_0012_059_vt_detection_linux
    #[test]
    fn req_0012_059_vt_detection_linux() {
        assert!(vt_supported(&env_of(&[("TERM", "xterm")]), false));
        assert!(!vt_supported(&env_of(&[("TERM", "dumb")]), false));
        assert!(!vt_supported(&env_of(&[]), false));
        assert!(!color_enabled(false, &env_of(&[]), true, false));
    }

    // req_0012_059_vt_detection_windows
    #[test]
    fn req_0012_059_vt_detection_windows() {
        assert!(vt_supported(&env_of(&[("WT_SESSION", "abc")]), true));
        assert!(vt_supported(&env_of(&[("ConEmuANSI", "ON")]), true));
        assert!(!vt_supported(&env_of(&[("ConEmuANSI", "OFF")]), true));
        assert!(!vt_supported(&env_of(&[]), true));
        assert!(color_enabled(
            false,
            &env_of(&[("WT_SESSION", "1")]),
            true,
            true
        ));
        assert!(!color_enabled(
            true,
            &env_of(&[("WT_SESSION", "1")]),
            true,
            true
        ));
    }
}
