use super::TranscriptCellStatus;

const DEFAULT_THINKING_SNIPPETS: &[&str] = &[
    "先确认当前会话状态，再开始改动。",
    "把变更收敛到最小但完整的一步。",
    "优先复用已有抽象，而不是新增一层。",
    "先压测实现里风险最高的分支。",
    "让这次交互更容易验证和回归。",
    "最终输出保持紧凑且可执行。",
];

const DEFAULT_THINKING_VERBS: &[&str] = &[
    "思考中",
    "整理中",
    "推敲中",
    "拆解中",
    "校准中",
    "交叉检查中",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingPresentationState {
    pub verb: String,
    pub summary: String,
    pub hint: String,
    pub preview: String,
    pub expanded_body: String,
    pub is_playing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingSnippetPool {
    snippets: &'static [&'static str],
    verbs: &'static [&'static str],
}

impl Default for ThinkingSnippetPool {
    fn default() -> Self {
        Self {
            snippets: DEFAULT_THINKING_SNIPPETS,
            verbs: DEFAULT_THINKING_VERBS,
        }
    }
}

impl ThinkingSnippetPool {
    pub fn sequence(&self, seed: u64, count: usize) -> Vec<&'static str> {
        if self.snippets.is_empty() {
            return vec!["thinking"];
        }

        (0..count.max(1))
            .map(|offset| {
                let index = ((seed as usize).wrapping_add(offset * 3)) % self.snippets.len();
                self.snippets[index]
            })
            .collect()
    }

    pub fn sample(&self, seed: u64, frame: u64) -> &'static str {
        if self.snippets.is_empty() {
            return "thinking";
        }
        let index = ((seed as usize).wrapping_add(frame as usize)) % self.snippets.len();
        self.snippets[index]
    }

    pub fn verb(&self, seed: u64, frame: u64) -> &'static str {
        if self.verbs.is_empty() {
            return "思考中";
        }
        let index = ((seed as usize).wrapping_add(frame as usize * 2)) % self.verbs.len();
        self.verbs[index]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThinkingPlaybackDriver {
    pub session_seed: u64,
    pub frame: u64,
}

impl ThinkingPlaybackDriver {
    pub fn sync_session(&mut self, session_id: Option<&str>) {
        self.session_seed = session_id.map(stable_hash).unwrap_or_default();
        self.frame = 0;
    }

    pub fn advance(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    pub fn present(
        &self,
        pool: &ThinkingSnippetPool,
        cell_id: &str,
        raw_body: &str,
        status: TranscriptCellStatus,
        expanded: bool,
    ) -> ThinkingPresentationState {
        let seed = stable_hash(cell_id) ^ self.session_seed;
        let playlist = pool.sequence(seed, 4);
        let scripted_body = playlist.join("\n");
        let summary = first_non_empty_line(raw_body)
            .map(str::to_string)
            .unwrap_or_else(|| playlist[0].to_string());
        let verb = pool.verb(seed, self.frame).to_string();

        let is_streaming = matches!(status, TranscriptCellStatus::Streaming);
        let summary_line = if expanded {
            format!("{verb} · Ctrl+O 收起")
        } else if is_streaming {
            format!("{verb}… · Ctrl+O 展开")
        } else {
            format!("{verb} · Ctrl+O 展开")
        };

        let preview = if is_streaming {
            pool.sample(seed, self.frame).to_string()
        } else {
            summary
        };
        let hint = if is_streaming {
            "提示：让当前任务自然完成，不要中断流式过程。".to_string()
        } else {
            "Ctrl+O 可展开完整思考内容。".to_string()
        };

        let expanded_body = if raw_body.trim().is_empty() {
            scripted_body
        } else {
            raw_body.trim().to_string()
        };

        ThinkingPresentationState {
            verb,
            summary: summary_line,
            hint,
            preview,
            expanded_body,
            is_playing: is_streaming,
        }
    }
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn stable_hash(input: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_is_stable_for_same_seed() {
        let pool = ThinkingSnippetPool::default();
        assert_eq!(pool.sequence(42, 4), pool.sequence(42, 4));
    }

    #[test]
    fn sequence_differs_for_different_seed() {
        let pool = ThinkingSnippetPool::default();
        assert_ne!(pool.sequence(1, 4), pool.sequence(2, 4));
    }

    #[test]
    fn streaming_preview_advances_with_frame() {
        let pool = ThinkingSnippetPool::default();
        let mut driver = ThinkingPlaybackDriver::default();
        driver.sync_session(Some("session-1"));
        let first = driver.present(
            &pool,
            "thinking-1",
            "",
            TranscriptCellStatus::Streaming,
            false,
        );
        driver.advance();
        let second = driver.present(
            &pool,
            "thinking-1",
            "",
            TranscriptCellStatus::Streaming,
            false,
        );
        assert_ne!(first.preview, second.preview);
    }

    #[test]
    fn complete_state_prefers_raw_summary() {
        let pool = ThinkingSnippetPool::default();
        let driver = ThinkingPlaybackDriver::default();
        let presentation = driver.present(
            &pool,
            "thinking-1",
            "先读取代码\n再收敛改动",
            TranscriptCellStatus::Complete,
            false,
        );
        assert_eq!(presentation.preview, "先读取代码");
    }
}
