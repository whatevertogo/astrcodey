use astrcode_client::{ConversationSlashActionKindDto, ConversationSlashCandidateDto};

use crate::state::PaletteSelection;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    SubmitPrompt { text: String },
    RunCommand(Command),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    New,
    Resume {
        query: Option<String>,
    },
    Model {
        query: Option<String>,
    },
    Mode {
        query: Option<String>,
    },
    Compact,
    SkillInvoke {
        skill_id: String,
        prompt: Option<String>,
    },
    Unknown {
        raw: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    SwitchSession { session_id: String },
    ReplaceInput { text: String },
    SelectModel { profile_name: String, model: String },
    RunCommand(Command),
}

pub fn classify_input(
    input: String,
    slash_candidates: &[ConversationSlashCandidateDto],
) -> InputAction {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return InputAction::Empty;
    }

    if !trimmed.starts_with('/') {
        return InputAction::SubmitPrompt {
            text: trimmed.to_string(),
        };
    }

    InputAction::RunCommand(parse_command(trimmed, slash_candidates))
}

pub fn fuzzy_contains(query: &str, fields: impl IntoIterator<Item = String>) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    fields
        .into_iter()
        .any(|field| field.to_lowercase().contains(&query))
}

pub fn palette_action(selection: PaletteSelection) -> PaletteAction {
    match selection {
        PaletteSelection::ResumeSession(session_id) => PaletteAction::SwitchSession { session_id },
        PaletteSelection::ModelOption(option) => PaletteAction::SelectModel {
            profile_name: option.profile_name,
            model: option.model,
        },
        PaletteSelection::SlashCandidate(candidate) => match candidate.action_kind {
            ConversationSlashActionKindDto::InsertText => PaletteAction::ReplaceInput {
                text: candidate.action_value,
            },
            ConversationSlashActionKindDto::ExecuteCommand => {
                PaletteAction::RunCommand(parse_command(candidate.action_value.as_str(), &[]))
            },
        },
    }
}

pub fn parse_command(command: &str, slash_candidates: &[ConversationSlashCandidateDto]) -> Command {
    let trimmed = command.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or_default();
    let tail = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    match head {
        "/new" => Command::New,
        "/resume" => Command::Resume { query: tail },
        "/model" => Command::Model { query: tail },
        "/mode" => Command::Mode { query: tail },
        "/compact" => Command::Compact,
        _ if head.starts_with('/') => {
            let skill_id = head.trim_start_matches('/');
            if slash_candidates.iter().any(|candidate| {
                candidate.action_kind == ConversationSlashActionKindDto::InsertText
                    && candidate.action_value == format!("/{skill_id}")
            }) {
                Command::SkillInvoke {
                    skill_id: skill_id.to_string(),
                    prompt: tail,
                }
            } else {
                Command::Unknown {
                    raw: trimmed.to_string(),
                }
            }
        },
        _ => Command::Unknown {
            raw: trimmed.to_string(),
        },
    }
}

pub fn filter_slash_candidates(
    candidates: &[ConversationSlashCandidateDto],
    query: &str,
) -> Vec<ConversationSlashCandidateDto> {
    candidates
        .iter()
        .filter(|candidate| {
            fuzzy_contains(
                query,
                std::iter::once(candidate.id.clone())
                    .chain(std::iter::once(candidate.title.clone()))
                    .chain(std::iter::once(candidate.description.clone()))
                    .chain(candidate.keywords.iter().cloned()),
            )
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_built_in_commands() {
        assert_eq!(parse_command("/new", &[]), Command::New);
        assert_eq!(
            parse_command("/resume terminal", &[]),
            Command::Resume {
                query: Some("terminal".to_string())
            }
        );
        assert_eq!(
            parse_command("/model claude", &[]),
            Command::Model {
                query: Some("claude".to_string())
            }
        );
        assert_eq!(
            parse_command("/mode review", &[]),
            Command::Mode {
                query: Some("review".to_string())
            }
        );
        assert_eq!(
            parse_command(
                "/review 修复失败测试",
                &[ConversationSlashCandidateDto {
                    id: "review".to_string(),
                    title: "Review".to_string(),
                    description: "Review current changes".to_string(),
                    keywords: vec!["review".to_string()],
                    action_kind: ConversationSlashActionKindDto::InsertText,
                    action_value: "/review".to_string(),
                }]
            ),
            Command::SkillInvoke {
                skill_id: "review".to_string(),
                prompt: Some("修复失败测试".to_string())
            }
        );
    }

    #[test]
    fn classifies_plain_prompt_without_command_semantics() {
        assert_eq!(
            classify_input("实现 terminal v1".to_string(), &[]),
            InputAction::SubmitPrompt {
                text: "实现 terminal v1".to_string()
            }
        );
    }

    #[test]
    fn unknown_slash_command_stays_unknown_when_skill_is_not_visible() {
        assert_eq!(
            parse_command("/review 修复失败测试", &[]),
            Command::Unknown {
                raw: "/review 修复失败测试".to_string()
            }
        );
    }
}
