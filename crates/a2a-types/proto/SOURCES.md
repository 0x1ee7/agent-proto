# Vendored protobuf sources

Fetched 2026-10-26. Only the files below are vendored; `build.rs` compiles them with protox.

| File | Origin | Pin |
|------|--------|-----|
| `a2a.proto` | https://a2a-protocol.org/latest/spec/a2a.proto (identical to GitHub `a2aproject/A2A` main) | main `98279d62316361ba8451eaef300f72b72e169185`; structurally identical to tag v1.0.0 and v1.0.1 (differences are comments only) |
| `google/api/annotations.proto`, `http.proto`, `client.proto`, `field_behavior.proto`, `launch_stage.proto` | https://github.com/googleapis/googleapis | `728c111a933ef4c7fc4cbc35894c559aa9eb2615` |
| `google/rpc/status.proto`, `google/rpc/error_details.proto` | https://github.com/googleapis/googleapis | `728c111a933ef4c7fc4cbc35894c559aa9eb2615` |

`google/protobuf/*.proto` well-known types are bundled in protox and are not vendored.

The A2A-Bus extension protos live in `proto/a2abus/v1/` at the repository root and are compiled from there by `build.rs`.
