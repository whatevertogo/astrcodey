# S5R 3.0 implementation decisions

Status: Phase 0 implemented

**What S5R is.** S5R is the name of the subprocess extension wire protocol: a disk extension runs
as a separate process and talks to the host over stdio using decimal-length-prefixed JSON frames.
The name is the protocol's own (it succeeded the earlier internal s6r line) and is not an acronym
expanded anywhere in this repository; all S5R-related modules and spans should be read as "the
subprocess extension protocol". The host half lives in `astrcode-extensions::s5r_ext`, the shared
wire types in `astrcode-extension-sdk::{s5r, wire}`, and the worker-side runtime in
`astrcode-extension-worker`.

S5R 3.0 was built as a sequence of mergeable changes on top of the extension authoring API. The
production composition root now accepts only S5R 3.0. The transitional S5R 2.0 peer and adapters
were crate-private and have been removed; old on-disk data is rejected at its boundary, not deleted.

## Dependency direction

`astrcode-extension-sdk::wire` owns wire DTOs, operation and error catalogs, framing, and the peer
session state machine. This module does not depend on the SDK authoring surface or host domain
crates. Both the worker runtime and `astrcode-extensions` consume the same wire module; the host
never depends on the worker runtime. `WireErrorCode` is defined once in `wire` and re-exported at
the SDK root for authors. The worker prelude combines wire DTOs with the curated SDK/domain author
types needed by handlers; it does not expose host implementation crates. Bundled extensions may
share additional `astrcode-core` types because they run in-process.

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

The supervisor actor owns lifecycle decisions, the generation gate, permits, and the watch
snapshot. `RetirementSupervisor` owns cancellation and the retirement barrier;
`S5rV3Session` owns the worker process. This split is the Phase 0 ownership boundary rather than an
incomplete single actor: each resource has one writer and lifecycle transitions still pass through
the supervisor. Moving process and retirement resources behind the actor command boundary should
only be considered if a concrete cross-owner race or new atomic operation requires it.

## Protocol and persistence boundaries

The existing decimal-length newline frame and 16 MiB limit are retained. Feature negotiation,
nested invocation, incremental streams, cancellation, and typed custom-event errors ship together
in protocol version 3.0. Versions 1.0 and 2.0 are rejected after the final switch.

Durable session custom events share the session journal order and use independent compare-and-swap
consumer checkpoints. Delivery is at least once, keyed by a stable event id. Old wire and consumer
formats are not migrated or deleted; unsupported data is rejected at its boundary.

Consumer state version 3 is replaced atomically after flushing and syncing the temporary file;
Unix also syncs the containing directory metadata. Quarantine and manual-skip totals are monotonic,
while only the latest 128 audit records are retained and individual error text is bounded to 4 KiB.
This keeps the operational history useful without allowing the control file or each rewrite to grow
without bound.

## Observability

The stable span names are `extension.lifecycle`, `extension.invoke`, `extension.stream`,
`custom_event.delivery`, and `s5r.frame`. Fields include extension, generation, operation,
consumer/session sequence, frame direction and byte count where applicable; payloads and secrets
are never recorded. Invocation permits, retries, quarantine, checkpoint failures and unknown wire
codes are emitted at their owning boundary.

## Post-Phase-0 follow-ups

- A process-wide live custom-event bus is intentionally outside Phase 0; session live and session
  durable delivery cover the current product paths.
- The bundled and worker paths have independent behavior and E2E coverage, but do not yet run one
  shared parity fixture. Add one when shared author-facing behavior expands enough to justify the
  additional fixture ownership.
- TTFT and reload-window p95 thresholds have not been benchmarked on the three CI operating
  systems. This is a performance baseline follow-up, not a correctness acceptance gate.
