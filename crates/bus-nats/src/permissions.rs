//! Pure generator of the per-identity NATS permission sets (RFC-0010 section 4.8,
//! 0010-057..064) and of the `nats-server` configuration that carries them.

use std::fmt::Write as _;
use std::path::Path;

use a2a_types::ids::{AgentId, TenantId};

use crate::layout::{
    ARCHIVER_CONSUMER, CARDS, CLAIMS, PRESENCE, RESPONSE_EXPIRES, artifact_bucket, audit_stream,
    ev_stream, kv_bucket, kv_stream, object_stream,
};

/// The four roles of RFC-0010 section 4.8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// An agent worker (omp, claude, hermes, ...).
    Agent,
    /// The gateway.
    Gateway,
    /// An operator client (tui, cli).
    Operator,
    /// The supervisor (provisioning and reaper).
    Supervisor,
}

/// Errors of [`permissions`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PermissionError {
    /// The identity is not valid for the requested role.
    #[error("identity `{identity}` is not valid for role {role:?}")]
    Identity {
        /// The rejected identity.
        identity: String,
        /// The requested role.
        role: Role,
    },
}

/// A NATS user permission set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permissions {
    /// Subjects the user may publish to.
    pub publish: Vec<String>,
    /// Subjects the user may subscribe to.
    pub subscribe: Vec<String>,
    /// Whether the user may answer requests (`allow_responses`, 0010-061).
    pub responses: bool,
}

/// Permission set of `identity` in `role` on `tenant`.
///
/// Agents take any identity that is a valid [`AgentId`] and not reserved.
/// Gateway, operator and supervisor identities are fixed names.
pub fn permissions(
    role: Role,
    identity: &str,
    tenant: &TenantId,
) -> Result<Permissions, PermissionError> {
    let reject = || PermissionError::Identity {
        identity: identity.to_owned(),
        role,
    };
    let t = tenant.as_str();
    let base = format!("a2a.v1.{t}");
    let rpc = format!("{base}.rpc");
    let cards = kv_bucket(tenant, CARDS);
    let presence = kv_bucket(tenant, PRESENCE);
    let claims = kv_bucket(tenant, CLAIMS);
    let artifacts = artifact_bucket(tenant);
    let ev = ev_stream(tenant);
    let audit = audit_stream(tenant);

    let mut perms = match role {
        Role::Agent => {
            let agent = AgentId::parse(identity).map_err(|_| reject())?;
            if agent.is_reserved() {
                return Err(reject());
            }
            let me = agent.as_str();
            let mut read = Vec::new();
            for stream in [
                ev.clone(),
                kv_stream(&cards),
                kv_stream(&presence),
                kv_stream(&claims),
                object_stream(&artifacts),
            ] {
                read.extend(read_bundle(&stream));
            }
            Permissions {
                publish: with(
                    vec![
                        format!("{rpc}.*.*"),
                        format!("{base}.ev.{me}.>"),
                        format!("{base}.audit.{me}"),
                        format!("$KV.{cards}.{me}"),
                        format!("$KV.{presence}.{me}"),
                        format!("$KV.{claims}.{me}.*"),
                        format!("$O.{artifacts}.C.>"),
                        format!("$O.{artifacts}.M.>"),
                    ],
                    read,
                ),
                subscribe: vec![format!("_INBOX.{me}.>"), format!("{rpc}.{me}.*")],
                responses: true,
            }
        }
        Role::Gateway => {
            if identity != AgentId::gateway().as_str() {
                return Err(reject());
            }
            let mut read = Vec::new();
            for stream in [
                ev.clone(),
                kv_stream(&cards),
                kv_stream(&presence),
                kv_stream(&claims),
                object_stream(&artifacts),
            ] {
                read.extend(read_bundle(&stream));
            }
            Permissions {
                publish: with(
                    vec![
                        format!("{rpc}.*.*"),
                        format!("$KV.{cards}.gateway"),
                        format!("$KV.{presence}.gateway"),
                        format!("{base}.audit.gateway"),
                    ],
                    read,
                ),
                subscribe: vec!["_INBOX.gateway.>".to_owned()],
                responses: false,
            }
        }
        Role::Operator => {
            if identity != AgentId::tui().as_str() && identity != AgentId::cli().as_str() {
                return Err(reject());
            }
            let mut read = Vec::new();
            for stream in [
                audit.clone(),
                kv_stream(&cards),
                kv_stream(&presence),
                kv_stream(&claims),
            ] {
                read.extend(read_bundle(&stream));
            }
            Permissions {
                publish: with(
                    vec![format!("{rpc}.*.*"), format!("{base}.audit.{identity}")],
                    read,
                ),
                subscribe: vec![format!("_INBOX.{identity}.>")],
                responses: false,
            }
        }
        Role::Supervisor => {
            if identity != AgentId::supervisor().as_str() {
                return Err(reject());
            }
            let mut admin = Vec::new();
            let streams = [
                ev.clone(),
                audit.clone(),
                kv_stream(&cards),
                kv_stream(&presence),
                kv_stream(&claims),
                object_stream(&artifacts),
            ];
            for stream in &streams {
                admin.push(format!("$JS.API.STREAM.CREATE.{stream}"));
                admin.push(format!("$JS.API.STREAM.UPDATE.{stream}"));
                admin.push(format!("$JS.API.STREAM.INFO.{stream}"));
            }
            let archiver = format!("{audit}.{ARCHIVER_CONSUMER}");
            admin.extend([
                // async-nats names the filter subject in the create subject:
                // `CONSUMER.CREATE.<stream>.<durable>.<filter>` (RFC-0010 0010-054).
                format!("$JS.API.CONSUMER.CREATE.{archiver}"),
                format!("$JS.API.CONSUMER.CREATE.{archiver}.>"),
                format!("$JS.API.CONSUMER.DURABLE.CREATE.{archiver}"),
                format!("$JS.API.CONSUMER.DURABLE.CREATE.{archiver}.>"),
                format!("$JS.API.CONSUMER.INFO.{archiver}"),
                format!("$JS.API.CONSUMER.DELETE.{archiver}"),
                format!("$JS.API.CONSUMER.MSG.NEXT.{archiver}"),
                "$JS.ACK.>".to_owned(),
            ]);
            Permissions {
                publish: with(vec![format!("{base}.ev.*.>")], admin),
                subscribe: vec![
                    "$JS.EVENT.ADVISORY.>".to_owned(),
                    "_INBOX.supervisor.>".to_owned(),
                ],
                responses: false,
            }
        }
    };
    perms.publish.sort();
    perms.subscribe.sort();
    Ok(perms)
}

/// The read bundle of one stream (0010-060): stream info, consumer management,
/// direct and message reads, and acknowledgements on that stream's consumers.
fn read_bundle(stream: &str) -> Vec<String> {
    vec![
        format!("$JS.API.STREAM.INFO.{stream}"),
        format!("$JS.API.CONSUMER.CREATE.{stream}"),
        format!("$JS.API.CONSUMER.CREATE.{stream}.>"),
        format!("$JS.API.CONSUMER.DURABLE.CREATE.{stream}.>"),
        format!("$JS.API.CONSUMER.INFO.{stream}.>"),
        format!("$JS.API.CONSUMER.DELETE.{stream}.>"),
        format!("$JS.API.DIRECT.GET.{stream}"),
        format!("$JS.API.DIRECT.GET.{stream}.>"),
        format!("$JS.API.STREAM.MSG.GET.{stream}"),
        format!("$JS.ACK.{stream}.>"),
    ]
}

/// Account info (`$JS.API.INFO`) is queried by every binding at connect for the M1
/// profile check (`connection::connect`), so every role may publish it.
/// Adds the read subjects to a publish list; read subjects are published to
/// the JetStream API and are therefore part of the publish allow-list.
fn with(mut publish: Vec<String>, read: Vec<String>) -> Vec<String> {
    publish.push("$JS.API.INFO".to_owned());
    publish.extend(read);
    publish
}

/// One user of the rendered server configuration.
#[derive(Debug, Clone)]
pub struct ServerUser {
    /// The public NKey of the user. Seeds never reach the configuration.
    pub public_key: String,
    /// The user's permissions.
    pub permissions: Permissions,
}

/// Inputs of [`render_server_conf`].
#[derive(Debug, Clone)]
pub struct ServerSpec<'a> {
    /// Listen address.
    pub host: &'a str,
    /// Listen port.
    pub port: u16,
    /// JetStream store directory.
    pub store_dir: &'a Path,
    /// NKey users and their permissions.
    pub users: &'a [ServerUser],
}

/// Renders a `nats-server` configuration file with NKey users and their
/// permissions (`max_payload` 1 MiB, 0010-015).
#[must_use]
pub fn render_server_conf(spec: &ServerSpec<'_>) -> String {
    let store = spec.store_dir.to_string_lossy().replace('\\', "/");
    let mut out = String::new();
    let _ = writeln!(out, "host: \"{}\"", spec.host);
    let _ = writeln!(out, "port: {}", spec.port);
    out.push_str("max_payload: 1048576\n");
    out.push_str("jetstream {\n");
    let _ = writeln!(out, "  store_dir: \"{store}\"");
    out.push_str("  max_memory_store: 268435456\n");
    out.push_str("  max_file_store: 4294967296\n");
    out.push_str("}\n");
    out.push_str("authorization {\n  users = [\n");
    for user in spec.users {
        let p = &user.permissions;
        let _ = write!(
            out,
            "    {{ nkey: \"{}\", permissions: {{ publish: {{ allow: {} }}, subscribe: {{ allow: {} }}",
            user.public_key,
            list(&p.publish),
            list(&p.subscribe),
        );
        if p.responses {
            let _ = write!(
                out,
                ", allow_responses: {{ max: 1, expires: \"{RESPONSE_EXPIRES}\" }}"
            );
        }
        out.push_str(" } },\n");
    }
    out.push_str("  ]\n}\n");
    out
}

fn list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    format!("[{}]", quoted.join(", "))
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn tenant() -> TenantId {
        TenantId::parse("local").expect("tenant")
    }

    fn has(list: &[String], subject: &str) -> bool {
        list.iter().any(|s| s == subject)
    }

    // 0010-060: agents publish only to their own event, audit and KV keys.
    #[test]
    fn req_0010_060_agent_publish_is_scoped_to_self() {
        let p = permissions(Role::Agent, "omp", &tenant()).expect("agent");
        assert!(has(&p.publish, "a2a.v1.local.ev.omp.>"));
        assert!(has(&p.publish, "a2a.v1.local.audit.omp"));
        assert!(has(&p.publish, "$KV.a2ab-local-cards.omp"));
        assert!(has(&p.publish, "$KV.a2ab-local-presence.omp"));
        assert!(has(&p.publish, "$KV.a2ab-local-claims.omp.*"));
        assert!(!p.publish.iter().any(|s| s.contains("ev.claude")));
        assert!(!p.publish.iter().any(|s| s == "$KV.a2ab-local-cards.claude"));
        assert!(p.responses);
    }

    // 0010-060: agents subscribe only to their own inbox and rpc subjects.
    #[test]
    fn req_0010_060_agent_subscribe_is_own_inbox_and_rpc() {
        let p = permissions(Role::Agent, "claude", &tenant()).expect("agent");
        assert_eq!(
            p.subscribe,
            vec!["_INBOX.claude.>", "a2a.v1.local.rpc.claude.*"]
        );
    }

    // 0010-062: gateway publishes rpc, its own card, presence and audit only.
    #[test]
    fn req_0010_062_gateway_has_no_event_writes() {
        let p = permissions(Role::Gateway, "gateway", &tenant()).expect("gateway");
        assert!(has(&p.publish, "a2a.v1.local.rpc.*.*"));
        assert!(has(&p.publish, "$KV.a2ab-local-cards.gateway"));
        assert!(!p.publish.iter().any(|s| s.contains(".ev.")));
        assert!(!p.responses);
    }

    // 0010-063: operators are read-only on KV and publish only their own audit subject.
    #[test]
    fn req_0010_063_operator_is_read_only_on_kv() {
        let p = permissions(Role::Operator, "tui", &tenant()).expect("operator");
        assert!(has(&p.publish, "a2a.v1.local.audit.tui"));
        assert!(!p.publish.iter().any(|s| s.starts_with("$KV.")));
        assert!(!p.publish.iter().any(|s| s.starts_with("$O.")));
        assert!(has(&p.publish, "$JS.API.DIRECT.GET.KV_a2ab-local-cards"));
        assert!(!has(
            &p.publish,
            "$JS.API.STREAM.CREATE.KV_a2ab-local-cards"
        ));
    }

    // 0010-064: supervisor creates and reads streams and runs the archiver only.
    #[test]
    fn req_0010_064_supervisor_manages_streams_and_archiver() {
        let p = permissions(Role::Supervisor, "supervisor", &tenant()).expect("supervisor");
        assert!(has(&p.publish, "$JS.API.STREAM.CREATE.A2AB_EV_local"));
        assert!(has(&p.publish, "$JS.API.STREAM.UPDATE.A2A_AUDIT_local"));
        assert!(has(
            &p.publish,
            "$JS.API.CONSUMER.MSG.NEXT.A2A_AUDIT_local.audit-archiver"
        ));
        assert!(has(&p.publish, "a2a.v1.local.ev.*.>"));
        assert!(
            !p.publish
                .iter()
                .any(|s| s == "$JS.API.STREAM.DELETE.A2AB_EV_local")
        );
        assert!(!p.publish.iter().any(|s| s.starts_with("$KV.")));
    }

    // Every binding queries account info at connect (`connection::connect`), so every
    // role, including the supervisor, may publish `$JS.API.INFO`.
    #[test]
    fn req_0010_064_every_role_may_query_account_info() {
        let t = tenant();
        for (role, identity) in [
            (Role::Agent, "omp-1"),
            (Role::Gateway, "gateway"),
            (Role::Operator, "tui"),
            (Role::Supervisor, "supervisor"),
        ] {
            let p = permissions(role, identity, &t).expect("role");
            assert!(
                has(&p.publish, "$JS.API.INFO"),
                "{role:?} lacks $JS.API.INFO"
            );
        }
    }

    // NATS wildcard matching of a publish subject against an allow-list pattern. A `>`
    // in the pattern matches one or more remaining tokens, including a literal `>`.
    fn nats_matches(pattern: &str, subject: &str) -> bool {
        let mut want = pattern.split('.');
        let mut got = subject.split('.');
        loop {
            match (want.next(), got.next()) {
                (Some(">"), Some(_)) => return true,
                (Some("*"), Some(_)) => {}
                (Some(w), Some(g)) if w == g => {}
                (None, None) => return true,
                _ => return false,
            }
        }
    }

    // 0010-054: async-nats creates the archiver with its filter in the create subject
    // (`CONSUMER.CREATE.<stream>.<durable>.<filter>`), so the supervisor may publish it.
    #[test]
    fn req_0010_054_archiver_create_with_filter_is_allowed() {
        let p = permissions(Role::Supervisor, "supervisor", &tenant()).expect("supervisor");
        let create = "$JS.API.CONSUMER.CREATE.A2A_AUDIT_local.audit-archiver.a2a.v1.local.audit.>";
        assert!(p.publish.iter().any(|s| nats_matches(s, create)));
        let durable =
            "$JS.API.CONSUMER.DURABLE.CREATE.A2A_AUDIT_local.audit-archiver.a2a.v1.local.audit.>";
        assert!(p.publish.iter().any(|s| nats_matches(s, durable)));
        let other = "$JS.API.CONSUMER.CREATE.A2A_AUDIT_local.other-consumer.a2a.v1.local.audit.>";
        assert!(!p.publish.iter().any(|s| nats_matches(s, other)));
    }

    // 0010-011: identities are fixed per role; an agent may not take a reserved name.
    #[test]
    fn req_0010_011_roles_reject_wrong_identities() {
        assert!(permissions(Role::Gateway, "omp", &tenant()).is_err());
        assert!(permissions(Role::Operator, "omp", &tenant()).is_err());
        assert!(permissions(Role::Supervisor, "gateway", &tenant()).is_err());
        assert!(permissions(Role::Agent, "gateway", &tenant()).is_err());
        assert!(permissions(Role::Agent, "Bad Name", &tenant()).is_err());
    }

    #[test]
    fn server_conf_carries_users_responses_and_max_payload() {
        let users = vec![ServerUser {
            public_key: "UABC".to_owned(),
            permissions: permissions(Role::Agent, "omp", &tenant()).expect("agent"),
        }];
        let spec = ServerSpec {
            host: "127.0.0.1",
            port: 4555,
            store_dir: Path::new("C:\\tmp\\store"),
            users: &users,
        };
        let conf = render_server_conf(&spec);
        assert!(conf.contains("port: 4555"));
        assert!(conf.contains("max_payload: 1048576"));
        assert!(conf.contains("store_dir: \"C:/tmp/store\""));
        assert!(conf.contains("nkey: \"UABC\""));
        assert!(conf.contains("allow_responses: { max: 1, expires: \"2m\" }"));
    }

    #[test]
    fn server_conf_omits_responses_for_non_responders() {
        let users = vec![ServerUser {
            public_key: "UGW".to_owned(),
            permissions: permissions(Role::Gateway, "gateway", &tenant()).expect("gw"),
        }];
        let spec = ServerSpec {
            host: "127.0.0.1",
            port: 1,
            store_dir: Path::new("/tmp/s"),
            users: &users,
        };
        assert!(!render_server_conf(&spec).contains("allow_responses"));
    }
}
