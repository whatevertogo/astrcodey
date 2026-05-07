# Plan: <title>

## Context

**Current state:** <brief description of the existing codebase behavior and relevant code paths>

**Desired state:** <brief description of what should change and why>

## Goal

<One sentence describing the concrete, measurable outcome of this plan.>

## Scope

- <Bullet points of what this plan covers — specific modules, files, or behaviors>

## Non-Goals

- <What is explicitly excluded to prevent scope creep>

## Existing Code to Reuse

- <Functions, modules, or patterns already in the codebase that should be leveraged instead of rewritten>

## Dependencies and Risks

- **Dependencies:** <external crates, APIs, or in-progress work this depends on, or "None">
- **Risks:** <potential breakage points, migration concerns, or performance implications, or "None">

## Implementation Steps

### Step 1: <verb phrase describing the change>

**Files:** `path/to/file.rs`

<What this step does and why>

- <Concrete action, e.g.: Add `fn new_thing()` with signature `fn new_thing(input: &str) -> Result<Output>`>
- <Concrete action, e.g.: Replace call sites in `mod_a` and `mod_b` to use `new_thing`>

### Step 2: <verb phrase describing the change>

**Files:** `path/to/other.rs`

<What this step does and why>

- <Concrete action>

## Verification

1. [ ] `cargo check` — compiles without errors or warnings
2. [ ] `cargo test` — all existing and new tests pass
3. [ ] Manual verification: <describe what to manually test if applicable, or remove this item>

## Assumptions

- <Any assumptions made about the codebase, user requirements, or external systems that could invalidate this plan if wrong>
