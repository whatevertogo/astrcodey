//! # 外部工具搜索工具
//!
//! 为模型按需展开 MCP / plugin 工具 schema，避免外部工具在 system prompt 中
//! 全量铺开。
//!
//! ## 事实源
//!
//! 搜索索引由组合根从 `CapabilityRouter` 快照注入，
//! `tool_search` 不直接依赖 kernel，不创建平行注册表。

use std::sync::{Arc, RwLock};

use astrcode_core::{CapabilitySpec, Result, SideEffect};
use astrcode_runtime_contract::tool::{
    Tool, ToolCapabilityMetadata, ToolContext, ToolDefinition, ToolExecutionResult,
    ToolPromptMetadata,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

// ---------- 搜索索引 ----------

/// 外部工具搜索索引。
///
/// 轻量索引，仅持有非 builtin 来源的工具（MCP、plugin）。
/// 数据从 `CapabilityRouter` 快照注入，不做独立发现。
#[derive(Clone, Default)]
pub struct ToolSearchIndex {
    specs: Arc<RwLock<Vec<CapabilitySpec>>>,
}

impl ToolSearchIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用 router 快照替换索引内容。
    ///
    /// 仅保留 `source:mcp` 或 `source:plugin` 标签的外部工具。
    pub fn replace_from_specs(&self, specs: Vec<CapabilitySpec>) {
        let external = specs
            .into_iter()
            .filter(|spec| {
                spec.tags
                    .iter()
                    .any(|tag| tag == "source:mcp" || tag == "source:plugin")
            })
            .collect();
        if let Ok(mut guard) = self.specs.write() {
            *guard = external;
        }
    }

    /// 搜索匹配的外部工具。
    pub fn search(&self, query: &str, limit: usize) -> Vec<CapabilitySpec> {
        let Ok(guard) = self.specs.read() else {
            return Vec::new();
        };
        let mut results = guard.clone();
        let normalized_query = query.trim().to_ascii_lowercase();
        if !normalized_query.is_empty() {
            results.retain(|spec| matches_query(spec, &normalized_query));
        }
        results.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        results.truncate(limit);
        results
    }
}

fn matches_query(spec: &CapabilitySpec, query: &str) -> bool {
    spec.name.as_str().to_ascii_lowercase().contains(query)
        || spec.description.to_ascii_lowercase().contains(query)
        || spec
            .tags
            .iter()
            .any(|tag| tag.to_ascii_lowercase().contains(query))
}

// ---------- Tool 实现 ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolSearchArgs {
    #[serde(default)]
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct ToolSearchTool {
    index: Arc<ToolSearchIndex>,
}

impl ToolSearchTool {
    pub fn new(index: Arc<ToolSearchIndex>) -> Self {
        Self { index }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tool_search".to_string(),
            description: "Search MCP and plugin tools, returning their full schema on demand."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Substring query matched against tool name, description, and tags"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Maximum results to return (default 10)"
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn capability_metadata(&self) -> ToolCapabilityMetadata {
        ToolCapabilityMetadata::builtin()
            .side_effect(SideEffect::None)
            .concurrency_safe(true)
            .prompt(
                ToolPromptMetadata::new(
                    "Search external MCP/plugin tools and return their full schema on demand.",
                    "Start with builtin tools. If an external MCP/plugin tool is visible but its \
                     parameters are unclear, call `tool_search` with part of the tool name or its \
                     purpose instead of guessing argument names. `tool_search` returns candidate \
                     tools plus `inputSchema`; read that schema first, then call the matching \
                     concrete tool such as `mcp__...`.",
                )
                .example("Find schema by exact tool name: { \"query\": \"webReader\" }")
                .example("Find schema by purpose: { \"query\": \"github repo structure\" }")
                .example(
                    "List available external tools when you are unsure which one to use: { \
                     \"query\": \"\", \"limit\": 20 }",
                ),
            )
    }

    async fn execute(
        &self,
        tool_call_id: String,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolExecutionResult> {
        let args = serde_json::from_value::<ToolSearchArgs>(input).unwrap_or(ToolSearchArgs {
            query: String::new(),
            limit: None,
        });
        let limit = args.limit.unwrap_or(10).clamp(1, 50);
        let results = self.index.search(&args.query, limit);

        let payload: Vec<_> = results
            .into_iter()
            .map(|spec| {
                let source = spec
                    .tags
                    .iter()
                    .find_map(|tag| tag.strip_prefix("source:"))
                    .unwrap_or("external")
                    .to_string();
                json!({
                    "name": spec.name,
                    "description": spec.description,
                    "source": source,
                    "tags": spec.tags,
                    "inputSchema": spec.input_schema,
                })
            })
            .collect();

        Ok(ToolExecutionResult {
            tool_call_id,
            tool_name: "tool_search".to_string(),
            ok: true,
            output: serde_json::to_string(&payload)
                .expect("tool_search result serialization should not fail"),
            error: None,
            metadata: Some(json!({
                "returned": payload.len(),
                "query": args.query,
            })),
            continuation: None,
            duration_ms: 0,
            truncated: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::CancelToken;
    use astrcode_runtime_contract::tool::ToolContext;
    use serde_json::json;

    use super::*;

    fn tool_context() -> ToolContext {
        ToolContext::new("session".into(), std::env::temp_dir(), CancelToken::new())
    }

    fn external_spec(name: &str, tag: &str) -> CapabilitySpec {
        CapabilitySpec::builder(name, astrcode_core::CapabilityKind::Tool)
            .description(format!("description for {name}"))
            .schema(json!({"type": "object"}), json!({"type": "object"}))
            .tags([tag])
            .build()
            .expect("spec should build")
    }

    fn builtin_spec(name: &str) -> CapabilitySpec {
        CapabilitySpec::builder(name, astrcode_core::CapabilityKind::Tool)
            .description("builtin tool")
            .schema(json!({"type": "object"}), json!({"type": "object"}))
            .tags(["source:builtin"])
            .build()
            .expect("spec should build")
    }

    #[test]
    fn index_only_keeps_external_tools() {
        let index = ToolSearchIndex::new();
        index.replace_from_specs(vec![
            builtin_spec("read_file"),
            external_spec("mcp__demo__search", "source:mcp"),
            external_spec("plugin.search", "source:plugin"),
        ]);
        let results = index.search("", 10);
        let names: Vec<&str> = results.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["mcp__demo__search", "plugin.search"]);
    }

    #[test]
    fn index_search_matches_name_and_description() {
        let index = ToolSearchIndex::new();
        index.replace_from_specs(vec![external_spec("mcp__demo__search", "source:mcp")]);
        assert_eq!(index.search("demo", 10).len(), 1);
        assert_eq!(index.search("description", 10).len(), 1);
        assert_eq!(index.search("nonexistent", 10).len(), 0);
    }

    #[test]
    fn index_respects_limit() {
        let index = ToolSearchIndex::new();
        index.replace_from_specs(vec![
            external_spec("mcp__a", "source:mcp"),
            external_spec("mcp__b", "source:mcp"),
            external_spec("mcp__c", "source:mcp"),
        ]);
        assert_eq!(index.search("", 2).len(), 2);
    }

    #[tokio::test]
    async fn tool_returns_full_schema() {
        let index = Arc::new(ToolSearchIndex::new());
        index.replace_from_specs(vec![external_spec("mcp__demo__search", "source:mcp")]);
        let tool = ToolSearchTool::new(index);

        let result = tool
            .execute(
                "call-1".to_string(),
                json!({"query": "demo"}),
                &tool_context(),
            )
            .await
            .expect("tool_search should succeed");

        assert!(result.ok);
        assert!(result.output.contains("mcp__demo__search"));
        assert!(result.output.contains("inputSchema"));
        assert!(result.output.contains("source:mcp"));
    }
}
