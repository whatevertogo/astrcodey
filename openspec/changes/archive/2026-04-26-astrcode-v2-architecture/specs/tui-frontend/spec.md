## ADDED Requirements

### Requirement: TUI connects to server via stdio
The TUI frontend SHALL launch or connect to an astrcode server process via stdio.
All communication SHALL use the `astrcode-client` library for typed JSON-RPC calls.
The TUI SHALL handle server startup, readiness detection, and graceful shutdown.

#### Scenario: TUI launches server
- **WHEN** user runs `astrcode tui`
- **THEN** the TUI spawns the server binary as a child process
- **THEN** the TUI waits for server ready signal (initialized event on stdout)
- **THEN** the TUI renders the welcome screen

#### Scenario: TUI reconnects to existing server
- **WHEN** user runs `astrcode tui --server-id abc123`
- **THEN** the TUI connects to the already-running server via the run info file
- **THEN** no new server process is spawned

### Requirement: State-render separation
The TUI SHALL separate state management from rendering.
State changes SHALL be driven by Actions dispatched from input handlers and stream events.
Rendering SHALL be a pure function: `fn render(state: &CliState) -> Frame`.
Each render SHALL produce the same output for the same state input.

#### Scenario: Dirty-flag optimized rendering
- **WHEN** a stream event updates the transcript state
- **THEN** only the dirty regions are marked for re-render
- **THEN** unchanged regions reuse the previous frame

### Requirement: Chat surface rendering
The TUI SHALL render conversation as scrollable blocks: markdown messages, code blocks, tool calls, thinking segments.
Streaming content SHALL appear in a preview area before being committed to history.
The TUI SHALL support syntax highlighting in code blocks.

#### Scenario: LLM response streams in
- **WHEN** LLM generates "Here is the fix:\n```rust\nfn main() {}\n```"
- **THEN** the text appears incrementally in the preview area
- **THEN** when message_end arrives, the full block moves to the history area
- **THEN** the code block is syntax-highlighted as Rust

#### Scenario: Scrolling through history
- **WHEN** user presses PageUp in the transcript area
- **THEN** the visible window scrolls up through older messages
- **THEN** new streaming content scrolls the view to the bottom

### Requirement: Multi-session switching in TUI
The TUI SHALL support listing and switching between multiple sessions.
A session list panel SHALL show session_id, working_dir, last_active, and parent_session.

#### Scenario: Switch to different session
- **WHEN** user opens session list and selects session-B
- **THEN** the TUI sends switch_session to the server
- **THEN** the display updates to show session-B's transcript

### Requirement: Theme system
The TUI SHALL support TrueColor, ANSI256, and monochrome terminals.
Theme SHALL gracefully degrade: TrueColor → ANSI256 → ANSI16 → ASCII.
Color schemes SHALL be defined as data, not hardcoded in rendering code.

#### Scenario: Terminal does not support TrueColor
- **WHEN** terminal reports COLORTERM is unset and only 16 colors
- **THEN** the TUI uses the ANSI16 color palette
- **THEN** all UI elements are still distinguishable

### Requirement: Composer with input modes
The TUI SHALL provide a multi-line input composer at the bottom of the screen.
The composer SHALL support: text input, paste, cursor movement, and slash command recognition.

#### Scenario: Slash command completion
- **WHEN** user types "/mode" in the composer
- **THEN** a completion popup shows matching commands: /mode, /model
- **THEN** user can Tab-complete or arrow-select

#### Scenario: Multi-line paste
- **WHEN** user pastes 50 lines of code into the composer
- **THEN** all 50 lines are inserted
- **THEN** the composer expands vertically to show the content
