//! Prompt 贡献的数据结构。
//!
//! [`PromptContribution`] 是 [`PromptContributor`](crate::PromptContributor) 的产出物，
//! 包含一组 block 规格、贡献者变量和额外工具定义。

use std::collections::{HashMap, HashSet};

use astrcode_runtime_contract::tool::ToolDefinition;

use super::BlockSpec;

/// 单个 contributor 对 prompt 组装的贡献。
///
/// 每个 contributor 的 `contribute()` 方法返回此结构，composer 收集所有贡献后
/// 进行去重、依赖解析和渲染。
///
/// # 字段说明
///
/// - `blocks`: 该 contributor 提供的 prompt 块规格列表
/// - `contributor_vars`: 贡献者级别的变量，优先级高于 context 全局变量
/// - `extra_tools`: 该 contributor 引入的额外工具定义（如 skill tool）
#[derive(Default, Clone, Debug)]
pub struct PromptContribution {
    pub blocks: Vec<BlockSpec>,
    pub contributor_vars: HashMap<String, String>,
    pub extra_tools: Vec<ToolDefinition>,
}

/// 将额外工具定义追加到列表中，自动去重。
///
/// 当多个 contributor 都引入同名工具时，仅保留第一个。
/// 这确保 tool list 中不会出现重复的工具定义。
pub fn append_unique_tools(base: &mut Vec<ToolDefinition>, extra: Vec<ToolDefinition>) {
    let mut existing: HashSet<String> = base.iter().map(|tool| tool.name.clone()).collect();

    for tool in extra {
        if existing.insert(tool.name.clone()) {
            base.push(tool);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("{name} tool"),
            parameters: json!({ "type": "object" }),
        }
    }

    #[test]
    fn append_unique_tools_deduplicates_existing_and_extra_items() {
        let mut base = vec![tool("shell"), tool("readFile")];

        append_unique_tools(
            &mut base,
            vec![
                tool("readFile"),
                tool("grep"),
                tool("grep"),
                tool("shell"),
                tool("findFiles"),
            ],
        );

        let names = base
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "shell".to_string(),
                "readFile".to_string(),
                "grep".to_string(),
                "findFiles".to_string()
            ]
        );
    }
}
