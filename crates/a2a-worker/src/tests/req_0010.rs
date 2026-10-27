//! RFC-0010 [0010-079]: with signing enabled, an operator decision is applied only when its
//! envelope verifies under an operator-role key and is relayed by `cli` or `tui` ([0007-038]).
//! An unsigned decision is refused with SIGNATURE_MISSING, and a signature under an agent-role
//! key with KEY_ROLE_DENIED. A refused decision changes no task state.

use super::*;
use a2a_core::envelope::ReplyDecodeError;
use a2a_core::errors::error_info;
use a2a_core::headers as hdr;
use a2a_security::clock::ManualClock;
use a2a_security::envelope::sign_envelope;
use a2a_security::identity::Signer;
use a2a_security::identity::{Ed25519Signer, encode_public_key};
use a2a_security::trust::{SCHEMA, TrustStore};

/// Relay identity that carries operator decisions to the worker ([0010-063]).
const RELAY: &str = "cli";
const OPERATOR_SEED: [u8; 32] = [0x11; 32];
const PLANNER_SEED: [u8; 32] = [0x22; 32];

/// A trust file holding `keys` as (signer, role) pairs, all serving the test tenant.
fn trust_of(keys: &[(&Ed25519Signer, &str)]) -> Arc<TrustStore> {
    let mut text = format!("schema = \"{SCHEMA}\"\n");
    for (signer, role) in keys {
        text.push_str(&format!(
            "\n[[key]]\nkid = \"{}\"\nagent = \"{}\"\nrole = \"{role}\"\n\
             tenants = [\"{TENANT}\"]\npublic_key = \"{}\"\n",
            signer.kid(),
            signer.kid().agent().as_str(),
            encode_public_key(&signer.public_key()),
        ));
    }
    Arc::new(TrustStore::parse(&text).unwrap())
}

/// A rig with signing enabled under `trust` and the default `AskAll` approvals.
fn rig_signed(trust: Arc<TrustStore>) -> Rig {
    build_rig(
        ApprovalMode::AskAll,
        3_600_000,
        crate::config::DEFAULT_MAX_WAITING_PER_CONTEXT,
        false,
        Some(trust),
    )
}

/// Sends `decision` as `sender`, signed by `signer` over the envelope the bus carries
/// ([0007-030], [0007-044]). The signature block rides in the request headers.
async fn decide_signed(
    bus: &MemBus,
    sender: &str,
    signer: &Ed25519Signer,
    task: &str,
    context: &str,
    decision: &a2abus::ApprovalDecision,
) -> Result<pb::SendMessageResponse, ReplyDecodeError> {
    let parts = RequestParts {
        tenant: tenant(),
        agent: agent(WORKER),
        msg_id: MessageId::generate(),
        sender: agent(sender),
        origin: None,
        ts_ms: now_ms(bus),
        ttl_ms: TTL_MS,
        traceparent: None,
        tracestate: None,
        extensions: vec![APPROVAL.to_owned()],
        service_params: Vec::new(),
        json: false,
        op: Op::Send,
        body: decision_body(task, context, decision),
    };
    let mut out = envelope::encode_request(parts, envelope::MAX_INLINE_BYTES).unwrap();
    let mut signed = crate::signing::envelope_of(out.subject.as_str(), &out.headers);
    // encode_request wrote A2AB-Ts as the send time. Signing sets the block's own fields, so
    // the block starts empty, as in the client hook ([0007-032]).
    signed.kid = None;
    signed.ts = None;
    signed.nonce = None;
    signed.signature = None;
    let clock = ManualClock::new(i64::try_from(now_ms(bus)).unwrap());
    sign_envelope(signer, &clock, &mut signed, &out.payload).unwrap();
    for (name, value) in [
        (hdr::KID, &signed.kid),
        (hdr::TS, &signed.ts),
        (hdr::NONCE, &signed.nonce),
        (hdr::SIG, &signed.signature),
    ] {
        out.headers.insert(name, value.as_deref().unwrap()).unwrap();
    }
    let reply = bus
        .request(
            &principal(sender),
            &out.subject,
            &out.headers,
            out.payload,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    envelope::decode_reply::<pb::SendMessageResponse>(&reply)
}

/// 0010-079: with signing enabled, a decision signed under the operator key and relayed by
/// `cli` is applied and the turn completes.
#[test]
fn req_0010_079_signed_operator_decision_is_applied() {
    let operator = Ed25519Signer::from_seed(&agent(OPERATOR), &OPERATOR_SEED);
    let mut rig = rig_signed(trust_of(&[(&operator, "operator")]));
    let (bus, mock, store) = (&rig.bus, &rig.mock, &rig.store);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0010-079a").await;
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
        let (_, request) = next_approval(&mut events, &task.id).await;
        let digest = approval_digest(store, &request.request_id);
        decide_signed(
            bus,
            RELAY,
            &operator,
            &task.id,
            &task.context_id,
            &decision(&request, ALLOW_ONCE_ID, &digest),
        )
        .await
        .unwrap();
        wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
    });
}

/// 0010-079: with signing enabled an unsigned decision is refused with SIGNATURE_MISSING and
/// changes nothing; the same decision signed afterwards is applied.
#[test]
fn req_0010_079_unsigned_decision_is_refused_when_signing_is_enabled() {
    let operator = Ed25519Signer::from_seed(&agent(OPERATOR), &OPERATOR_SEED);
    let mut rig = rig_signed(trust_of(&[(&operator, "operator")]));
    let (bus, mock, store) = (&rig.bus, &rig.mock, &rig.store);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0010-079b").await;
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
        let (_, request) = next_approval(&mut events, &task.id).await;
        let digest = approval_digest(store, &request.request_id);
        let verdict = decision(&request, ALLOW_ONCE_ID, &digest);

        let refused = decide(bus, &task.id, &task.context_id, &verdict).await;
        let Err(ReplyDecodeError::Failed(status)) = refused else {
            panic!("unsigned decision must be refused");
        };
        assert_eq!(error_info(&status).unwrap().reason, "SIGNATURE_MISSING");
        assert_eq!(
            state(&get(bus, &task.id).await),
            pb::TaskState::InputRequired as i32
        );

        decide_signed(bus, RELAY, &operator, &task.id, &task.context_id, &verdict)
            .await
            .unwrap();
        wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
    });
}

/// 0010-079: a decision signed under an agent-role key is refused with KEY_ROLE_DENIED, even
/// when sent under the requester's own name, and changes nothing; the operator key still
/// applies.
#[test]
fn req_0010_079_decision_under_an_agent_key_is_refused() {
    let operator = Ed25519Signer::from_seed(&agent(OPERATOR), &OPERATOR_SEED);
    let planner = Ed25519Signer::from_seed(&agent(REQUESTER), &PLANNER_SEED);
    let mut rig = rig_signed(trust_of(&[(&operator, "operator"), (&planner, "agent")]));
    let (bus, mock, store) = (&rig.bus, &rig.mock, &rig.store);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0010-079c").await;
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
        let (_, request) = next_approval(&mut events, &task.id).await;
        let digest = approval_digest(store, &request.request_id);
        let verdict = decision(&request, ALLOW_ONCE_ID, &digest);

        let refused = decide_signed(
            bus,
            REQUESTER,
            &planner,
            &task.id,
            &task.context_id,
            &verdict,
        )
        .await;
        let Err(ReplyDecodeError::Failed(status)) = refused else {
            panic!("decision under an agent-role key must be refused");
        };
        assert_eq!(error_info(&status).unwrap().reason, "KEY_ROLE_DENIED");
        assert_eq!(
            state(&get(bus, &task.id).await),
            pb::TaskState::InputRequired as i32
        );

        decide_signed(bus, RELAY, &operator, &task.id, &task.context_id, &verdict)
            .await
            .unwrap();
        wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
    });
}
