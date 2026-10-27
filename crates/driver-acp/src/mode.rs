//! Approval mode safety (RFC-0009 section 4.9, 0009-063..067).
//!
//! The allowed set is operator-configured. A dangerous mode runs only when the operator also
//! sets `acknowledge_dangerous`. Mode names are canonicalized before any check, so alias
//! spellings cannot bypass the dangerous set.

use crate::config::Profile;

/// Canonical name and dangerous flag for a mode name under `profile`, or `None` when the name is
/// unknown to that profile.
pub(crate) fn canonical(profile: Profile, name: &str) -> Option<(&'static str, bool)> {
    match (profile, name) {
        (Profile::Omp, "ask-exec") => Some(("always-ask-cached", false)),
        (Profile::Omp, "write") => Some(("write", true)),
        (Profile::Omp, "yolo") => Some(("yolo", true)),
        (Profile::Claude, "default" | "ask-all") => Some(("default", false)),
        (Profile::Claude, "plan") => Some(("plan", false)),
        (Profile::Claude, "bypass" | "bypassPermissions") => Some(("bypassPermissions", true)),
        (Profile::Claude, "acceptEdits") => Some(("acceptEdits", true)),
        (Profile::Claude, "dontAsk") => Some(("dontAsk", true)),
        (Profile::Claude, "auto") => Some(("auto", true)),
        _ => None,
    }
}

/// Canonical names of the modes that run without acknowledgement, for capability reporting.
pub(crate) fn safe_modes(profile: Profile) -> &'static [&'static str] {
    match profile {
        Profile::Omp => &["always-ask-cached"],
        Profile::Claude => &["default", "plan"],
    }
}

/// Checks the configured mode before a turn starts (0009-063, 0009-064). Returns the refusal
/// reason, which is recorded in the fault detail.
pub(crate) fn check_start(
    profile: Profile,
    configured: &str,
    acknowledged: bool,
) -> Result<(), &'static str> {
    match canonical(profile, configured) {
        None => Err("approval mode is not in the allowed set for this profile"),
        Some((_, true)) if !acknowledged => {
            Err("dangerous approval mode requires acknowledge_dangerous")
        }
        Some(_) => Ok(()),
    }
}

/// True when the agent switched to `mode_id` and that switch must be refused: the target is a
/// dangerous mode and it is not the configured, acknowledged dangerous mode (0009-065, 0009-067).
pub(crate) fn switch_refused(
    profile: Profile,
    configured: &str,
    acknowledged: bool,
    mode_id: &str,
) -> bool {
    match canonical(profile, mode_id) {
        Some((target, true)) => {
            let configured_canonical = canonical(profile, configured).map(|(name, _)| name);
            !(acknowledged && configured_canonical == Some(target))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_modes_start_without_acknowledgement_0009_063() {
        assert!(check_start(Profile::Omp, "ask-exec", false).is_ok());
        assert!(check_start(Profile::Claude, "default", false).is_ok());
        assert!(check_start(Profile::Claude, "plan", false).is_ok());
    }

    #[test]
    fn omp_always_ask_and_unknown_names_are_refused_0009_063() {
        assert!(check_start(Profile::Omp, "always-ask", true).is_err());
        assert!(check_start(Profile::Omp, "default", true).is_err());
    }

    #[test]
    fn dangerous_modes_need_acknowledgement_0009_064() {
        assert!(check_start(Profile::Claude, "bypassPermissions", false).is_err());
        assert!(check_start(Profile::Claude, "bypass", false).is_err());
        assert!(check_start(Profile::Claude, "bypass", true).is_ok());
        assert!(check_start(Profile::Omp, "yolo", false).is_err());
        assert!(check_start(Profile::Omp, "write", true).is_ok());
    }

    #[test]
    fn mid_session_switch_to_dangerous_mode_is_refused_0009_065() {
        assert!(switch_refused(
            Profile::Claude,
            "default",
            false,
            "bypassPermissions"
        ));
        assert!(switch_refused(
            Profile::Claude,
            "default",
            true,
            "bypassPermissions"
        ));
        assert!(!switch_refused(
            Profile::Claude,
            "bypass",
            true,
            "bypassPermissions"
        ));
        assert!(!switch_refused(Profile::Claude, "default", false, "plan"));
    }
}
