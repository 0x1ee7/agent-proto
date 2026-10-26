//! Capability tokens and grants (RFC-0007 [0007-090..119], `delegation.proto`).
//!
//! Tokens are proto3 JSON objects ([0007-092]) checked on the object as received ([0007-091]).
//! [`verify_grant`] runs the checks in the order [0007-106] gives: signatures of every token in
//! the chain, chain invariants, audience, task, validity window, and finally revocation of every
//! kid in the chain. Expiry is hard: nothing is granted at or after `issuedAt + ttlMs`.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Mutex, MutexGuard, PoisonError};

use a2a_types::ids::AgentId;
use serde_json::{Map, Value};

use crate::canon::{b64_decode_canonical, b64_encode, contains_float, jcs, jcs_without, sha256};
use crate::clock::{format_ts_ms, parse_ts_ms};
use crate::decision::VerifiedDecision;
use crate::envelope::VerifyContext;
use crate::error::{BusError, Reason, SecurityError};
use crate::identity::{Kid, Signer, Verifier};
use crate::mode::McpMode;
use crate::trust::{OPERATOR_AGENT, Role, TrustEntry, TrustStore};

/// Deepest permitted delegation chain: a grant has at most this many ancestors ([0007-104]).
pub const MAX_DEPTH: u32 = 3;

const MCP_PREFIX: &str = "mcp:";
const DIGEST_PREFIX: &str = "sha-256:";
const SIGNATURE_LEN: usize = 64;
const GRANT_SECRET_LEN: usize = 32;
const TOKEN_MEMBERS: [&str; 13] = [
    "tokenId",
    "issuer",
    "issuerKid",
    "audience",
    "taskId",
    "contextId",
    "scope",
    "issuedAt",
    "ttlMs",
    "maxUses",
    "ancestors",
    "depth",
    "signature",
];
const SCOPE_MEMBERS: [&str; 4] = ["resource", "actions", "constraints", "mcpMode"];
const REFERENCE_MEMBERS: [&str; 5] = ["tokenId", "tokenDigest", "delegator", "taskId", "token"];

fn invalid() -> BusError {
    BusError::new(Reason::CapabilityInvalid)
}

fn chain_invalid() -> BusError {
    BusError::new(Reason::CapabilityChainInvalid)
}

fn task_mismatch() -> BusError {
    BusError::new(Reason::CapabilityTaskMismatch)
}

/// One scope entry: what a grant allows on one resource ([0007-112]).
#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub resource: String,
    pub actions: Vec<String>,
    /// Passed to the proxy for enforcement; this crate does not interpret them ([0007-114]).
    pub constraints: Map<String, Value>,
    pub mode: McpMode,
}

/// One token of a chain, parsed from its JSON object. Signatures are checked separately.
#[derive(Debug)]
struct Node {
    object: Map<String, Value>,
    token_id: String,
    issuer: String,
    issuer_kid: String,
    audience: AgentId,
    task_id: String,
    context_id: String,
    scope: Vec<Scope>,
    issued_ms: i64,
    expires_ms: i64,
    max_uses: u32,
    depth: u32,
    signature: String,
    ancestors: Vec<Map<String, Value>>,
}

impl Node {
    fn parse(object: &Map<String, Value>) -> Result<Self, BusError> {
        reject_unknown(object, &TOKEN_MEMBERS)?;
        let audience = AgentId::parse(required_str(object, "audience")?).map_err(|_| invalid())?;
        let issued_ms = parse_ts_ms(required_str(object, "issuedAt")?).ok_or_else(invalid)?;
        let ttl_ms = parse_ttl(object)?;
        let expires_ms = i64::try_from(ttl_ms)
            .ok()
            .and_then(|ttl| issued_ms.checked_add(ttl))
            .ok_or_else(invalid)?;
        let max_uses = u32_member(object, "maxUses", 1)?;
        if max_uses == 0 {
            return Err(invalid());
        }
        let depth = u32_member(object, "depth", 0)?;
        let ancestors = parse_ancestors(object)?;
        if depth > MAX_DEPTH || usize::try_from(depth).ok() != Some(ancestors.len()) {
            return Err(chain_invalid());
        }
        Ok(Self {
            object: object.clone(),
            token_id: required_str(object, "tokenId")?.to_owned(),
            issuer: required_str(object, "issuer")?.to_owned(),
            issuer_kid: required_str(object, "issuerKid")?.to_owned(),
            audience,
            task_id: required_str(object, "taskId")?.to_owned(),
            context_id: str_member(object, "contextId")?
                .unwrap_or_default()
                .to_owned(),
            scope: parse_scopes(object)?,
            issued_ms,
            expires_ms,
            max_uses,
            depth,
            signature: required_str(object, "signature")?.to_owned(),
            ancestors,
        })
    }
}

/// A token whose signature has been checked, with the trust entry that vouched for it.
struct Link<'a> {
    node: Node,
    entry: &'a TrustEntry,
}

impl Link<'_> {
    fn fail(&self, reason: Reason) -> BusError {
        BusError::new(reason).with_kid(self.entry.kid().to_string())
    }
}

fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), BusError> {
    if object.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn str_member<'o>(object: &'o Map<String, Value>, name: &str) -> Result<Option<&'o str>, BusError> {
    match object.get(name) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(invalid()),
    }
}

fn required_str<'o>(object: &'o Map<String, Value>, name: &str) -> Result<&'o str, BusError> {
    match str_member(object, name)? {
        Some(text) if !text.is_empty() => Ok(text),
        _ => Err(invalid()),
    }
}

fn u32_member(object: &Map<String, Value>, name: &str, default: u32) -> Result<u32, BusError> {
    match object.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .ok_or_else(invalid),
    }
}

/// `ttlMs` is a proto3 64-bit integer, so it is a canonical decimal string ([0007-092]).
fn parse_ttl(object: &Map<String, Value>) -> Result<u64, BusError> {
    let text = required_str(object, "ttlMs")?;
    let canonical = text.len() <= 20
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if !canonical {
        return Err(invalid());
    }
    match text.parse::<u64>() {
        Ok(ttl) if ttl > 0 => Ok(ttl),
        _ => Err(invalid()),
    }
}

fn parse_ancestors(object: &Map<String, Value>) -> Result<Vec<Map<String, Value>>, BusError> {
    let items = match object.get("ancestors") {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(invalid()),
    };
    if items.len() > MAX_DEPTH as usize {
        return Err(chain_invalid());
    }
    items
        .iter()
        .map(|item| item.as_object().cloned().ok_or_else(invalid))
        .collect()
}

fn parse_scopes(object: &Map<String, Value>) -> Result<Vec<Scope>, BusError> {
    let scopes = match object.get("scope") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(parse_scope)
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(invalid()),
    };
    // One entry per resource keeps attenuation and authorization unambiguous.
    let distinct = scopes.iter().enumerate().all(|(i, scope)| {
        scopes[..i]
            .iter()
            .all(|earlier| earlier.resource != scope.resource)
    });
    if distinct { Ok(scopes) } else { Err(invalid()) }
}

fn parse_scope(value: &Value) -> Result<Scope, BusError> {
    let object = value.as_object().ok_or_else(invalid)?;
    reject_unknown(object, &SCOPE_MEMBERS)?;
    let resource = required_str(object, "resource")?;
    let constraints = match object.get("constraints") {
        None => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return Err(invalid()),
    };
    Ok(Scope {
        resource: resource.to_owned(),
        actions: string_array(object, "actions")?,
        constraints,
        mode: parse_mode(object.get("mcpMode"))?,
    })
}

fn string_array(object: &Map<String, Value>, name: &str) -> Result<Vec<String>, BusError> {
    let Some(Value::Array(items)) = object.get(name) else {
        return Err(invalid());
    };
    let actions = items
        .iter()
        .map(|item| match item {
            Value::String(text) if !text.is_empty() => Ok(text.clone()),
            _ => Err(invalid()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if actions.is_empty() {
        Err(invalid())
    } else {
        Ok(actions)
    }
}

/// `mcpMode` accepts enum names or numbers. Absent or unspecified means proxy ([0007-112]).
fn parse_mode(value: Option<&Value>) -> Result<McpMode, BusError> {
    match value {
        None => Ok(McpMode::Proxy),
        Some(Value::String(name)) => match name.as_str() {
            "MCP_MODE_UNSPECIFIED" | "MCP_MODE_PROXY" => Ok(McpMode::Proxy),
            "MCP_MODE_DIRECT" => Ok(McpMode::Direct),
            _ => Err(invalid()),
        },
        Some(Value::Number(number)) => match number.as_u64() {
            Some(0 | 1) => Ok(McpMode::Proxy),
            Some(2) => Ok(McpMode::Direct),
            _ => Err(invalid()),
        },
        Some(_) => Err(invalid()),
    }
}

fn digest_of(object: &Map<String, Value>) -> Result<String, BusError> {
    let bytes = jcs(object).map_err(|_| invalid())?;
    Ok(format!("{DIGEST_PREFIX}{}", b64_encode(&sha256(&bytes))))
}

/// Checks one token's signature against its issuer key ([0007-090], [0007-106](a)).
fn check_signature<'a>(ctx: &VerifyContext<'a>, node: &Node) -> Result<&'a TrustEntry, BusError> {
    let trust: &'a TrustStore = ctx.trust;
    let kid_text = node.issuer_kid.as_str();
    if Kid::parse(kid_text).is_none() {
        return Err(invalid());
    }
    let entry = trust
        .entry(kid_text)
        .filter(|entry| entry.serves(ctx.tenant))
        .ok_or_else(|| BusError::new(Reason::SignatureKeyUnknown))?;
    let attach = |reason: Reason| BusError::new(reason).with_kid(kid_text);
    let role_ok = match node.depth {
        0 => entry.role() == Role::Operator,
        _ => entry.role() == Role::Agent,
    };
    if !role_ok {
        return Err(attach(Reason::KeyRoleDenied));
    }
    if entry.agent().as_str() != node.issuer.as_str() {
        return Err(attach(Reason::SignatureIdentityMismatch));
    }
    let message =
        jcs_without(&node.object, "signature").map_err(|_| attach(Reason::CapabilityInvalid))?;
    let signature = b64_decode_canonical(&node.signature)
        .filter(|bytes| bytes.len() == SIGNATURE_LEN)
        .ok_or_else(|| attach(Reason::CapabilityInvalid))?;
    if !entry.verify_bytes(&message, &signature) {
        return Err(attach(Reason::CapabilityInvalid));
    }
    Ok(entry)
}

/// Chain invariants for root-first `links` ([0007-104], [0007-105]).
fn check_chain(links: &[Link<'_>]) -> Result<(), BusError> {
    let leaf = links.last().ok_or_else(invalid)?;
    let mut seen: Vec<&str> = Vec::with_capacity(links.len() * 2);
    for (index, link) in links.iter().enumerate() {
        let node = &link.node;
        let lineage = node
            .ancestors
            .iter()
            .eq(links[..index].iter().map(|earlier| &earlier.node.object));
        if usize::try_from(node.depth).ok() != Some(index)
            || !lineage
            || node.task_id != leaf.node.task_id
        {
            return Err(chain_invalid());
        }
        if index == 0 {
            if node.issuer != OPERATOR_AGENT {
                return Err(chain_invalid());
            }
        } else {
            let parent = &links[index - 1].node;
            if node.issuer.as_str() != parent.audience.as_str() {
                return Err(chain_invalid());
            }
            narrows(parent, node)?;
        }
        seen.push(node.issuer.as_str());
        if seen.contains(&node.audience.as_str()) {
            return Err(chain_invalid());
        }
        seen.push(node.audience.as_str());
    }
    Ok(())
}

/// A child may not outlive, outspend, or predate its parent; its scope must be a subset.
fn narrows(parent: &Node, child: &Node) -> Result<(), BusError> {
    if child.expires_ms > parent.expires_ms
        || child.issued_ms < parent.issued_ms
        || child.max_uses > parent.max_uses
    {
        return Err(chain_invalid());
    }
    if !scope_within(&child.scope, &parent.scope) {
        return Err(BusError::new(Reason::CapabilityScopeDenied));
    }
    Ok(())
}

fn scope_within(child: &[Scope], parent: &[Scope]) -> bool {
    child.iter().all(|wanted| {
        parent.iter().any(|allowed| {
            allowed.resource == wanted.resource
                && wanted
                    .actions
                    .iter()
                    .all(|action| allowed.actions.contains(action))
                && constraints_within(&wanted.constraints, &allowed.constraints)
                && (!wanted.resource.starts_with(MCP_PREFIX)
                    || mode_rank(wanted.mode) <= mode_rank(allowed.mode))
        })
    })
}

/// Child constraints keep every parent key. Arrays narrow to a subset; other values must match.
/// Extra child keys are allowed. A parent with no constraints allows anything.
fn constraints_within(child: &Map<String, Value>, parent: &Map<String, Value>) -> bool {
    parent.iter().all(|(key, bound)| match child.get(key) {
        None => false,
        Some(value) => match (value, bound) {
            (Value::Array(items), Value::Array(allowed)) => {
                items.iter().all(|item| allowed.contains(item))
            }
            _ => value == bound,
        },
    })
}

/// Proxy (1) is narrower than direct (2) ([0007-118]).
const fn mode_rank(mode: McpMode) -> u8 {
    match mode {
        McpMode::Proxy => 1,
        McpMode::Direct => 2,
    }
}

/// A token that passed [`verify_grant`], with the fields the recipient needs.
#[derive(Clone)]
pub struct VerifiedGrant {
    token_id: String,
    issuer: String,
    audience: AgentId,
    task_id: String,
    context_id: String,
    scope: Vec<Scope>,
    issued_ms: i64,
    expires_ms: i64,
    max_uses: u32,
    depth: u32,
    digest: String,
    /// The leaf object as received. Child tokens embed it as an ancestor.
    token: Map<String, Value>,
    ancestors: Vec<Map<String, Value>>,
    kids: Vec<Kid>,
}

impl fmt::Debug for VerifiedGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Logs record the token id and digest only ([0007-100]).
        f.debug_struct("VerifiedGrant")
            .field("token_id", &self.token_id)
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("task_id", &self.task_id)
            .field("expires_ms", &self.expires_ms)
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

impl VerifiedGrant {
    #[must_use]
    pub fn token_id(&self) -> &str {
        &self.token_id
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    #[must_use]
    pub const fn audience(&self) -> &AgentId {
        &self.audience
    }

    #[must_use]
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    #[must_use]
    pub fn context_id(&self) -> &str {
        &self.context_id
    }

    #[must_use]
    pub fn scope(&self) -> &[Scope] {
        &self.scope
    }

    #[must_use]
    pub const fn issued_ms(&self) -> i64 {
        self.issued_ms
    }

    #[must_use]
    pub const fn expires_ms(&self) -> i64 {
        self.expires_ms
    }

    #[must_use]
    pub const fn max_uses(&self) -> u32 {
        self.max_uses
    }

    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    /// `sha-256:` plus base64url over the JCS form of the full token ([0007-102]).
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Kids of every token in the chain, leaf last. Used for revocation ([0007-110]).
    #[must_use]
    pub fn kids(&self) -> &[Kid] {
        &self.kids
    }

    /// Expiry and revocation at the clock's instant ([0007-106](e)(f), [0007-110], [0007-111]).
    pub fn recheck(&self, ctx: &VerifyContext<'_>) -> Result<(), BusError> {
        let now = ctx.clock.now_ms();
        if now >= self.expires_ms {
            return Err(
                BusError::new(Reason::CapabilityExpired).with_token_id(self.token_id.as_str())
            );
        }
        for kid in &self.kids {
            let text = kid.to_string();
            let revoked = || BusError::new(Reason::CapabilityRevoked).with_kid(text.clone());
            let entry = ctx.trust.entry(&text).ok_or_else(revoked)?;
            entry.check_validity(now).map_err(|_| revoked())?;
        }
        Ok(())
    }

    /// Default-deny lookup of the scope entry for `resource` and `action` ([0007-109]).
    pub fn authorize(&self, resource: &str, action: &str) -> Result<&Scope, BusError> {
        self.scope
            .iter()
            .find(|scope| {
                scope.resource == resource
                    && scope
                        .actions
                        .iter()
                        .any(|allowed| allowed.as_str() == action)
            })
            .ok_or_else(|| self.denied())
    }

    /// Scope check for one MCP call ([0007-112..119]). Cross-host calls are refused in M1. Direct
    /// mode also needs the recipient's `mcp_direct` flag ([0007-118]).
    pub fn authorize_mcp(
        &self,
        ctx: &VerifyContext<'_>,
        resource: &str,
        action: &str,
        same_host: bool,
    ) -> Result<&Scope, BusError> {
        self.recheck(ctx)?;
        if !resource.starts_with(MCP_PREFIX) || !same_host {
            return Err(self.denied());
        }
        let scope = self.authorize(resource, action)?;
        if scope.mode == McpMode::Direct {
            let direct_ok = ctx
                .trust
                .active_for_agent(&self.audience)
                .is_some_and(TrustEntry::mcp_direct);
            if !direct_ok {
                return Err(self.denied());
            }
        }
        Ok(scope)
    }

    fn denied(&self) -> BusError {
        BusError::new(Reason::CapabilityScopeDenied).with_token_id(self.token_id.as_str())
    }
}

/// Verifies a full token received for `recipient` on `task_id` ([0007-106]). The token's JSON is
/// checked as received ([0007-091]); floats are refused before any cryptography ([0007-092]).
pub fn verify_grant(
    ctx: &VerifyContext<'_>,
    token: &Value,
    recipient: &AgentId,
    task_id: &str,
) -> Result<VerifiedGrant, BusError> {
    if contains_float(token) {
        return Err(invalid());
    }
    let object = token.as_object().ok_or_else(invalid)?;
    let leaf = Node::parse(object)?;
    let mut nodes = Vec::with_capacity(leaf.ancestors.len() + 1);
    for ancestor in &leaf.ancestors {
        nodes.push(Node::parse(ancestor)?);
    }
    nodes.push(leaf);

    let mut links = Vec::with_capacity(nodes.len());
    for node in nodes {
        let entry = check_signature(ctx, &node)?;
        links.push(Link { node, entry });
    }
    check_chain(&links)?;

    let leaf_link = links.last().ok_or_else(invalid)?;
    if leaf_link.node.audience != *recipient {
        return Err(BusError::new(Reason::CapabilityAudienceMismatch));
    }
    if leaf_link.node.task_id != task_id {
        return Err(task_mismatch());
    }

    let now = ctx.clock.now_ms();
    for link in &links {
        if now < link.node.issued_ms.saturating_sub(link.entry.max_skew_ms()) {
            return Err(link.fail(Reason::CapabilityNotYetValid));
        }
        if now >= link.node.expires_ms {
            return Err(link.fail(Reason::CapabilityExpired));
        }
    }
    for link in &links {
        if link.entry.check_validity(now).is_err() {
            return Err(link.fail(Reason::CapabilityRevoked));
        }
    }

    let leaf = links.pop().ok_or_else(invalid)?;
    let digest = digest_of(&leaf.node.object)?;
    let mut kids: Vec<Kid> = links.iter().map(|link| link.entry.kid().clone()).collect();
    kids.push(leaf.entry.kid().clone());
    let ancestors: Vec<Map<String, Value>> =
        links.into_iter().map(|link| link.node.object).collect();
    let node = leaf.node;
    Ok(VerifiedGrant {
        token_id: node.token_id,
        issuer: node.issuer,
        audience: node.audience,
        task_id: node.task_id,
        context_id: node.context_id,
        scope: node.scope,
        issued_ms: node.issued_ms,
        expires_ms: node.expires_ms,
        max_uses: node.max_uses,
        depth: node.depth,
        digest,
        token: node.object,
        ancestors,
        kids,
    })
}

/// Verified tokens by `tokenId`, held until each token's expiry ([0007-101]).
#[derive(Debug, Default)]
pub struct TokenCache {
    grants: Mutex<HashMap<String, VerifiedGrant>>,
}

impl TokenCache {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, VerifiedGrant>> {
        self.grants.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores `grant`. A different digest under the same `tokenId` is refused ([0007-102]).
    fn remember(&self, now_ms: i64, grant: &VerifiedGrant) -> Result<(), BusError> {
        let mut grants = self.lock();
        grants.retain(|_, held| now_ms < held.expires_ms);
        if let Some(held) = grants.get(grant.token_id()) {
            return if held.digest == grant.digest {
                Ok(())
            } else {
                Err(invalid())
            };
        }
        grants.insert(grant.token_id().to_owned(), grant.clone());
        Ok(())
    }

    fn lookup(&self, now_ms: i64, token_id: &str) -> Option<VerifiedGrant> {
        let mut grants = self.lock();
        grants.retain(|_, held| now_ms < held.expires_ms);
        grants.get(token_id).cloned()
    }
}

/// Resolves a `DelegationRef` into a verified grant ([0007-101], [0007-102]). A full token is
/// verified and cached. A bare reference needs a cached, unexpired token with a matching digest.
pub fn resolve_reference(
    ctx: &VerifyContext<'_>,
    cache: &TokenCache,
    reference: &Value,
    recipient: &AgentId,
    task_id: &str,
) -> Result<VerifiedGrant, BusError> {
    let object = reference.as_object().ok_or_else(invalid)?;
    reject_unknown(object, &REFERENCE_MEMBERS)?;
    let token_id = required_str(object, "tokenId")?;
    let digest = required_str(object, "tokenDigest")?;
    let delegator = required_str(object, "delegator")?;
    if str_member(object, "taskId")?.unwrap_or_default() != task_id {
        return Err(task_mismatch());
    }
    match object.get("token") {
        Some(token) => {
            let full = token.as_object().ok_or_else(invalid)?;
            if digest_of(full)? != digest {
                return Err(invalid());
            }
            let grant = verify_grant(ctx, token, recipient, task_id)?;
            if grant.token_id != token_id || grant.issuer != delegator {
                return Err(invalid());
            }
            cache.remember(ctx.clock.now_ms(), &grant)?;
            Ok(grant)
        }
        None => {
            let grant = cache
                .lookup(ctx.clock.now_ms(), token_id)
                .ok_or_else(invalid)?;
            if grant.digest != digest || grant.issuer != delegator {
                return Err(invalid());
            }
            if grant.audience != *recipient {
                return Err(BusError::new(Reason::CapabilityAudienceMismatch));
            }
            if grant.task_id != task_id {
                return Err(task_mismatch());
            }
            grant.recheck(ctx)?;
            Ok(grant)
        }
    }
}

/// Counts accepted redemptions per token ([0007-107]). The count lives in the recipient
/// supervisor's memory (T4).
#[derive(Debug, Default)]
pub struct UseLedger {
    uses: Mutex<HashMap<String, u32>>,
}

impl UseLedger {
    /// Records one redemption of `grant`. Refused when the grant is expired or revoked, or when
    /// `maxUses` redemptions have already been accepted.
    pub fn redeem(&self, ctx: &VerifyContext<'_>, grant: &VerifiedGrant) -> Result<(), BusError> {
        grant.recheck(ctx)?;
        let mut uses = self.uses.lock().unwrap_or_else(PoisonError::into_inner);
        let count = uses.entry(grant.token_id.clone()).or_insert(0);
        if *count >= grant.max_uses {
            return Err(
                BusError::new(Reason::CapabilityUsed).with_token_id(grant.token_id.as_str())
            );
        }
        *count = count.saturating_add(1);
        Ok(())
    }
}

/// The secret a local MCP client presents to a grant's proxy ([0007-116]). It is bound to one
/// `tokenId` and one audience, and it expires with the token.
pub struct GrantCredential {
    token_id: String,
    audience: AgentId,
    expires_ms: i64,
    secret: String,
}

impl GrantCredential {
    /// Mints a fresh 256-bit secret for `grant`. Refused once the grant has expired.
    pub fn issue(grant: &VerifiedGrant, now_ms: i64) -> Result<Self, SecurityError> {
        if now_ms >= grant.expires_ms {
            return Err(SecurityError::Invalid("grant already expired"));
        }
        let mut bytes = [0u8; GRANT_SECRET_LEN];
        getrandom::fill(&mut bytes).map_err(|_| SecurityError::Random)?;
        let secret = b64_encode(&bytes);
        bytes.fill(0);
        Ok(Self {
            token_id: grant.token_id.clone(),
            audience: grant.audience.clone(),
            expires_ms: grant.expires_ms,
            secret,
        })
    }

    #[must_use]
    pub fn token_id(&self) -> &str {
        &self.token_id
    }

    #[must_use]
    pub const fn audience(&self) -> &AgentId {
        &self.audience
    }

    #[must_use]
    pub const fn expires_ms(&self) -> i64 {
        self.expires_ms
    }

    /// The secret. Only supervisor IPC and the recipient's `mcpServers` entry may carry it.
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// True when `presented` is the secret and the grant has not expired.
    #[must_use]
    pub fn matches(&self, presented: &str, now_ms: i64) -> bool {
        now_ms < self.expires_ms && constant_time_eq(self.secret.as_bytes(), presented.as_bytes())
    }
}

impl fmt::Debug for GrantCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrantCredential")
            .field("token_id", &self.token_id)
            .field("audience", &self.audience)
            .field("expires_ms", &self.expires_ms)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Refuses any proxy listen address that is not loopback ([0007-113]).
pub fn check_loopback_bind(addr: &SocketAddr) -> Result<(), SecurityError> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(SecurityError::Invalid("proxy must listen on loopback"))
    }
}

/// What the caller wants a token to say. Issuance verifies the sealed result again.
#[derive(Clone, Debug)]
pub struct TokenSpec {
    pub token_id: String,
    pub audience: AgentId,
    pub task_id: String,
    pub context_id: String,
    pub scope: Vec<Scope>,
    pub issued_ms: i64,
    pub ttl_ms: u64,
    pub max_uses: u32,
}

/// A sealed token and its digest. The body holds no secrets ([0007-100]).
pub struct IssuedToken {
    pub token_id: String,
    pub digest: String,
    pub token: Value,
}

impl fmt::Debug for IssuedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedToken")
            .field("token_id", &self.token_id)
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

/// Creates a root grant from an approved decision for the same task ([0007-103]). `signer` must
/// hold an operator-role key.
pub fn issue_root(
    ctx: &VerifyContext<'_>,
    signer: &dyn Signer,
    decision: &VerifiedDecision,
    spec: &TokenSpec,
) -> Result<IssuedToken, BusError> {
    validate_spec(spec)?;
    if !decision.allows() {
        return Err(invalid());
    }
    if decision.task_id() != spec.task_id.as_str() {
        return Err(task_mismatch());
    }
    let object = emit(signer, spec, OPERATOR_AGENT, 0, Vec::new())?;
    seal(ctx, signer, object, spec)
}

/// Delegates `parent` to `spec.audience` ([0007-104]). `signer` must be the parent's audience.
pub fn issue_child(
    ctx: &VerifyContext<'_>,
    signer: &dyn Signer,
    parent: &VerifiedGrant,
    spec: &TokenSpec,
) -> Result<IssuedToken, BusError> {
    validate_spec(spec)?;
    parent.recheck(ctx)?;
    if spec.task_id != parent.task_id {
        return Err(task_mismatch());
    }
    if signer.kid().agent() != &parent.audience {
        return Err(BusError::new(Reason::CapabilityAudienceMismatch));
    }
    let depth = parent.depth + 1;
    if depth > MAX_DEPTH {
        return Err(chain_invalid());
    }
    let mut ancestors = parent.ancestors.clone();
    ancestors.push(parent.token.clone());
    let object = emit(signer, spec, parent.audience.as_str(), depth, ancestors)?;
    seal(ctx, signer, object, spec)
}

fn validate_spec(spec: &TokenSpec) -> Result<(), BusError> {
    if spec.token_id.is_empty() || spec.task_id.is_empty() || spec.ttl_ms == 0 || spec.max_uses == 0
    {
        return Err(invalid());
    }
    Ok(())
}

/// Builds the unsigned token object in proto3 JSON form. Zero values are omitted.
fn emit(
    signer: &dyn Signer,
    spec: &TokenSpec,
    issuer: &str,
    depth: u32,
    ancestors: Vec<Map<String, Value>>,
) -> Result<Map<String, Value>, BusError> {
    let issued = format_ts_ms(spec.issued_ms).map_err(|_| invalid())?;
    let mut object = Map::new();
    object.insert("tokenId".to_owned(), Value::String(spec.token_id.clone()));
    object.insert("issuer".to_owned(), Value::String(issuer.to_owned()));
    object.insert(
        "issuerKid".to_owned(),
        Value::String(signer.kid().to_string()),
    );
    object.insert(
        "audience".to_owned(),
        Value::String(spec.audience.as_str().to_owned()),
    );
    object.insert("taskId".to_owned(), Value::String(spec.task_id.clone()));
    if !spec.context_id.is_empty() {
        object.insert(
            "contextId".to_owned(),
            Value::String(spec.context_id.clone()),
        );
    }
    if !spec.scope.is_empty() {
        let scopes: Vec<Value> = spec.scope.iter().map(scope_json).collect();
        object.insert("scope".to_owned(), Value::Array(scopes));
    }
    object.insert("issuedAt".to_owned(), Value::String(issued));
    object.insert("ttlMs".to_owned(), Value::String(spec.ttl_ms.to_string()));
    object.insert("maxUses".to_owned(), Value::from(spec.max_uses));
    if !ancestors.is_empty() {
        let items: Vec<Value> = ancestors.into_iter().map(Value::Object).collect();
        object.insert("ancestors".to_owned(), Value::Array(items));
    }
    if depth > 0 {
        object.insert("depth".to_owned(), Value::from(depth));
    }
    Ok(object)
}

fn scope_json(scope: &Scope) -> Value {
    let mut object = Map::new();
    object.insert("resource".to_owned(), Value::String(scope.resource.clone()));
    let actions: Vec<Value> = scope.actions.iter().cloned().map(Value::String).collect();
    object.insert("actions".to_owned(), Value::Array(actions));
    if !scope.constraints.is_empty() {
        object.insert(
            "constraints".to_owned(),
            Value::Object(scope.constraints.clone()),
        );
    }
    if scope.resource.starts_with(MCP_PREFIX) {
        let name = match scope.mode {
            McpMode::Proxy => "MCP_MODE_PROXY",
            McpMode::Direct => "MCP_MODE_DIRECT",
        };
        object.insert("mcpMode".to_owned(), Value::String(name.to_owned()));
    }
    Value::Object(object)
}

/// Signs `object` over its JCS form ([0007-090]) and checks the result with [`verify_grant`].
fn seal(
    ctx: &VerifyContext<'_>,
    signer: &dyn Signer,
    mut object: Map<String, Value>,
    spec: &TokenSpec,
) -> Result<IssuedToken, BusError> {
    let message = jcs(&object).map_err(|_| invalid())?;
    let signature = signer.sign_bytes(&message);
    object.insert(
        "signature".to_owned(),
        Value::String(b64_encode(&signature)),
    );
    let token = Value::Object(object);
    let grant = verify_grant(ctx, &token, &spec.audience, &spec.task_id)?;
    Ok(IssuedToken {
        token_id: grant.token_id,
        digest: grant.digest,
        token,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::identity::Ed25519Signer;
    use crate::mode::Policy;
    use crate::testkit::{agent, key_block, key_block_with_role, signer, store};
    use a2a_types::ids::TenantId;
    use a2a_types::proto::a2abus::ApprovalOptionKind;
    use serde_json::json;

    const T0: i64 = 1_767_225_600_000;
    const TASK: &str = "task-1";
    const SEEDS: [(&str, u8); 6] = [
        ("operator", 1),
        ("alice", 2),
        ("bob", 3),
        ("carol", 4),
        ("dave", 5),
        ("erin", 6),
    ];

    struct Fixture {
        trust: TrustStore,
        clock: ManualClock,
        tenant: TenantId,
    }

    impl Fixture {
        fn new() -> Self {
            Self::trusting("", "")
        }

        /// Every test key. `bob_extra` and `operator_extra` add lines to those two entries.
        fn trusting(bob_extra: &str, operator_extra: &str) -> Self {
            let body = [
                key_block_with_role("operator", 1, "operator", operator_extra),
                key_block("alice", 2, ""),
                key_block("bob", 3, bob_extra),
                key_block("carol", 4, ""),
                key_block("dave", 5, ""),
                key_block("erin", 6, ""),
            ]
            .concat();
            Self {
                trust: store(&body),
                clock: ManualClock::new(T0),
                tenant: TenantId::parse("local").expect("tenant id"),
            }
        }

        fn ctx(&self) -> VerifyContext<'_> {
            VerifyContext {
                trust: &self.trust,
                clock: &self.clock,
                policy: Policy::default(),
                tenant: &self.tenant,
            }
        }
    }

    fn key(name: &str) -> Ed25519Signer {
        let seed = SEEDS
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, seed)| *seed);
        signer(name, seed.expect("known test key"))
    }

    fn scope(resource: &str, actions: &[&str]) -> Scope {
        Scope {
            resource: resource.to_owned(),
            actions: actions.iter().map(|action| (*action).to_owned()).collect(),
            constraints: Map::new(),
            mode: McpMode::Proxy,
        }
    }

    fn spec(token_id: &str, audience: &str) -> TokenSpec {
        TokenSpec {
            token_id: token_id.to_owned(),
            audience: agent(audience),
            task_id: TASK.to_owned(),
            context_id: "ctx-1".to_owned(),
            scope: vec![scope("file:/a", &["read"])],
            issued_ms: T0,
            ttl_ms: 60_000,
            max_uses: 1,
        }
    }

    fn root_token(fx: &Fixture, token_spec: &TokenSpec) -> IssuedToken {
        let decision = VerifiedDecision::for_test(TASK, ApprovalOptionKind::AllowOnce);
        issue_root(&fx.ctx(), &key("operator"), &decision, token_spec).expect("root token issues")
    }

    fn root_grant(fx: &Fixture, token_spec: &TokenSpec) -> VerifiedGrant {
        let issued = root_token(fx, token_spec);
        verify_grant(&fx.ctx(), &issued.token, &token_spec.audience, TASK).expect("root verifies")
    }

    fn child_token(
        fx: &Fixture,
        parent: &VerifiedGrant,
        who: &str,
        token_spec: &TokenSpec,
    ) -> Result<IssuedToken, BusError> {
        issue_child(&fx.ctx(), &key(who), parent, token_spec)
    }

    fn child_grant(
        fx: &Fixture,
        parent: &VerifiedGrant,
        who: &str,
        audience: &str,
        token_id: &str,
    ) -> VerifiedGrant {
        let issued = child_token(fx, parent, who, &spec(token_id, audience)).expect("child issues");
        verify_grant(&fx.ctx(), &issued.token, &agent(audience), TASK).expect("child verifies")
    }

    fn object_of(value: &Value) -> Map<String, Value> {
        value.as_object().cloned().expect("token is a JSON object")
    }

    fn first_scope(object: &mut Map<String, Value>) -> &mut Map<String, Value> {
        object
            .get_mut("scope")
            .and_then(Value::as_array_mut)
            .and_then(|items| items.first_mut())
            .and_then(Value::as_object_mut)
            .expect("token has a scope entry")
    }

    /// Signs `object` again after a test edit, so only the parser or a later check can refuse it.
    fn resign(who: &dyn Signer, mut object: Map<String, Value>) -> Value {
        object.remove("signature");
        let message = jcs(&object).expect("canonical JSON");
        let signature = who.sign_bytes(&message);
        object.insert(
            "signature".to_owned(),
            Value::String(b64_encode(&signature)),
        );
        Value::Object(object)
    }

    fn refusal<T>(result: Result<T, BusError>) -> Option<Reason> {
        result.err().map(|err| err.reason())
    }

    fn reference(token_id: &str, digest: &str, delegator: &str, task: &str) -> Value {
        json!({
            "tokenId": token_id,
            "tokenDigest": digest,
            "delegator": delegator,
            "taskId": task,
        })
    }

    #[test]
    fn req_0007_090_signature_covers_every_member_but_itself() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        verify_grant(&fx.ctx(), &issued.token, &agent("bob"), TASK)
            .expect("untouched token verifies");
        let edits: [fn(&mut Map<String, Value>); 4] = [
            |object| {
                object.insert("taskId".to_owned(), json!("task-2"));
            },
            |object| {
                object.insert("maxUses".to_owned(), json!(2));
            },
            |object| {
                object.insert("ttlMs".to_owned(), json!("120000"));
            },
            |object| {
                object.insert("contextId".to_owned(), json!("ctx-2"));
            },
        ];
        for edit in edits {
            let mut object = object_of(&issued.token);
            edit(&mut object);
            let tampered = Value::Object(object);
            assert_eq!(
                refusal(verify_grant(&fx.ctx(), &tampered, &agent("bob"), TASK)),
                Some(Reason::CapabilityInvalid)
            );
        }
        let mut forged = object_of(&issued.token);
        forged.insert("signature".to_owned(), json!(b64_encode(&[0u8; 64])));
        assert_eq!(
            refusal(verify_grant(
                &fx.ctx(),
                &Value::Object(forged),
                &agent("bob"),
                TASK
            )),
            Some(Reason::CapabilityInvalid)
        );
    }

    #[test]
    fn req_0007_090_jcs_drops_only_the_top_level_signature() {
        let object = json!({
            "taskId": "task-1",
            "signature": "AAAA",
            "audience": "bob",
            "maxUses": 1,
            "ttlMs": "60000",
            "depth": 0,
            "issuer": "operator",
            "tokenId": "t1"
        });
        let bytes =
            jcs_without(object.as_object().expect("object"), "signature").expect("canonical");
        assert_eq!(
            String::from_utf8(bytes).expect("utf-8"),
            r#"{"audience":"bob","depth":0,"issuer":"operator","maxUses":1,"taskId":"task-1","tokenId":"t1","ttlMs":"60000"}"#
        );
    }

    #[test]
    fn req_0007_092_floats_are_refused_before_any_key_lookup() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        let mut object = object_of(&issued.token);
        first_scope(&mut object).insert("constraints".to_owned(), json!({"limit": 1.5}));
        let err = verify_grant(
            &fx.ctx(),
            &resign(&key("operator"), object),
            &agent("bob"),
            TASK,
        )
        .expect_err("float refused");
        assert_eq!(err.reason(), Reason::CapabilityInvalid);
        assert_eq!(err.kid(), None);
    }

    #[test]
    fn req_0007_092_ttl_is_a_canonical_decimal_string() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        for ttl in [
            json!(60000),
            json!("060000"),
            json!("+60000"),
            json!("0"),
            json!(""),
        ] {
            let mut object = object_of(&issued.token);
            object.insert("ttlMs".to_owned(), ttl);
            let token = resign(&key("operator"), object);
            assert_eq!(
                refusal(verify_grant(&fx.ctx(), &token, &agent("bob"), TASK)),
                Some(Reason::CapabilityInvalid)
            );
        }
    }

    #[test]
    fn req_0007_093_unknown_members_are_refused_in_tokens_scopes_and_references() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        let mut top = object_of(&issued.token);
        top.insert("extra".to_owned(), json!(true));
        assert_eq!(
            refusal(verify_grant(
                &fx.ctx(),
                &resign(&key("operator"), top),
                &agent("bob"),
                TASK
            )),
            Some(Reason::CapabilityInvalid)
        );
        let mut nested = object_of(&issued.token);
        first_scope(&mut nested).insert("extra".to_owned(), json!(true));
        assert_eq!(
            refusal(verify_grant(
                &fx.ctx(),
                &resign(&key("operator"), nested),
                &agent("bob"),
                TASK
            )),
            Some(Reason::CapabilityInvalid)
        );
        let mut extra_ref = reference("root", &issued.digest, "operator", TASK);
        extra_ref["extra"] = json!(1);
        assert_eq!(
            refusal(resolve_reference(
                &fx.ctx(),
                &TokenCache::default(),
                &extra_ref,
                &agent("bob"),
                TASK
            )),
            Some(Reason::CapabilityInvalid)
        );
    }

    #[test]
    fn req_0007_094_root_grants_need_an_operator_role_key() {
        let fx = Fixture::new();
        let decision = VerifiedDecision::for_test(TASK, ApprovalOptionKind::AllowOnce);
        let token_spec = spec("root", "bob");
        let err = issue_root(&fx.ctx(), &key("alice"), &decision, &token_spec)
            .expect_err("agent key cannot create a root grant");
        assert_eq!(err.reason(), Reason::KeyRoleDenied);
        assert!(err.kid().is_some());
    }

    #[test]
    fn req_0007_094_child_grants_need_an_agent_role_key() {
        let fx = Fixture::new();
        let parent = root_grant(&fx, &spec("root", "bob"));
        let legit =
            child_token(&fx, &parent, "bob", &spec("child", "carol")).expect("child issues");
        // The same child object, now claiming the operator-role key as its signer.
        let mut object = object_of(&legit.token);
        object.insert(
            "issuerKid".to_owned(),
            json!(key("operator").kid().to_string()),
        );
        let forged = resign(&key("operator"), object);
        assert_eq!(
            refusal(verify_grant(&fx.ctx(), &forged, &agent("carol"), TASK)),
            Some(Reason::KeyRoleDenied)
        );
    }

    #[test]
    fn req_0007_100_debug_output_never_shows_the_token_body() {
        let fx = Fixture::new();
        let mut marked = spec("root", "bob");
        marked.scope[0]
            .constraints
            .insert("note".to_owned(), json!("MARKER-7731"));
        let issued = root_token(&fx, &marked);
        let grant = verify_grant(&fx.ctx(), &issued.token, &agent("bob"), TASK).expect("verifies");
        let grant_debug = format!("{grant:?}");
        assert!(!grant_debug.contains("MARKER-7731"));
        assert!(grant_debug.contains(&issued.digest));
        let issued_debug = format!("{issued:?}");
        assert!(!issued_debug.contains("MARKER-7731"));
        assert!(issued_debug.contains(&issued.token_id));
        let signature = issued.token["signature"].as_str().expect("signed");
        assert!(!issued_debug.contains(signature));
    }

    #[test]
    fn req_0007_101_full_token_is_cached_and_bare_references_resolve_until_expiry() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        let cache = TokenCache::default();
        let full = json!({
            "tokenId": "root",
            "tokenDigest": issued.digest,
            "delegator": "operator",
            "taskId": TASK,
            "token": issued.token,
        });
        resolve_reference(&fx.ctx(), &cache, &full, &agent("bob"), TASK)
            .expect("first receipt verifies");
        let bare = reference("root", &issued.digest, "operator", TASK);
        resolve_reference(&fx.ctx(), &cache, &bare, &agent("bob"), TASK)
            .expect("cached token resolves");
        let uncached = reference("other", &issued.digest, "operator", TASK);
        assert_eq!(
            refusal(resolve_reference(
                &fx.ctx(),
                &cache,
                &uncached,
                &agent("bob"),
                TASK
            )),
            Some(Reason::CapabilityInvalid)
        );
        fx.clock.set(T0 + 60_000);
        assert_eq!(
            refusal(resolve_reference(
                &fx.ctx(),
                &cache,
                &bare,
                &agent("bob"),
                TASK
            )),
            Some(Reason::CapabilityInvalid)
        );
    }

    #[test]
    fn req_0007_102_digest_is_sha256_over_the_full_token() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        let cache = TokenCache::default();
        let wrong_hash = format!("sha-256:{}", b64_encode(&[0u8; 32]));
        let wrong_prefix = issued.digest.replacen("sha-256:", "sha-512:", 1);
        for digest in [wrong_hash.as_str(), wrong_prefix.as_str()] {
            let full = json!({
                "tokenId": "root",
                "tokenDigest": digest,
                "delegator": "operator",
                "taskId": TASK,
                "token": issued.token,
            });
            assert_eq!(
                refusal(resolve_reference(
                    &fx.ctx(),
                    &cache,
                    &full,
                    &agent("bob"),
                    TASK
                )),
                Some(Reason::CapabilityInvalid)
            );
        }
        let good = json!({
            "tokenId": "root",
            "tokenDigest": issued.digest,
            "delegator": "operator",
            "taskId": TASK,
            "token": issued.token,
        });
        resolve_reference(&fx.ctx(), &cache, &good, &agent("bob"), TASK).expect("verifies");
        let bare_wrong = reference("root", &wrong_hash, "operator", TASK);
        assert_eq!(
            refusal(resolve_reference(
                &fx.ctx(),
                &cache,
                &bare_wrong,
                &agent("bob"),
                TASK
            )),
            Some(Reason::CapabilityInvalid)
        );
    }

    #[test]
    fn req_0007_103_root_grant_only_from_an_approved_decision_for_its_task() {
        let fx = Fixture::new();
        let op = key("operator");
        let token_spec = spec("root", "bob");
        let denied = VerifiedDecision::for_test(TASK, ApprovalOptionKind::RejectOnce);
        assert_eq!(
            refusal(issue_root(&fx.ctx(), &op, &denied, &token_spec)),
            Some(Reason::CapabilityInvalid)
        );
        let other_task = VerifiedDecision::for_test("task-2", ApprovalOptionKind::AllowOnce);
        assert_eq!(
            refusal(issue_root(&fx.ctx(), &op, &other_task, &token_spec)),
            Some(Reason::CapabilityTaskMismatch)
        );
        let approved = VerifiedDecision::for_test(TASK, ApprovalOptionKind::AllowAlways);
        assert!(issue_root(&fx.ctx(), &op, &approved, &token_spec).is_ok());
    }

    #[test]
    fn req_0007_103_root_names_the_operator_and_depth_zero() {
        let fx = Fixture::new();
        let grant = root_grant(&fx, &spec("root", "bob"));
        assert_eq!(grant.issuer(), "operator");
        assert_eq!(grant.depth(), 0);
        assert!(grant.ancestors.is_empty());
        assert_eq!(
            grant.kids().first().map(|kid| kid.agent().as_str()),
            Some("operator")
        );
    }

    #[test]
    fn req_0007_104_child_narrows_scope_and_extends_the_ancestor_chain() {
        let fx = Fixture::new();
        let mut parent_spec = spec("root", "bob");
        parent_spec.scope = vec![scope("file:/a", &["read", "write"])];
        let parent = root_grant(&fx, &parent_spec);
        let mut child_spec = spec("child", "carol");
        child_spec.scope = vec![scope("file:/a", &["read"])];
        let issued = child_token(&fx, &parent, "bob", &child_spec).expect("narrower child issues");
        let child =
            verify_grant(&fx.ctx(), &issued.token, &agent("carol"), TASK).expect("child verifies");
        assert_eq!(child.depth(), 1);
        assert_eq!(child.issuer(), "bob");
        assert_eq!(child.ancestors.len(), 1);
        assert_eq!(child.ancestors[0], parent.token);
        assert_eq!(
            refusal(child_token(&fx, &parent, "carol", &child_spec)),
            Some(Reason::CapabilityAudienceMismatch)
        );
        let off_task = TokenSpec {
            task_id: "task-2".to_owned(),
            ..child_spec.clone()
        };
        assert_eq!(
            refusal(child_token(&fx, &parent, "bob", &off_task)),
            Some(Reason::CapabilityTaskMismatch)
        );
    }

    #[test]
    fn req_0007_104_child_scope_must_be_a_subset_per_resource_action_and_constraint() {
        let fx = Fixture::new();
        let mut limited = scope("file:/a", &["read"]);
        limited
            .constraints
            .insert("path_prefix".to_owned(), json!("/a"));
        let mut verbs = scope("api:v1", &["call"]);
        verbs
            .constraints
            .insert("methods".to_owned(), json!(["get", "put"]));
        let mut parent_spec = spec("root", "bob");
        parent_spec.scope = vec![limited, scope("mcp:fs", &["read"]), verbs];
        let parent = root_grant(&fx, &parent_spec);

        let attempt = |scopes: Vec<Scope>| {
            let mut child_spec = spec("child", "carol");
            child_spec.scope = scopes;
            child_token(&fx, &parent, "bob", &child_spec)
        };
        let mut same_prefix = scope("file:/a", &["read"]);
        same_prefix
            .constraints
            .insert("path_prefix".to_owned(), json!("/a"));
        let mut narrower_verbs = scope("api:v1", &["call"]);
        narrower_verbs
            .constraints
            .insert("methods".to_owned(), json!(["get"]));
        assert!(
            attempt(vec![
                same_prefix,
                scope("mcp:fs", &["read"]),
                narrower_verbs
            ])
            .is_ok()
        );

        let mut other_action = scope("file:/a", &["write"]);
        other_action
            .constraints
            .insert("path_prefix".to_owned(), json!("/a"));
        assert_eq!(
            refusal(attempt(vec![other_action])),
            Some(Reason::CapabilityScopeDenied)
        );
        assert_eq!(
            refusal(attempt(vec![scope("file:/b", &["read"])])),
            Some(Reason::CapabilityScopeDenied)
        );
        assert_eq!(
            refusal(attempt(vec![scope("file:/a", &["read"])])),
            Some(Reason::CapabilityScopeDenied),
            "a dropped parent constraint widens the scope"
        );
        let mut wider_prefix = scope("file:/a", &["read"]);
        wider_prefix
            .constraints
            .insert("path_prefix".to_owned(), json!("/b"));
        assert_eq!(
            refusal(attempt(vec![wider_prefix])),
            Some(Reason::CapabilityScopeDenied)
        );
        let mut extra_method = scope("api:v1", &["call"]);
        extra_method
            .constraints
            .insert("methods".to_owned(), json!(["delete"]));
        assert_eq!(
            refusal(attempt(vec![extra_method])),
            Some(Reason::CapabilityScopeDenied)
        );
        let mut direct = scope("mcp:fs", &["read"]);
        direct.mode = McpMode::Direct;
        assert_eq!(
            refusal(attempt(vec![direct])),
            Some(Reason::CapabilityScopeDenied),
            "proxy parent cannot grant direct mode"
        );
    }

    #[test]
    fn req_0007_104_child_cannot_outlive_outspend_or_predate_its_parent() {
        let fx = Fixture::new();
        let parent = root_grant(&fx, &spec("root", "bob"));
        let longer = TokenSpec {
            ttl_ms: 120_000,
            ..spec("c1", "carol")
        };
        let more_uses = TokenSpec {
            max_uses: 2,
            ..spec("c2", "carol")
        };
        let earlier = TokenSpec {
            issued_ms: T0 - 1_000,
            ..spec("c3", "carol")
        };
        for child in [longer, more_uses, earlier] {
            assert_eq!(
                refusal(child_token(&fx, &parent, "bob", &child)),
                Some(Reason::CapabilityChainInvalid)
            );
        }
        assert!(child_token(&fx, &parent, "bob", &spec("c4", "carol")).is_ok());
    }

    #[test]
    fn req_0007_104_depth_is_capped_at_three() {
        let fx = Fixture::new();
        let root = root_grant(&fx, &spec("root", "bob"));
        let d1 = child_grant(&fx, &root, "bob", "carol", "d1");
        let d2 = child_grant(&fx, &d1, "carol", "alice", "d2");
        let d3 = child_grant(&fx, &d2, "alice", "dave", "d3");
        assert_eq!(d3.depth(), 3);
        assert_eq!(d3.ancestors.len(), 3);
        assert_eq!(
            refusal(child_token(&fx, &d3, "dave", &spec("d4", "erin"))),
            Some(Reason::CapabilityChainInvalid)
        );
    }

    #[test]
    fn req_0007_105_no_audience_may_repeat_along_the_chain() {
        let fx = Fixture::new();
        let root = root_grant(&fx, &spec("root", "bob"));
        assert_eq!(
            refusal(child_token(&fx, &root, "bob", &spec("self", "bob"))),
            Some(Reason::CapabilityChainInvalid),
            "self-delegation"
        );
        let d1 = child_grant(&fx, &root, "bob", "carol", "d1");
        assert_eq!(
            refusal(child_token(&fx, &d1, "carol", &spec("back", "bob"))),
            Some(Reason::CapabilityChainInvalid),
            "back to an earlier holder"
        );
        assert_eq!(
            refusal(child_token(&fx, &d1, "carol", &spec("root2", "operator"))),
            Some(Reason::CapabilityChainInvalid),
            "back to the operator"
        );
    }

    #[test]
    fn req_0007_106_signature_is_checked_before_audience() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        let mut object = object_of(&issued.token);
        object.insert("audience".to_owned(), json!("carol"));
        assert_eq!(
            refusal(verify_grant(
                &fx.ctx(),
                &Value::Object(object),
                &agent("carol"),
                TASK
            )),
            Some(Reason::CapabilityInvalid)
        );
    }

    #[test]
    fn req_0007_106_audience_and_task_must_match_the_recipient_and_envelope() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        assert_eq!(
            refusal(verify_grant(
                &fx.ctx(),
                &issued.token,
                &agent("carol"),
                TASK
            )),
            Some(Reason::CapabilityAudienceMismatch)
        );
        assert_eq!(
            refusal(verify_grant(
                &fx.ctx(),
                &issued.token,
                &agent("bob"),
                "task-2"
            )),
            Some(Reason::CapabilityTaskMismatch)
        );
        verify_grant(&fx.ctx(), &issued.token, &agent("bob"), TASK)
            .expect("matching recipient and task verify");
    }

    #[test]
    fn req_0007_106_validity_starts_at_issued_minus_skew_and_ends_at_expiry_without_grace() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        let bob = agent("bob");
        let skew: i64 = 300_000;
        fx.clock.set(T0 - skew - 1);
        assert_eq!(
            refusal(verify_grant(&fx.ctx(), &issued.token, &bob, TASK)),
            Some(Reason::CapabilityNotYetValid)
        );
        fx.clock.set(T0 - skew);
        verify_grant(&fx.ctx(), &issued.token, &bob, TASK)
            .expect("exactly at the skew bound is valid");
        fx.clock.set(T0 + 60_000 - 1);
        verify_grant(&fx.ctx(), &issued.token, &bob, TASK).expect("last valid millisecond");
        fx.clock.set(T0 + 60_000);
        assert_eq!(
            refusal(verify_grant(&fx.ctx(), &issued.token, &bob, TASK)),
            Some(Reason::CapabilityExpired)
        );
    }

    #[test]
    fn req_0007_107_max_uses_defaults_to_one_and_is_at_least_one() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        let mut absent = object_of(&issued.token);
        absent.remove("maxUses");
        let grant = verify_grant(
            &fx.ctx(),
            &resign(&key("operator"), absent),
            &agent("bob"),
            TASK,
        )
        .expect("absent maxUses means one");
        assert_eq!(grant.max_uses(), 1);
        let mut zero = object_of(&issued.token);
        zero.insert("maxUses".to_owned(), json!(0));
        assert_eq!(
            refusal(verify_grant(
                &fx.ctx(),
                &resign(&key("operator"), zero),
                &agent("bob"),
                TASK
            )),
            Some(Reason::CapabilityInvalid)
        );
        let decision = VerifiedDecision::for_test(TASK, ApprovalOptionKind::AllowOnce);
        let zero_uses = TokenSpec {
            max_uses: 0,
            ..spec("zero", "bob")
        };
        assert_eq!(
            refusal(issue_root(
                &fx.ctx(),
                &key("operator"),
                &decision,
                &zero_uses
            )),
            Some(Reason::CapabilityInvalid)
        );
        let zero_ttl = TokenSpec {
            ttl_ms: 0,
            ..spec("zero-ttl", "bob")
        };
        assert_eq!(
            refusal(issue_root(
                &fx.ctx(),
                &key("operator"),
                &decision,
                &zero_ttl
            )),
            Some(Reason::CapabilityInvalid)
        );
    }

    #[test]
    fn req_0007_107_redemptions_count_until_max_uses_then_refuse() {
        let fx = Fixture::new();
        let grant = root_grant(
            &fx,
            &TokenSpec {
                max_uses: 2,
                ..spec("root", "bob")
            },
        );
        let ledger = UseLedger::default();
        ledger.redeem(&fx.ctx(), &grant).expect("first use");
        ledger.redeem(&fx.ctx(), &grant).expect("second use");
        assert_eq!(
            refusal(ledger.redeem(&fx.ctx(), &grant)),
            Some(Reason::CapabilityUsed)
        );
    }

    #[test]
    fn req_0007_108_a_reference_authorizes_only_its_task_and_audience() {
        let fx = Fixture::new();
        let issued = root_token(&fx, &spec("root", "bob"));
        let cache = TokenCache::default();
        let full = json!({
            "tokenId": "root",
            "tokenDigest": issued.digest,
            "delegator": "operator",
            "taskId": TASK,
            "token": issued.token,
        });
        resolve_reference(&fx.ctx(), &cache, &full, &agent("bob"), TASK).expect("bob receives");
        let bare = reference("root", &issued.digest, "operator", TASK);
        assert_eq!(
            refusal(resolve_reference(
                &fx.ctx(),
                &cache,
                &bare,
                &agent("carol"),
                TASK
            )),
            Some(Reason::CapabilityAudienceMismatch)
        );
        let other_task = reference("root", &issued.digest, "operator", "task-2");
        assert_eq!(
            refusal(resolve_reference(
                &fx.ctx(),
                &cache,
                &other_task,
                &agent("bob"),
                "task-2"
            )),
            Some(Reason::CapabilityTaskMismatch)
        );
        let wrong_delegator = reference("root", &issued.digest, "bob", TASK);
        assert_eq!(
            refusal(resolve_reference(
                &fx.ctx(),
                &cache,
                &wrong_delegator,
                &agent("bob"),
                TASK
            )),
            Some(Reason::CapabilityInvalid)
        );
    }

    #[test]
    fn req_0007_109_default_deny_for_unlisted_resources_and_actions() {
        let fx = Fixture::new();
        let grant = root_grant(&fx, &spec("root", "bob"));
        assert!(grant.authorize("file:/a", "read").is_ok());
        assert_eq!(
            refusal(grant.authorize("file:/a", "write")),
            Some(Reason::CapabilityScopeDenied)
        );
        assert_eq!(
            refusal(grant.authorize("file:/b", "read")),
            Some(Reason::CapabilityScopeDenied)
        );
        assert_eq!(
            refusal(grant.authorize("mcp:fs", "read")),
            Some(Reason::CapabilityScopeDenied)
        );
    }

    #[test]
    fn req_0007_110_revoking_any_kid_in_the_chain_invalidates_the_leaf() {
        let good = Fixture::new();
        let root = root_grant(&good, &spec("root", "bob"));
        let issued = child_token(&good, &root, "bob", &spec("c", "carol")).expect("child issues");
        let carol = agent("carol");
        let child = verify_grant(&good.ctx(), &issued.token, &carol, TASK)
            .expect("valid before revocation");

        let bob_revoked = Fixture::trusting("status = \"revoked\"", "");
        let err = verify_grant(&bob_revoked.ctx(), &issued.token, &carol, TASK)
            .expect_err("ancestor kid revoked");
        assert_eq!(err.reason(), Reason::CapabilityRevoked);
        assert!(err.kid().is_some_and(|kid| kid.starts_with("bob:")));
        assert_eq!(
            refusal(child.recheck(&bob_revoked.ctx())),
            Some(Reason::CapabilityRevoked),
            "a grant held in memory sees the revocation too"
        );

        let operator_revoked = Fixture::trusting("", "status = \"revoked\"");
        assert_eq!(
            refusal(verify_grant(
                &operator_revoked.ctx(),
                &issued.token,
                &carol,
                TASK
            )),
            Some(Reason::CapabilityRevoked)
        );
    }

    #[test]
    fn req_0007_111_expiry_is_hard_for_redemption_proxy_calls_and_credentials() {
        let fx = Fixture::new();
        let grant = root_grant(
            &fx,
            &TokenSpec {
                max_uses: 5,
                ..spec("root", "bob")
            },
        );
        let credential = GrantCredential::issue(&grant, T0).expect("issues while valid");
        fx.clock.set(grant.expires_ms());
        let ledger = UseLedger::default();
        assert_eq!(
            refusal(ledger.redeem(&fx.ctx(), &grant)),
            Some(Reason::CapabilityExpired)
        );
        assert_eq!(
            refusal(grant.authorize_mcp(&fx.ctx(), "mcp:fs", "read", true)),
            Some(Reason::CapabilityExpired)
        );
        assert!(!credential.matches(credential.secret(), grant.expires_ms()));
    }

    #[test]
    fn req_0007_112_mcp_scope_defaults_to_proxy_mode() {
        assert_eq!(parse_mode(None).ok(), Some(McpMode::Proxy));
        assert_eq!(
            parse_mode(Some(&json!("MCP_MODE_UNSPECIFIED"))).ok(),
            Some(McpMode::Proxy)
        );
        assert_eq!(parse_mode(Some(&json!(0))).ok(), Some(McpMode::Proxy));
        assert_eq!(
            parse_mode(Some(&json!("MCP_MODE_DIRECT"))).ok(),
            Some(McpMode::Direct)
        );
        assert_eq!(parse_mode(Some(&json!(2))).ok(), Some(McpMode::Direct));
        assert!(parse_mode(Some(&json!(3))).is_err());
        assert!(parse_mode(Some(&json!("MCP_MODE_BOGUS"))).is_err());
        let mcp = scope_json(&scope("mcp:fs", &["read"]));
        assert_eq!(mcp["mcpMode"], json!("MCP_MODE_PROXY"));
        let file = scope_json(&scope("file:/a", &["read"]));
        assert!(file.get("mcpMode").is_none());
    }

    #[test]
    fn req_0007_113_proxies_listen_on_loopback_only() {
        for text in ["127.0.0.1:0", "127.255.0.9:4000", "[::1]:0"] {
            let addr: SocketAddr = text.parse().expect("socket address");
            assert!(check_loopback_bind(&addr).is_ok(), "{text}");
        }
        for text in ["0.0.0.0:0", "192.168.1.10:0", "[::]:0"] {
            let addr: SocketAddr = text.parse().expect("socket address");
            assert!(check_loopback_bind(&addr).is_err(), "{text}");
        }
    }

    #[test]
    fn req_0007_114_proxy_enforces_scope_on_each_call() {
        let fx = Fixture::new();
        let mut parent_spec = spec("root", "bob");
        parent_spec.scope = vec![scope("mcp:fs", &["read"])];
        let grant = root_grant(&fx, &parent_spec);
        assert!(
            grant
                .authorize_mcp(&fx.ctx(), "mcp:fs", "read", true)
                .is_ok()
        );
        assert_eq!(
            refusal(grant.authorize_mcp(&fx.ctx(), "mcp:fs", "write", true)),
            Some(Reason::CapabilityScopeDenied)
        );
        assert_eq!(
            refusal(grant.authorize_mcp(&fx.ctx(), "mcp:db", "read", true)),
            Some(Reason::CapabilityScopeDenied)
        );
    }

    #[test]
    fn req_0007_118_direct_mode_needs_the_recipient_mcp_direct_flag() {
        let mut direct_scope = scope("mcp:fs", &["read"]);
        direct_scope.mode = McpMode::Direct;
        let mut parent_spec = spec("root", "bob");
        parent_spec.scope = vec![direct_scope];

        let plain = Fixture::new();
        let grant = root_grant(&plain, &parent_spec);
        assert_eq!(
            refusal(grant.authorize_mcp(&plain.ctx(), "mcp:fs", "read", true)),
            Some(Reason::CapabilityScopeDenied)
        );

        let direct = Fixture::trusting("mcp_direct = true", "");
        let grant = root_grant(&direct, &parent_spec);
        let allowed = grant
            .authorize_mcp(&direct.ctx(), "mcp:fs", "read", true)
            .expect("direct mode allowed");
        assert_eq!(allowed.mode, McpMode::Direct);
    }

    #[test]
    fn req_0007_119_cross_host_mcp_is_denied_in_both_modes() {
        let fx = Fixture::trusting("mcp_direct = true", "");
        for mode in [McpMode::Proxy, McpMode::Direct] {
            let mut mcp = scope("mcp:fs", &["read"]);
            mcp.mode = mode;
            let mut parent_spec = spec("root", "bob");
            parent_spec.scope = vec![mcp];
            let grant = root_grant(&fx, &parent_spec);
            assert_eq!(
                refusal(grant.authorize_mcp(&fx.ctx(), "mcp:fs", "read", false)),
                Some(Reason::CapabilityScopeDenied)
            );
        }
    }

    #[test]
    fn req_0007_116_grant_credential_is_bound_random_and_redacted() {
        let fx = Fixture::new();
        let grant = root_grant(&fx, &spec("root", "bob"));
        let credential = GrantCredential::issue(&grant, T0).expect("issues while valid");
        assert_eq!(credential.token_id(), "root");
        assert_eq!(credential.audience(), &agent("bob"));
        assert_eq!(credential.expires_ms(), grant.expires_ms());
        assert_eq!(
            credential.secret().len(),
            43,
            "256 bits, base64url without padding"
        );
        assert!(credential.matches(credential.secret(), T0));
        assert!(!credential.matches("guess", T0));
        let other = GrantCredential::issue(&grant, T0).expect("second issue");
        assert_ne!(other.secret(), credential.secret());
        let debug = format!("{credential:?}");
        assert!(!debug.contains(credential.secret()));
        assert!(GrantCredential::issue(&grant, grant.expires_ms()).is_err());
    }
}
