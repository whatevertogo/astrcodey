# prompt-composition Specification

## Purpose
TBD - created by archiving change astrcode-v2-architecture. Update Purpose after archive.
## Requirements
### Requirement: Contributor-based prompt assembly
The system SHALL use a "Contributor" pattern for prompt assembly.
Each `PromptContributor` SHALL produce named `BlockSpec` entries with priority, layer, conditions, and dependencies.
A `PromptComposer` SHALL collect, deduplicate, filter, sort, render, and validate blocks.

#### Scenario: Multiple contributors produce blocks
- **WHEN** 9 contributors each produce 1-3 block specs
- **THEN** the composer collects all blocks
- **THEN** duplicates are removed by block name
- **THEN** conditional blocks are included/excluded based on context

#### Scenario: Dependency ordering
- **WHEN** block-A depends on block-B
- **THEN** block-B is rendered before block-A
- **THEN** if block-B is filtered out, block-A is also removed

### Requirement: Four-layer caching architecture
The system prompt SHALL be assembled in 4 layers: Stable, SemiStable, Inherited, Dynamic.
Stable layer SHALL be cached indefinitely (never expires).
SemiStable and Inherited layers SHALL have a configurable TTL (default 5 minutes).
Dynamic layer SHALL never be cached (rebuilt every turn).

#### Scenario: Stable layer persists across turns
- **WHEN** the agent runs 10 turns with the same tool set
- **THEN** the Stable layer (tool guides, identity) is generated once and cached
- **THEN** subsequent turns reuse the cached Stable layer

#### Scenario: Dynamic layer rebuilt every turn
- **WHEN** user sends a new message
- **THEN** the Dynamic layer (user message context, recent history) is rebuilt
- **THEN** it is not served from cache

### Requirement: Template variable resolution
The system SHALL support `{{variable}}` template syntax in block content.
Variables SHALL resolve in priority order: block-level → contributor-level → context globals → builtins.
Built-in variables SHALL include: os, date, shell, working_dir, available_tools.

#### Scenario: Template rendered with context
- **WHEN** a block contains "Today is {{date}}, working in {{working_dir}}"
- **THEN** `{{date}}` is resolved from context (e.g., "2026-04-27")
- **THEN** `{{working_dir}}` is resolved from context (e.g., "/home/user/project")

#### Scenario: Undefined variable
- **WHEN** a block contains "{{undefined_var}}"
- **THEN** a diagnostic warning is emitted
- **THEN** the placeholder is replaced with empty string in the output

### Requirement: Built-in contributors
The system SHALL ship with at minimum these built-in contributors: Identity, Environment, AgentsMd, Capability, AgentProfileSummary, SkillSummary, WorkflowExamples, ResponseStyle, SystemInstruction.

#### Scenario: Identity contributor loads user profile
- **WHEN** file `~/.astrcode/IDENTITY.md` exists
- **THEN** its content is included in the system prompt as a Stable block
- **THEN** if the file does not exist, a built-in default identity is used

#### Scenario: Environment contributor injects runtime context
- **WHEN** session is created in /home/user/rust-project on Linux with bash
- **THEN** the environment block includes: os=linux, shell=bash, working_dir=/home/user/rust-project

### Requirement: Prompt diagnostics
The system SHALL track: blocks skipped, dependencies missing, template render failures.
Diagnostics SHALL be accessible for debugging via the diagnostics API.
Warnings SHALL not abort prompt assembly.

#### Scenario: Missing dependency warning
- **WHEN** a block has a dependency that no contributor provides
- **THEN** the block is skipped
- **THEN** a diagnostic entry is recorded: "Block X skipped: missing dependency Y"

