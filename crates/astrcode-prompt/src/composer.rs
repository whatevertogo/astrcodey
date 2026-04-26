//! Prompt composer: collect → deduplicate → condition-filter → dependency-sort → render.

use std::collections::HashSet;

use astrcode_core::prompt::*;

pub struct PromptComposer {
    contributors: Vec<Box<dyn PromptContributor>>,
}

impl PromptComposer {
    pub fn new() -> Self { Self { contributors: Vec::new() } }
    pub fn add_contributor(&mut self, c: Box<dyn PromptContributor>) { self.contributors.push(c); }

    pub async fn assemble_impl(&self, context: &PromptContext) -> PromptPlan {
        // 1. Collect all block specs
        let mut all: Vec<BlockSpec> = Vec::new();
        for c in &self.contributors {
            let blocks = c.contribute(context).await;
            all.extend(blocks);
        }

        // 2. Deduplicate by name (first wins)
        let mut seen = HashSet::new();
        all.retain(|b| seen.insert(b.name.clone()));

        // 3. Condition filter
        all.retain(|b| evaluate_conditions(b, context));

        // 4. Dependency-aware topological sort (wave front)
        let sorted = topological_sort(all);

        // 5. Render: group into PromptBlocks
        let blocks: Vec<PromptBlock> = sorted.into_iter().enumerate().map(|(i, b)| PromptBlock {
            name: b.name,
            content: b.content,
            layer: b.layer,
            priority: b.priority + i as u32, // stable insertion order
        }).collect();

        PromptPlan { system_blocks: blocks, prepend_messages: vec![], append_messages: vec![], extra_tools: vec![] }
    }
}

fn evaluate_conditions(block: &BlockSpec, ctx: &PromptContext) -> bool {
    for cond in &block.conditions {
        let value = ctx.custom.get(&cond.variable);
        if value != Some(&cond.equals) { return false; }
    }
    true
}

/// Wave-front topological sort by dependencies.
fn topological_sort(blocks: Vec<BlockSpec>) -> Vec<BlockSpec> {
    let names: HashSet<String> = blocks.iter().map(|b| b.name.clone()).collect();
    let mut sorted = Vec::new();
    let mut remaining = blocks;

    loop {
        let (ready, pending): (Vec<_>, Vec<_>) = remaining.into_iter()
            .partition(|b| b.dependencies.iter().all(|d| names.contains(d) && sorted.iter().any(|s: &BlockSpec| &s.name == d)));

        if ready.is_empty() {
            // Anything left has unsatisfied deps — skip them (diagnostics would go here)
            sorted.extend(pending);
            break;
        }
        sorted.extend(ready);
        remaining = pending;

        // Check if no progress (circular dependency or unresolvable)
        if remaining.is_empty() { break; }
        let made_progress = !remaining.iter().all(|b|
            b.dependencies.iter().any(|d| !names.contains(d) || !sorted.iter().any(|s| &s.name == d))
        );
        if !made_progress { sorted.extend(remaining); break; }
    }
    sorted
}

// ─── PromptProvider trait impl ───────────────────────────────────────────

#[async_trait::async_trait]
impl PromptProvider for PromptComposer {
    async fn assemble(&self, context: PromptContext) -> PromptPlan {
        self.assemble_impl(&context).await
    }
}
