# Code Review — staged changes

## Summary

Files reviewed: 7 (6 modified, 1 new) | New issues: 1 (low) | Perspectives: 4/4

---

## 🔒 Security

*No new security issues introduced by this diff.*

The SEC-003 fix correctly replaces the hardcoded key with an obvious test placeholder.

---

## 📝 Code Quality

*No new code quality issues found.*

All 4 fixes are correct:

- **QUAL-001 (BatchAppender)**: Ownership documentation is clear. `into_inner()` correctly flushes before returning. Pattern matches existing `unwrap_or_else(|e| e.into_inner())` used in the codebase for poisoned mutex recovery.
- **QUAL-002 (SidecarState)**: `Inner { port, child }` behind a single `std::sync::Mutex` eliminates the race. `lock_inner` helper follows the project's poisoned-lock pattern. The spawn happens while holding the lock, which is acceptable because `sidecar_command.spawn()` is a quick OS call, not an async operation.
- **QUAL-003 (duplicate tool call IDs)**: Correctly replaces instead of appends. The `tracing::warn!` is appropriate. The `else` branch preserves the original behavior for non-duplicate calls.
- **strip_think_block**: Depth-counting approach is correct. `saturating_sub` avoids underflow on unmatched closing tags. One minor observation below.

### Low-Confidence Observation

`strip_think_block` at `loop.rs:316-323` has an `if depth == 0 { ... } else { ... }` where both branches do the same character advancement (`text[pos..].chars().next().unwrap(); pos += ch.len_utf8()`). The only difference is that the `depth == 0` branch also pushes to `result`. This is correct but could be simplified by hoisting the shared character-advance logic:

```rust
let ch = text[pos..].chars().next().unwrap();
if depth == 0 {
    result.push(ch);
}
pos += ch.len_utf8();
```

Not a correctness issue, just a minor readability observation.

---

## ✅ Tests

**Run results**: 101 passed, 0 failed, 0 skipped

- `astrcode-core`: 23 passed
- `astrcode-storage`: 21 passed (including 3 new BatchAppender tests)
- `astrcode-server`: 57 passed

All new tests are meaningful:

- `batch_appender_push_and_flush_round_trip`: Verifies seq assignment and replay after flush.
- `batch_appender_flush_empty_is_noop`: Verifies empty flush doesn't corrupt the log.
- `batch_appender_into_inner_flushes_remaining`: Verifies `into_inner` flushes buffered events.

---

## 🏗️ Architecture

*No new architecture issues introduced.*

- `PROJECT_ARCHITECTURE.md` correctly adds `astrcode-log` to Layer 0 and `src-tauri` to Layer 4.
- `AGENTS.md` correctly documents the camelCase exception for LLM tool call argument types.
- `CODE_REVIEW_ISSUES.md` is a new documentation file (the review report itself).

---

## 🚨 Must Fix Before Merge

*None. Diff is clear to merge.*

---

## 🤔 Low-Confidence Observations

- `strip_think_block` `if/else` branches share identical character-advance logic; could be simplified (see above). Not blocking.
