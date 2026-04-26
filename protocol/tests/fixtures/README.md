# Protocol Fixture Coverage

This directory records protocol fixture coverage used by conformance tests.

## Plugin v1 Fixtures

- plugin/v1/initialize.json: Plugin handshake initialize payload baseline.
- plugin/v1/invoke.json: Plugin capability invocation payload baseline.
- plugin/v1/event_delta.json: Plugin streaming delta payload baseline.
- plugin/v1/cancel.json: Plugin cancellation payload baseline.
- plugin/v1/result_initialize.json: Successful plugin initialize result baseline.
- plugin/v1/result_error.json: Plugin error result payload baseline.

## Terminal v1 Fixtures

- terminal/v1/snapshot.json: Authoritative terminal hydration snapshot baseline.
- terminal/v1/delta_append_block.json: Terminal block append delta baseline.
- terminal/v1/delta_patch_block.json: Terminal block patch delta baseline.
- terminal/v1/delta_rehydrate_required.json: Cursor 失效后的 rehydrate-required delta baseline.
- terminal/v1/error_envelope.json: Terminal banner/status error envelope baseline.

## Historical History Coverage Note

Historical durable subrun lineage behavior is currently validated by runtime/server regression tests
that seed StorageEvent history directly. This fixture directory tracks wire-format payload samples;
historical lineage degradation semantics are tracked in:

- specs/001-runtime-boundary-refactor/quickstart.md (Scenario C)
- crates/server/src/tests/runtime_routes_tests.rs
- crates/server/src/tests/session_contract_tests.rs
