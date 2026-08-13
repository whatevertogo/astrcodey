# S5R 3.0 implementation decisions

Status: implemented; acceptance gaps remain

S5R 3.0 was built as a sequence of mergeable changes on top of the extension authoring API. The
production composition root now accepts only S5R 3.0. The transitional S5R 2.0 peer and adapters
were crate-private and have been removed; old on-disk data is rejected at its boundary, not deleted.

## Dependency direction

`astrcode-extension-contract` owns wire DTOs, operation and error catalogs, framing, and the peer
session state machine. It has no dependency on host domain crates. Both the worker runtime and
`astrcode-extensions` depend on the contract; the host never depends on the worker runtime.
`WireErrorCode` is defined once in the contract; `astrcode-extension-sdk` re-exports it for authors.
The worker prelude combines wire DTOs from the contract with the curated SDK/domain author types
needed by handlers; it does not expose host implementation crates. Bundled extensions may share
additional `astrcode-core` types because they run in-process.

`astrcode-extension-worker` owns only worker-side assembly: `run_stdio`, handler dispatch, and the
remote host transport. Bundled extensions depend on the SDK and use typed calls without wire
encoding.

## Host-first activation

The Host owns discovery, placement, identity, registration, and activation. It sends Initialize
with the expected extension id and the operation catalog derived directly from
`HOST_OPERATION_SPECS`. The Worker returns a typed `InitializeManifest`; there is no generic
metadata, handler, or bidirectional capability catalog. Both peers remain Initialized while the
Host validates the declaration and global registration conflicts. Only a subsequent Host Activate
transitions both peers to Ready and permits runtime traffic.

The operation catalog describes implementation support by Host version, not grants or current
availability. Runtime authority remains the Worker manifest declaration, registration validation,
HostRouter lookup, trusted `InvokeContext` scope check, backend availability, and dispatch.

## Stable Rust policy

The workspace uses edition 2024 with Rust 1.88 as its minimum supported version. The dated nightly
toolchain remains pinned for reproducible formatting, linting, and tests, but repository sources
must not contain `#![feature(...)]`. CI compiles all workspace targets and features on Rust 1.88 on
Linux, macOS, and Windows; this stable lane is authoritative for dependencies too.

## Handler registration

An isolated compiler spike used the shape of `SessionOperations::cancel_turn`: the returned future
borrowed both the backend and an independently borrowed session access value. On stable Rust 1.97,
an `AsyncFn` handler could be registered and erased to a non-`Send` future, but erasing it to
`Pin<Box<dyn Future + Send>>` failed because the stable type system could not express that every
`CallRefFuture` is `Send`.

Host handler registration therefore uses owned `Arc` backends, owned call contexts, and owned
requests. A normal closure returns an `async move` future whose concrete type is bounded by `Send`.
The host registry explicitly associates every `HostOp` marker with one erased group handler; a
const validation and a bidirectional completeness test reject duplicates, omissions, or a group
mismatch before production dispatch can use the table.
Borrowed `AsyncFn` registration may be reconsidered only after the exact registry compiles on the
workspace MSRV without nightly features, a `LocalSet`, call-site boxing, or generated business
logic.

## Runtime authority

Each extension supervisor actor is the sole writer of generation lifecycle state. The generation
gate authorizes calls. The `ArcSwap` handler index only routes to a generation endpoint, while the
`watch` snapshot only reports immutable status and diagnostics; readers do not synchronize these
two snapshots. A route to a closed generation must fail with `extension_draining`.

Reload is stop-old/start-new: close admission, reject queued calls, let admitted calls observe
cancellation within their existing timeout, stop the extension, initialize and validate the
replacement, atomically replace the handler index, then publish the ready snapshot.
Dual-generation reload is not implemented.

The supervisor actor currently owns lifecycle state, the generation gate, permits, and the watch
snapshot. `RetirementSupervisor` still owns task cancellation and `S5rV3Session` still owns the
worker process. Moving those resources behind the actor command boundary remains required before
the stronger single-owner lifecycle claim can be considered complete.

## Protocol and persistence boundaries

The existing decimal-length newline frame and 16 MiB limit are retained. Feature negotiation,
nested invocation, incremental streams, cancellation, and typed custom-event errors ship together
in protocol version 3.0. Versions 1.0 and 2.0 are rejected after the final switch.

Durable session custom events share the session journal order and use independent compare-and-swap
consumer checkpoints. Delivery is at least once, keyed by a stable event id. Old wire and consumer
formats are not migrated or deleted; unsupported data is rejected at its boundary.

## Observability

The stable span names are `extension.lifecycle`, `extension.invoke`, `extension.stream`,
`custom_event.delivery`, and `s5r.frame`. Fields include extension, generation, operation,
consumer/session sequence, frame direction and byte count where applicable; payloads and secrets
are never recorded. Invocation permits, retries, quarantine, checkpoint failures and unknown wire
codes are emitted at their owning boundary.

## Outstanding acceptance

- Global live custom-event delivery is not wired to a process-wide event bus; session live and
  session durable delivery are implemented.
- The bundled and worker paths have independent behavior and E2E coverage, but do not yet run one
  shared parity fixture.
- TTFT and reload-window p95 thresholds have not been benchmarked on the three CI operating
  systems; the spans and counters needed to produce that report are present only in part.
