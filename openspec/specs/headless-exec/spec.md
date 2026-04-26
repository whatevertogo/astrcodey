# headless-exec Specification

## Purpose
TBD - created by archiving change astrcode-v2-architecture. Update Purpose after archive.
## Requirements
### Requirement: Single-shot prompt execution
The exec mode SHALL accept a prompt, send it to the server, stream results, and exit.
Results SHALL be output in either human-readable text or JSONL format.
No interactive UI SHALL be required (headless).

#### Scenario: Text output mode
- **WHEN** user runs `astrcode exec "explain main.rs"`
- **THEN** the exec mode sends the prompt to the server
- **THEN** the LLM response text is streamed to stdout
- **THEN** the process exits with code 0

#### Scenario: JSONL output mode
- **WHEN** user runs `astrcode exec --jsonl "explain main.rs"`
- **THEN** each server event is written as a JSONL line to stdout
- **THEN** the final line is a turn_end event with complete metadata

### Requirement: CI/CD integration
Exec mode SHALL support non-interactive execution suitable for CI/CD pipelines.
Exec mode SHALL exit with non-zero code on error.
Exec mode SHALL support a timeout parameter.

#### Scenario: CI pipeline runs exec
- **WHEN** CI runs `astrcode exec --timeout 300 "review this PR diff"`
- **THEN** the exec mode processes for at most 300 seconds
- **THEN** exit code 0 means review completed successfully
- **THEN** exit code 1 means an error occurred

### Requirement: Auto-approve for non-interactive mode
Since exec mode has no user interaction, all UI requests (confirm/select/input) SHALL have configurable auto-response behavior.
Default behavior SHALL be: confirm=false, select=first_option, input=empty.

#### Scenario: Tool execution requires confirmation in exec mode
- **WHEN** agent calls a tool that triggers a ui_request confirm
- **THEN** the exec mode auto-responds according to configured default
- **THEN** if default is "reject", the tool is blocked and LLM is informed

### Requirement: Session lifecycle in exec mode
Exec mode SHALL create a new session for each invocation by default.
Exec mode SHALL support `--session-id` to resume an existing session.
Exec mode SHALL support `--no-save` to discard the session after completion.

#### Scenario: Resume existing session
- **WHEN** user runs `astrcode exec --session-id abc123 "continue the refactor"`
- **THEN** the exec mode resumes session abc123
- **THEN** the prompt is processed in the context of previous conversation

#### Scenario: No-save discards session
- **WHEN** user runs `astrcode exec --no-save "one-off query"`
- **THEN** the session is created, prompt processed, results output
- **THEN** the session is deleted after exit

