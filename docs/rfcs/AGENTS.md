# docs/rfcs

Owns: the RFCs, one file per RFC.
Provides: normative requirements with IDs `[NNNN-xxx]`; the conformance tables other work is derived from.
Uses: `docs/interop/*.md` for third-party facts; `proto/` for extension schemas.

## Rules

- Follow `0000-process.md` exactly: header table, nine sections in order, requirement IDs, conformance table.
- Ownership of each topic is fixed in RFC-0001 sec 4.5. Do not specify what another RFC owns; reference its IDs.
- Use the shared vocabulary in RFC-0001 sec 4.4 verbatim (subject root, extension URI form, header prefix, ids).
- Every MUST needs an ID and a conformance row. Prefer fewer, sharper requirements.
- Include one worked example per wire-visible feature, consistent with the rules text.
- Always state what a stock A2A peer or Hermes sees; if it would break, say how the design avoids it.
- Decide, do not defer: if the requirements leave a choice, choose a sane default and justify it under Rationale.
- Leave Status as `Draft`; the integrator promotes it.
