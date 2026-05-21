//! 斜杠命令 — 解析、派发、命令列表收集。

use std::collections::HashSet;

use astrcode_core::{
    config::ModelSelection,
    extension::{ExtensionCommandResult, ExtensionError},
};

use super::{CommandHandler, HandlerError, PromptSubmission};

/// 解析后的斜杠命令。
pub(in crate::handler) struct ParsedSlashCommand {
    pub name: String,
    pub arguments: String,
}

/// 解析斜杠命令，如 "/compact arg1 arg2"。
/// 返回 None 表示不是斜杠命令。
pub(in crate::handler) fn parse_slash_command(text: &str) -> Option<ParsedSlashCommand> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix('/')?.trim();
    if body.is_empty() {
        return Some(ParsedSlashCommand {
            name: String::new(),
            arguments: String::new(),
        });
    }

    // 分割命令名和参数
    let (name, arguments) = body
        .split_once(char::is_whitespace)
        .map(|(name, arguments)| (name, arguments.trim()))
        .unwrap_or((body, ""));

    Some(ParsedSlashCommand {
        name: name.to_ascii_lowercase(),
        arguments: arguments.to_string(),
    })
}

/// 将 HandlerError 映射为错误码。
pub(in crate::handler) fn command_error_code(error: &HandlerError) -> i32 {
    match error {
        HandlerError::UnknownCommand(_) => 40402,
        _ => -32603,
    }
}

/// 判断命令来源：skill 或 extension。
fn command_source(extension_id: &str) -> &'static str {
    if extension_id == "astrcode-skill" {
        "skill"
    } else {
        "extension"
    }
}

/// 添加命令信息到列表，去重。
fn push_command_info(
    infos: &mut Vec<astrcode_protocol::events::ExtensionCommandInfo>,
    seen: &mut HashSet<String>,
    name: &str,
    description: &str,
    needs_argument: bool,
    source: &str,
) {
    if !seen.insert(name.to_string()) {
        return;
    }
    infos.push(astrcode_protocol::events::ExtensionCommandInfo {
        name: name.into(),
        description: description.into(),
        needs_argument,
        source: source.into(),
    });
}

impl CommandHandler {
    /// 执行指定会话的斜杠命令。
    pub(in crate::handler) async fn execute_slash_command_for_session(
        &mut self,
        sid: astrcode_core::types::SessionId,
        command: ParsedSlashCommand,
        visible_text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        // 内置 /compact 命令
        if command.name == "compact" {
            return match self.compact_session(&sid).await? {
                super::ManualCompactOutcome::Compacted { .. } => Ok(PromptSubmission::Handled {
                    message: "compact accepted".into(),
                }),
                super::ManualCompactOutcome::Skipped { message } => {
                    Ok(PromptSubmission::Handled { message })
                },
            };
        }

        // 内置 /model 命令
        if command.name == "model" {
            self.start_model_selection().await?;
            return Ok(PromptSubmission::Handled {
                message: "model selection started".into(),
            });
        }

        // 扩展命令分发
        let state = self.runtime.session_manager.read_model(&sid).await?;
        let cmd_ctx = astrcode_core::extension::CommandContext {
            session_id: sid.to_string(),
            working_dir: state.working_dir.clone(),
            model: ModelSelection::simple(
                self.runtime
                    .config_manager
                    .read_effective()
                    .llm
                    .model_id
                    .clone(),
            ),
        };

        match self
            .runtime
            .extension_runner
            .dispatch_command_typed(
                &command.name,
                &command.arguments,
                &state.working_dir,
                &cmd_ctx,
            )
            .await
        {
            // 显示结果到客户端
            Ok(ExtensionCommandResult::Display { content, is_error }) => {
                // mode 命令成功时，同步推送状态栏更新。
                if command.name == "mode" && !is_error {
                    if let Some(mode) = content
                        .strip_prefix("Switched to ")
                        .and_then(|s| s.strip_suffix(" mode"))
                        .or_else(|| {
                            content
                                .strip_prefix("Already in ")
                                .and_then(|s| s.strip_suffix(" mode"))
                        })
                    {
                        self.event_bus.send_notification(
                            astrcode_protocol::events::ClientNotification::StatusItemUpdate {
                                id: "mode".into(),
                                text: mode.to_string(),
                            },
                        );
                    }
                }
                self.event_bus.send_notification(
                    astrcode_protocol::events::ClientNotification::ExtensionCommandResult {
                        command_name: command.name,
                        content,
                        is_error,
                    },
                );
                Ok(PromptSubmission::Handled {
                    message: "command handled".into(),
                })
            },
            // 已处理，返回消息
            Ok(ExtensionCommandResult::Handled { message }) => {
                Ok(PromptSubmission::Handled { message })
            },
            // 启动新 Turn，skill 内容直接作为 user_text
            Ok(ExtensionCommandResult::StartTurn { instructions }) => {
                let user_text = if instructions.trim().is_empty() {
                    visible_text.clone()
                } else {
                    instructions
                };
                self.start_turn_for_session(sid, visible_text, user_text, None)
                    .await
                    .map(|turn_id| PromptSubmission::Accepted { turn_id })
            },
            // 命令不存在
            Err(ExtensionError::NotFound(name)) => Err(HandlerError::UnknownCommand(
                name.trim_start_matches('/').to_string(),
            )),
            Err(error) => Err(HandlerError::Other(format!("Command error: {error}"))),
        }
    }

    /// 收集指定工作目录的所有可用命令（内置 + 扩展）。
    pub(in crate::handler) async fn command_infos_for_working_dir(
        &self,
        working_dir: &str,
    ) -> Vec<astrcode_protocol::events::ExtensionCommandInfo> {
        let mut infos = Vec::new();
        let mut seen = HashSet::new();

        // 内置 compact 命令
        push_command_info(
            &mut infos,
            &mut seen,
            "compact",
            "Compact the current session context",
            false,
            "builtin",
        );

        // 内置 model 命令
        push_command_info(
            &mut infos,
            &mut seen,
            "model",
            "Select the active AI model",
            false,
            "builtin",
        );

        // 扩展命令：extension 优先于 skill
        let mut extension_commands = self
            .runtime
            .extension_runner
            .collect_commands_for_typed(working_dir)
            .await;
        extension_commands.sort_by_key(|(ext_id, _, _)| match command_source(ext_id.as_str()) {
            "extension" => 0,
            "skill" => 1,
            _ => 2,
        });

        for (ext_id, cmd, _handler) in extension_commands {
            let source = command_source(&ext_id);
            if !seen.insert(cmd.name.clone()) {
                tracing::warn!(
                    command = %cmd.name,
                    source,
                    extension_id = %ext_id,
                    "slash command ignored because a higher priority command already exists"
                );
                continue;
            }
            infos.push(astrcode_protocol::events::ExtensionCommandInfo {
                name: cmd.name,
                description: cmd.description,
                needs_argument: cmd.args_schema.is_some(),
                source: source.into(),
            });
        }

        infos
    }
}
