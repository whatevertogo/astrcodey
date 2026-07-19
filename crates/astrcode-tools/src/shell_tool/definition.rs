use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use astrcode_core::tool::{ExecutionMode, ToolDefinition, ToolOrigin};
use astrcode_support::shell::resolve_shell;

use crate::background_shell::{DEFAULT_STATUS_OUTPUT_MAX_TOKENS, MAX_STATUS_OUTPUT_MAX_TOKENS};

pub(super) fn shell_tool_definition(timeout_secs: u64) -> ToolDefinition {
    static DEFINITIONS: OnceLock<Mutex<HashMap<(String, u64), ToolDefinition>>> = OnceLock::new();
    let shell = resolve_shell();
    let key = (shell.name.clone(), timeout_secs);
    let mut definitions = DEFINITIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(definition) = definitions.get(&key) {
        return definition.clone();
    }

    let definition = ToolDefinition {
        name: "shell".into(),
        description: format!(
            concat!(
                "Executes a {shell} command and returns output. Working directory persists, shell \
                 state does not.\n\n",
                "When NOT to use:\n",
                "- File search or reading files → `grep`/`glob`/`read`\n",
                "- Interactive REPL or debugger sessions → `terminal`\n\n",
                "Tips:\n",
                "- Foreground commands expected to exceed ~30s (builds, large scans, sleeps, \
                 network fetches) → set `runInBackground` to true; do not block the turn waiting \
                 for output.\n",
                "- One-shot foreground timeout up to 600s (default {timeout_secs}s); background \
                 up to 600s.\n",
                "- Long-running commands: `runInBackground` returns immediately with a `shellId`. \
                 Check it later with `shellId` and optional `blockUntilMs`; output is written \
                 under session background-shells/.\n",
                "- Foreground commands still running after ~30s may be auto-moved to background \
                 (same as `runInBackground`) and can be checked with the returned `shellId`.\n",
                "- Poll/wait on a background shell with `shellId` and optional `blockUntilMs` (0 \
                 = status only). Each poll returns only output written since the previous poll.\n",
                "- Background-shell poll output is token-budgeted (default {default_poll_tokens} \
                 tokens, max {max_poll_tokens}); large increments are shown as head+tail previews \
                 with omitted-token counts.\n",
                "- A completed background shell remains queryable through `shellId`; repeated \
                 polls return completed status with only newly written output, usually none. Stop \
                 polling once completed unless you need to inspect the output file.\n",
                "- If a `shellId` is reported as `unknown_stale_shell_id`, it belongs to an older \
                 server process/session or was cleaned up; stop polling that id.\n",
                "- Independent commands may run together; chain dependent ones with `&&`\n",
                "- Set `cwd` instead of using `cd`. Use `stdin` to pipe data.\n",
                "- Non-zero exit codes produce errors.\n",
                "- Foreground output is returned when the command finishes (not streamed live).\n",
                "- Very large output may be persisted to tool-results/; use `read` with \
                 charOffset and maxChars to paginate the saved path (do not re-run the command \
                 for more output).",
            ),
            shell = shell.name,
            timeout_secs = timeout_secs,
            default_poll_tokens = DEFAULT_STATUS_OUTPUT_MAX_TOKENS,
            max_poll_tokens = MAX_STATUS_OUTPUT_MAX_TOKENS,
        ),
        origin: ToolOrigin::Builtin,
        execution_mode: ExecutionMode::Sequential,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute. Use absolute paths; chain dependent commands with &&." },
                "intent": {
                    "type": "string",
                    "description": "Short active-voice reason, shown in audit/UI."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory. Prefer over shell-level cd."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 600,
                    "description": "Seconds. Override default for long commands."
                },
                "stdin": {
                    "type": "string",
                    "description": "Pipe data into stdin (jq, wc, python, etc.)."
                },
                "runInBackground": {
                    "type": "boolean",
                    "description": "Run in background when the command may take more than ~30s (builds, scans, sleep). Returns shellId immediately; use shellId to poll for incremental output and final status."
                },
                "shellId": {
                    "type": "string",
                    "description": "Existing background shell id. Omit command; returns output written since the previous poll plus current status."
                },
                "blockUntilMs": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "With shellId: max ms to wait for new output or completion (0 = immediate status)."
                },
                "maxOutputTokens": {
                    "type": "integer",
                    "minimum": 256,
                    "maximum": MAX_STATUS_OUTPUT_MAX_TOKENS,
                    "description": "With shellId only: token budget for this poll's incremental output preview. Defaults to 10000, matching Codex unified exec. Large increments return a head+tail preview with omittedOutputTokens metadata."
                }
            },
            "required": [],
            "additionalProperties": false
        }),
    };
    definitions.insert(key, definition.clone());
    definition
}
