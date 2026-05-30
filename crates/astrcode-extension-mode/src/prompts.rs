//! Built-in prompt loaders.

pub fn plan_entry_prompt() -> &'static str {
    include_str!("../builtin_prompts/plan_entry.md")
}

pub fn plan_exit_prompt() -> &'static str {
    include_str!("../builtin_prompts/plan_exit.md")
}
