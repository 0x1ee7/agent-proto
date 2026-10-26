# RFC-0000: RFC Process and Format

| Field | Value |
|---|---|
| Status | Accepted |
| Depends on | none |
| Normative language | BCP 14 (RFC 2119, RFC 8174): MUST, MUST NOT, SHOULD, MAY in capitals |

## 1. Purpose

RFCs are the source of truth for A2A-Bus. Code follows RFCs, not the reverse. An RFC is complete when an independent team could implement the feature from it and a conformance test could be derived from each requirement.

## 2. Lifecycle

`Draft` -> `Proposed` -> `Accepted` -> (`Superseded` | `Withdrawn`)

- Draft: being written; may change freely.
- Proposed: complete and reviewed for consistency with other RFCs.
- Accepted: implementable; changes need a new RFC or a numbered erratum section.
- Only the integrator (see root `AGENTS.md`) changes status.

## 3. File format

Path: `docs/rfcs/NNNN-kebab-title.md`. Numbers are never reused.

Required header table: Status, Depends on, Owner area, Related interop notes (paths under `docs/interop/`).

Required sections, in order:

1. Summary (at most 5 sentences)
2. Motivation and scope (and explicit non-goals)
3. Terminology (only terms not defined in RFC-0001)
4. Specification (normative)
5. Interoperability (A2A v1.0, Hermes, ACP agents: what maps, what is lost, what a stock peer sees)
6. Security considerations (which threats from `docs/requirements.md` section 8 this addresses)
7. Rationale and alternatives considered
8. Conformance (table: requirement ID, test idea, level)
9. Open issues (each with a proposed default)

## 4. Requirement IDs

Every normative statement carries an ID of the form `[NNNN-xxx]` where NNNN is the RFC number and xxx a zero-padded sequence (`[0004-017]`). IDs are stable: deleting a requirement leaves a gap, never a renumber. The conformance table and the future conformance suite reference these IDs.

## 5. Writing rules

- State behavior, not implementation. Rust types appear only in RFC-0001, and in non-normative appendices labeled as sketches (RFC-0009 Backend, RFC-0011 Bus).
- Every field, subject, header and state named in an RFC is defined exactly once; other RFCs reference it, never redefine it.
- Examples are normative-by-illustration: they MUST be consistent with the text and use the same names.
- Cross-RFC references use `RFC-NNNN [NNNN-xxx]`.
- Claims about third-party behavior (A2A, Hermes, ACP, omp, Claude Code, NATS) cite `docs/interop/*.md` and keep its `[UNVERIFIED]` markers.
- Choices not forced by the requirements are made by the RFC author and recorded under Rationale; do not leave a decision open when a sane default exists. Genuinely deferred items go in Open issues with a default.
- Plain ASCII punctuation. No emoji.

## 6. Layout

| Path | Contents |
|---|---|
| `docs/requirements.md` | Requirements, decisions D1-D15, threat model |
| `docs/rfcs/` | RFCs |
| `docs/interop/` | Researched facts about third-party systems (not normative for us) |
| `proto/` | Protobuf definitions for A2A-Bus extensions (normative companions to RFCs) |
| `crates/` | Rust workspace (created when implementation starts; boundaries fixed by RFC-0001) |
