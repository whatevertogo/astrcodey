use astrcode_core::llm::LlmMessage;

use super::PromptSectionOrder;

/// 将已渲染的 prompt 拆成 provider 可复用稳定前缀的 system messages。
///
/// Prompt 不是合法的 section 序列时保留原文，避免部分解析导致内容丢失。
pub fn system_messages_from_prompt(text: &str) -> Vec<LlmMessage> {
    if text.trim().is_empty() {
        return Vec::new();
    }

    let Some(sections) = parse_sections(text) else {
        return vec![LlmMessage::system(text)];
    };

    let mut grouped = Vec::<(bool, String)>::new();
    for section in sections {
        let stable = section_group(section.title);
        if let Some((last_stable, content)) = grouped.last_mut() {
            if *last_stable == stable {
                content.push_str("\n\n");
                content.push_str(section.rendered);
                continue;
            }
        }
        grouped.push((stable, section.rendered.to_string()));
    }

    grouped
        .into_iter()
        .map(|(_, content)| LlmMessage::system(content))
        .collect()
}

struct ParsedSection<'a> {
    title: &'a str,
    rendered: &'a str,
}

fn parse_sections(text: &str) -> Option<Vec<ParsedSection<'_>>> {
    let text = text.trim();
    if !text.starts_with('[') {
        return None;
    }

    let mut starts = vec![0];
    starts.extend(text.match_indices("\n\n[").map(|(index, _)| index + 2));
    starts.push(text.len());

    let mut sections = Vec::with_capacity(starts.len().saturating_sub(1));
    for window in starts.windows(2) {
        let rendered = text[window[0]..window[1]].trim();
        let title_end = rendered.find(']')?;
        let title = rendered.get(1..title_end)?.trim();
        let remainder = rendered.get(title_end + 1..)?;
        if title.is_empty() || !remainder.starts_with('\n') || remainder.trim().is_empty() {
            return None;
        }
        sections.push(ParsedSection { title, rendered });
    }

    (!sections.is_empty()).then_some(sections)
}

/// section 是否属于稳定前缀；未知标题按动态处理（保持与 `PromptSectionOrder` 一致）。
fn section_group(title: &str) -> bool {
    PromptSectionOrder::from_title(title).is_some_and(|order| order.is_stable())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_valid_sections_and_preserves_invalid_prompt() {
        let prompt =
            "[Identity]\n  id\n\n[System]\n  system\n\n[Environment]\n  env\n\n[Custom]\n  custom";
        let messages = system_messages_from_prompt(prompt);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].joined_display_text("\n").contains("[System]"));
        assert!(messages[1].joined_display_text("\n").contains("[Custom]"));

        let malformed = "[Identity]\n  id\n\n[Broken\n  must stay";
        let messages = system_messages_from_prompt(malformed);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].joined_display_text("\n"), malformed);

        assert!(system_messages_from_prompt(" \n ").is_empty());
    }
}
