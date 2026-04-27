//! Prompt composer: collect → deduplicate → condition-filter → dependency-sort → render.

use std::collections::HashSet;

use astrcode_core::prompt::*;

pub struct PromptComposer {
    contributors: Vec<Box<dyn PromptContributor>>,
}

impl PromptComposer {
    pub fn new() -> Self {
        Self {
            contributors: Vec::new(),
        }
    }

    pub fn add_contributor(&mut self, c: Box<dyn PromptContributor>) {
        self.contributors.push(c);
    }

    pub async fn assemble_impl(&self, context: &PromptContext) -> PromptPlan {
        // 收集所有 contributor 产出的 prompt block。
        let mut all: Vec<BlockSpec> = Vec::new();
        for c in &self.contributors {
            let blocks = c.contribute(context).await;
            all.extend(blocks);
        }

        // 同名 block 保留最先出现的版本，避免扩展重复注入。
        let mut seen = HashSet::new();
        all.retain(|b| seen.insert(b.name.clone()));

        // 按条件过滤只在当前上下文适用的 block。
        all.retain(|b| evaluate_conditions(b, context));

        // 按依赖关系排序，保证前置说明先出现。
        let sorted = topological_sort(all);

        let blocks: Vec<PromptBlock> = sorted
            .into_iter()
            .enumerate()
            .map(|(i, b)| PromptBlock {
                name: b.name,
                content: b.content,
                layer: b.layer,
                priority: b.priority + i as u32, // 保持同优先级 block 的稳定顺序。
            })
            .collect();

        PromptPlan {
            system_blocks: blocks,
            prepend_messages: vec![],
            append_messages: vec![],
            extra_tools: vec![],
        }
    }
}

impl Default for PromptComposer {
    fn default() -> Self {
        Self::new()
    }
}

fn evaluate_conditions(block: &BlockSpec, ctx: &PromptContext) -> bool {
    for cond in &block.conditions {
        let value = ctx.custom.get(&cond.variable);
        if value != Some(&cond.equals) {
            return false;
        }
    }
    true
}

/// 按依赖关系对 block 做波前拓扑排序。
fn topological_sort(blocks: Vec<BlockSpec>) -> Vec<BlockSpec> {
    let names: HashSet<String> = blocks.iter().map(|b| b.name.clone()).collect();
    let mut sorted = Vec::new();
    let mut remaining = blocks;

    loop {
        let (ready, pending): (Vec<_>, Vec<_>) = remaining.into_iter().partition(|b| {
            b.dependencies
                .iter()
                .all(|d| names.contains(d) && sorted.iter().any(|s: &BlockSpec| &s.name == d))
        });

        if ready.is_empty() {
            // 剩余项存在未满足依赖或循环依赖；保留输出，避免整段 prompt 丢失。
            sorted.extend(pending);
            break;
        }
        sorted.extend(ready);
        remaining = pending;

        if remaining.is_empty() {
            break;
        }
        let made_progress = !remaining.iter().all(|b| {
            b.dependencies
                .iter()
                .any(|d| !names.contains(d) || !sorted.iter().any(|s| &s.name == d))
        });
        if !made_progress {
            sorted.extend(remaining);
            break;
        }
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
