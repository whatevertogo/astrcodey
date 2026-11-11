//! 工具执行入口的参数解析与诊断；共享修复规则由 core 持有。

pub(crate) fn parse_and_repair_json(
    arguments: &str,
    tool_name: &str,
) -> serde_json::Result<serde_json::Value> {
    match serde_json::from_str(arguments) {
        Ok(value) => Ok(value),
        Err(original_error) => {
            tracing::warn!(tool = %tool_name, arguments_len = arguments.len(), "Failed to parse tool call arguments, attempting repair");
            match astrcode_core::tool::parse_tool_arguments(arguments) {
                Ok(value) => {
                    tracing::debug!(tool = %tool_name, "Successfully repaired tool call arguments");
                    Ok(value)
                },
                Err(_) => {
                    tracing::error!(tool = %tool_name, error = %original_error, "Tool call argument repair failed");
                    Err(original_error)
                },
            }
        },
    }
}
