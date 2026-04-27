//! Prompt composer: collect → deduplicate → condition-filter → dependency-sort → render.

use std::collections::{HashMap, HashSet, VecDeque};

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

/// 按依赖关系排序。缺失依赖或循环依赖保留原顺序追加，避免丢 prompt。
fn topological_sort(blocks: Vec<BlockSpec>) -> Vec<BlockSpec> {
    let count = blocks.len();
    let name_to_index: HashMap<String, usize> = blocks
        .iter()
        .enumerate()
        .map(|(index, block)| (block.name.clone(), index))
        .collect();
    let mut dependents = vec![Vec::new(); count];
    let mut indegree = vec![0usize; count];
    let mut has_missing_dependency = vec![false; count];

    for (index, block) in blocks.iter().enumerate() {
        for dependency in &block.dependencies {
            if let Some(&dependency_index) = name_to_index.get(dependency) {
                indegree[index] += 1;
                dependents[dependency_index].push(index);
            } else {
                has_missing_dependency[index] = true;
            }
        }
    }

    let mut queue: VecDeque<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, degree)| {
            (*degree == 0 && !has_missing_dependency[index]).then_some(index)
        })
        .collect();
    let mut emitted = vec![false; count];
    let mut order = Vec::with_capacity(count);

    while let Some(index) = queue.pop_front() {
        if emitted[index] {
            continue;
        }

        emitted[index] = true;
        order.push(index);

        for &dependent_index in &dependents[index] {
            indegree[dependent_index] -= 1;
            if indegree[dependent_index] == 0 && !has_missing_dependency[dependent_index] {
                queue.push_back(dependent_index);
            }
        }
    }

    let mut remaining_blocks: Vec<Option<BlockSpec>> = blocks.into_iter().map(Some).collect();
    let mut sorted = Vec::with_capacity(count);

    for index in order {
        if let Some(block) = remaining_blocks[index].take() {
            sorted.push(block);
        }
    }

    sorted.extend(remaining_blocks.into_iter().flatten());
    sorted
}

// ─── PromptProvider trait impl ───────────────────────────────────────────

#[async_trait::async_trait]
impl PromptProvider for PromptComposer {
    async fn assemble(&self, context: PromptContext) -> PromptPlan {
        self.assemble_impl(&context).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn block(name: &str, dependencies: &[&str]) -> BlockSpec {
        BlockSpec {
            name: name.to_string(),
            content: name.to_string(),
            priority: 0,
            layer: PromptLayer::Stable,
            conditions: vec![],
            dependencies: dependencies
                .iter()
                .map(|dependency| dependency.to_string())
                .collect(),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn topological_sort_keeps_dependencies_before_dependents() {
        let sorted = topological_sort(vec![
            block("consumer", &["provider"]),
            block("provider", &[]),
            block("independent", &[]),
        ]);

        let names = sorted
            .into_iter()
            .map(|block| block.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["provider", "independent", "consumer"]);
    }

    #[test]
    fn topological_sort_keeps_unresolved_blocks_in_original_order() {
        let sorted = topological_sort(vec![
            block("ready", &[]),
            block("missing", &["unknown"]),
            block("cycle-a", &["cycle-b"]),
            block("cycle-b", &["cycle-a"]),
        ]);

        let names = sorted
            .into_iter()
            .map(|block| block.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["ready", "missing", "cycle-a", "cycle-b"]);
    }
}
