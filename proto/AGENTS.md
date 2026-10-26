# proto

Owns: protobuf definitions for A2A-Bus extension payloads and `A2A_VERSION` (the pinned upstream A2A version).
Provides: schemas for data carried in A2A `metadata` Structs under extension URIs, and any envelope-adjacent messages an RFC declares.
Uses: upstream `a2a.proto` (`lf.a2a.v1`), imported, never copied or modified.

## Rules

- Package `a2abus.v1`. Fields are append-only; never reuse numbers; reserve removed ones.
- Each message names the RFC that owns it in a leading comment.
- Extension payloads MUST also have a canonical JSON form (proto3 JSON mapping) because they travel inside A2A JSON `Struct` values on the HTTP side.
- Do not define a message that duplicates an A2A type.
