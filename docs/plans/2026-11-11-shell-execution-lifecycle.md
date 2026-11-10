# Shell execution lifecycle

## Observed failure

A background test command received the implicit 120-second shell timeout. A long
`shell_poll(blockUntilMs: 600000)` did not extend that deadline: the process tree
was terminated at 120 seconds. The next poll failed with an unknown handle,
because the terminal result had already consumed it. The agent then reran work
and used sleep commands plus log files to work around the perceived handle loss.

## Design

Follow Codex's separation between an execution's lifetime and the time spent
waiting for one tool response. Preserve the existing `shell` / `shell_poll`
interface and Host-owned pipe processes.

- `shell.yieldTimeMs` controls the initial wait (default 10 seconds, range 0–30000).
  `runInBackground: true` remains an immediate return. On yield, promote the
  call-owned process to the session; do not start a replacement execution.
- `shell.timeout` remains an optional 1–600 second execution deadline. An explicit
  `extensions.astrcode-coding.shellTimeoutSecs` remains the default for subsequent
  calls. With neither set, there is no execution deadline. Likewise, omitted
  `process.start.timeout_ms` means no deadline; synchronous `process.spawn`
  retains its existing bounded default.
- Both initial execution and polling collect incremental output until exit or
  their wait deadline, rather than returning on each output notification.
  Output remains bounded and reports dropped bytes.
- Only running results expose a usable `shellId`. Terminal results include the
  final exit/timeout/cancellation state and explicitly tell the model to stop
  polling. Reading a terminal result releases its handle, as before.
- Session/extension cleanup, process-tree ownership, cancellation, permissions,
  and explicit timeout enforcement stay in the Host. Yielding does not remove
  lifecycle ownership. No PTY or shell-state persistence is introduced.

## Validation

Exercise a real supervised process across the former implicit Host deadline,
explicit timeout, incremental output, final output and cancellation. Check tool
orchestration with the registered Coding Extension and real Host. Run formatting,
focused tests, Clippy, then the repository's full feature checks.

## References

- OpenAI Codex prompting guide:
  https://developers.openai.com/cookbook/examples/gpt-5/codex_prompting_guide
- Codex source: `codex-rs/core/src/unified_exec/process_manager.rs`
  (`exec_command`, `write_stdin`, `collect_output_until_deadline`).
