//! Provider-specific validation for strict tool schemas.
//!
//! Strict tool use is opt-in at both the tool and provider-profile levels. This module validates
//! only declarations that will actually be sent, so legacy profiles keep their previous behavior.
//!
//! 实现按职责拆分：`compile`(schema 编译为各 provider strict 方言)、`validate`(限额/合规
//! 校验)、`traverse`(schema 遍历原语)。

mod compile;
mod traverse;
mod validate;

use astrcode_core::{llm::LlmError, tool::ToolDefinition};
use compile::{compile_openai_tool_schema, prepare_anthropic_tools};
use validate::validate_strict_tools;

#[derive(Debug, Clone, Copy)]
pub(crate) enum StrictToolProvider {
    OpenAi,
    Anthropic,
}

/// Compile first-party tool schemas to the strict JSON Schema dialect used by the provider.
///
/// Tool definitions keep their natural runtime contract: optional Rust fields remain optional and
/// validation constraints remain available to the executor. Provider strict dialects differ,
/// though. OpenAI requires every object property to appear in `required`, while Anthropic permits
/// optional properties but rejects several validation-only keywords. Compiling at this boundary
/// avoids duplicating provider-specific schemas in every tool implementation.
pub(crate) fn prepare_strict_tools(
    tools: &mut [ToolDefinition],
    supports_strict_tool_use: bool,
    provider: StrictToolProvider,
) -> Result<(), LlmError> {
    if !supports_strict_tool_use {
        return Ok(());
    }

    match provider {
        StrictToolProvider::OpenAi => {
            for tool in tools.iter_mut().filter(|tool| tool.strict) {
                compile_openai_tool_schema(&mut tool.parameters);
            }
        },
        StrictToolProvider::Anthropic => {
            prepare_anthropic_tools(tools)?;
        },
    }
    validate_strict_tools(tools, supports_strict_tool_use, provider)
}

#[cfg(test)]
pub(crate) fn tool(name: &str, parameters: serde_json::Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: String::new(),
        parameters,
        strict: true,
        origin: astrcode_core::tool::ToolOrigin::Bundled,
    }
}
