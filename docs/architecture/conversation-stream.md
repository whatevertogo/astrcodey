# Conversation stream contract

The conversation UI is a projection pipeline, not an independent source of
session truth:

```text
EventLog / live EventPayload
  → server conversation projection
  → ConversationStreamEnvelopeDto
  → frame-bounded frontend delta buffer
  → pure conversation reducer
  → virtualized render model
```

## Ownership boundaries

- `astrcode-core::EventPayload` contains runtime and durable domain facts.
- `astrcode-server::http::projection` maps those facts at the HTTP boundary.
- `astrcode-protocol` owns snapshot, block, delta, cursor, and envelope wire
  contracts.
- `frontend/src/services/protocol.ts` validates unknown JSON and maps wire DTOs
  into frontend domain types.
- `frontend/src/store/delta` owns lossless batching and pure state reduction.
- React components receive already-projected conversation blocks and do not
  reconstruct backend state.

The shared fixtures in `crates/astrcode-protocol/fixtures` are consumed by both
Rust and frontend contract tests. Any wire change must update the generated
TypeScript bindings, the fixture, and both sides of the contract test.

## Snapshot and cursor invariants

1. A snapshot is the complete render state at its cursor.
2. Every stream envelope carries the latest parent-session durable cursor known
   when the delta is emitted.
3. Agent-child token, reasoning, and tool-output deltas remain scoped to the
   child fan-out. Only phase-boundary signals and compact lineage changes enter
   the parent conversation and update its child-agent projection.
4. A child cursor must never replace the parent cursor.
5. Reconnect replays durable events strictly after the supplied cursor.
6. Invalid, ahead-of-head, or over-limit cursors produce `rehydrateRequired`.
   The old stream closes after that marker so it cannot race the replacement
   snapshot.
7. Once a stream has opened, any unexpected end or read error refreshes the
   snapshot before reconnecting because live-only fragments are not replayable.
8. Applying replay followed by live deltas must converge to the same visible
   state as fetching a fresh snapshot.

## Frontend frame policy

Incoming streaming fragments are accumulated until the next animation frame.
The buffer:

- keeps the newest cursor for the complete frame;
- flushes without dropping data when its delta-count or text-size budget is
  reached;
- hands the drained frame to the reducer, which is the single owner of
  order-preserving delta coalescing;
- performs one Zustand update for each reduced frame;
- runs refresh, rehydrate, and session-navigation effects after pure reduction.

The size limit is a memory bound, not backpressure sent to the server. A limit
hit may cause an additional state update within one display frame, but still
avoids per-token rendering.

## Rendering policy

- The message list virtualizes rows and keeps only the visible window mounted.
- Unchanged conversation block object references are preserved by the reducer.
- Live Markdown renders only a safe committed prefix; the unfinished tail stays
  plain text.
- The Markdown parser, settings UI, and plugins UI are loaded on demand.
- Tool details remain unmounted while collapsed.

## Performance profiling

Run:

```bash
cd frontend
npm run profile:conversation
```

The profile reduces 4,000 streaming fragments against a 10,000-block history
and reports median, p95, and maximum reducer time. It also verifies that the
active block is complete and unchanged history blocks preserve object identity.

Timing is diagnostic rather than a CI pass/fail threshold because developer and
CI hardware differ. Correctness invariants remain part of `npm run check`.

## Change checklist

When changing conversation events or rendering:

1. Decide whether the value is a durable fact, a live-only hint, or a wire-only
   projection.
2. Keep DTO mapping in the server/protocol boundary.
3. Update live projection, replay projection, and snapshot projection together.
4. Extend the shared reducer fixture.
5. Verify reconnect, rehydrate, compact continuation, and child-session routing.
6. Run the frontend profile and compare the reported baseline.
