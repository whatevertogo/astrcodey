//! Kimi K2.5 提供商。
//!
//! Kimi 在 `delta.content` 中以专有内联令牌嵌入 thinking 和工具调用参数：
//! `<think-block>...</think-block>` 包含推理，
//! `<|tool_calls_section_begin|>...<|tool_calls_section_end|>` 包含调用参数。
//!
//! 此模块使用 `KimiAccumulator` 包装标准 `StandardAccumulator`，
//! 在内容文本流上做预解析，提取 thinking 和工具调用，剥离令牌后发射干净文本。
//! 其余 HTTP/SSE 基础设施完全复用 `OpenAiProvider<A>`。

use astrcode_core::llm::*;
use tokio::sync::mpsc;

use super::openai::{ChatAccumulator, OpenAiProvider, StandardAccumulator};

// ─── Kimi 内联令牌常量 ─────────────────────────────────────────────────

const THINK_OPEN: &str = "<think-block>";
const THINK_CLOSE: &str = "</think-block>";
const TOOL_SECTION_OPEN: &str = "<|tool_calls_section_begin|>";
const TOOL_SECTION_CLOSE: &str = "<|tool_calls_section_end|>";
const TOOL_CALL_BEGIN: &str = "<|tool_call_begin|>";
const TOOL_CALL_END: &str = "<|tool_call_end|>";
const TOOL_ARG_BEGIN: &str = "<|tool_call_argument_begin|>";

// ─── KimiParser ─────────────────────────────────────────────────────────

/// Kimi 文本流内联解析器。
///
/// 缓冲 `delta.content` 文本，提取 `<think-block>` 和
/// `<|tool_calls_section_begin|>` 块，转为标准的 `ThinkingDelta` /
/// `ToolCallStart` / `ToolCallDelta` 事件，并发射剥离后的干净
/// `ContentDelta`。
pub(crate) struct KimiParser {
    buf: String,
    tool_count: u32,
}

impl KimiParser {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            tool_count: 0,
        }
    }

    pub fn has_inline_tools(&self) -> bool {
        self.tool_count > 0
    }

    /// 喂入一段文本 delta，触发解析与事件发射。
    pub fn feed(&mut self, delta: &str, tx: &mpsc::UnboundedSender<LlmEvent>) {
        self.buf.push_str(delta);
        self.flush(tx, false);
    }

    /// 强行清空缓冲区（Done 前调用）。
    pub fn flush_force(&mut self, tx: &mpsc::UnboundedSender<LlmEvent>) {
        self.flush(tx, true);
    }

    fn flush(&mut self, tx: &mpsc::UnboundedSender<LlmEvent>, force: bool) {
        if self.buf.is_empty() {
            return;
        }
        loop {
            let prev = self.buf.len();

            // 1. 提取 <think-block>...</think-block>
            while let (Some(start), Some(end)) =
                (self.buf.find(THINK_OPEN), self.buf.find(THINK_CLOSE))
            {
                if start < end {
                    let body = self.buf[start + THINK_OPEN.len()..end].trim().to_string();
                    self.emit_tool_args(&body, tx);
                    let _ = tx.send(LlmEvent::ThinkingDelta { delta: body });
                    self.buf.replace_range(start..end + THINK_CLOSE.len(), "");
                } else {
                    break;
                }
            }

            // 2. 提取 <|tool_calls_section_begin|>...<|tool_calls_section_end|>
            while let (Some(start), Some(end)) = (
                self.buf.find(TOOL_SECTION_OPEN),
                self.buf.find(TOOL_SECTION_CLOSE),
            ) {
                if start < end {
                    let section_start = start + TOOL_SECTION_OPEN.len();
                    let section = self.buf[section_start..end].to_string();
                    self.emit_tool_args(&section, tx);
                    self.buf
                        .replace_range(start..end + TOOL_SECTION_CLOSE.len(), "");
                } else {
                    break;
                }
            }

            // 3. 发射标签之前的干净文本
            let emit_end = if force {
                self.buf.len()
            } else {
                safe_emit_end(&self.buf)
            };
            if emit_end > 0 {
                let clean = self.buf[..emit_end].to_string();
                self.buf.replace_range(..emit_end, "");
                let _ = tx.send(LlmEvent::ContentDelta { delta: clean });
            }

            if self.buf.len() == prev {
                break;
            }
        }
        if force {
            let rest = std::mem::take(&mut self.buf);
            if !rest.is_empty() {
                let _ = tx.send(LlmEvent::ContentDelta { delta: rest });
            }
        }
    }

    fn emit_tool_args(&mut self, text: &str, tx: &mpsc::UnboundedSender<LlmEvent>) {
        let mut rest = text;
        while let Some(begin) = rest.find(TOOL_CALL_BEGIN) {
            let after_begin = &rest[begin + TOOL_CALL_BEGIN.len()..];
            let Some(end) = after_begin.find(TOOL_CALL_END) else {
                break;
            };
            let call_text = &after_begin[..end];
            if let Some(arg_pos) = call_text.find(TOOL_ARG_BEGIN) {
                let name_part = call_text[..arg_pos].trim();
                let name = name_part
                    .rsplit_once(':')
                    .map(|(n, _)| n.to_string())
                    .unwrap_or_else(|| name_part.to_string());
                let args = call_text[arg_pos + TOOL_ARG_BEGIN.len()..]
                    .trim()
                    .to_string();
                let call_id = format!("kimi-{}-{}", name, self.tool_count);
                self.tool_count += 1;
                let _ = tx.send(LlmEvent::ToolCallStart {
                    call_id: call_id.clone(),
                    name,
                    arguments: args.clone(),
                });
                let _ = tx.send(LlmEvent::ToolCallDelta {
                    call_id,
                    delta: args,
                });
            }
            rest = &after_begin[end + TOOL_CALL_END.len()..];
        }
    }
}

// ─── KimiAccumulator ────────────────────────────────────────────────────

/// 包装 `StandardAccumulator`，在内容文本上做 Kimi 内联解析。
pub struct KimiAccumulator {
    inner: StandardAccumulator,
    parser: KimiParser,
}

impl Default for KimiAccumulator {
    fn default() -> Self {
        Self {
            inner: StandardAccumulator::default(),
            parser: KimiParser::new(),
        }
    }
}

impl ChatAccumulator for KimiAccumulator {
    fn ingest_chat_completion(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        if let Some(choices) = event["choices"].as_array() {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta["content"].as_str() {
                        self.parser.feed(content, tx);
                    }
                    if let Some(reasoning) = delta["reasoning_content"].as_str() {
                        let _ = tx.send(LlmEvent::ThinkingDelta {
                            delta: reasoning.to_string(),
                        });
                    }
                    // 抑制 Kimi 的空参数标准 tool_calls
                    if let Some(tool_calls) = delta["tool_calls"].as_array() {
                        let has_inline = self.parser.has_inline_tools();
                        for tc in tool_calls {
                            if has_inline && is_empty_tool_call(tc) {
                                continue;
                            }
                            let synthetic = serde_json::json!({
                                "choices": [{"delta": {"tool_calls": [tc]}}]
                            });
                            self.inner.ingest_chat_completion(&synthetic, tx);
                        }
                    }
                }
                if let Some(finish) = choice["finish_reason"].as_str() {
                    if !self.inner.done_sent() {
                        self.parser.flush_force(tx);
                        self.inner.mark_done();
                        let _ = tx.send(LlmEvent::Done {
                            finish_reason: finish.to_string(),
                        });
                    }
                }
            }
        }
    }

    fn ingest_responses(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        self.inner.ingest_responses(event, tx);
    }

    fn done_sent(&self) -> bool {
        self.inner.done_sent()
    }

    fn mark_done(&mut self) {
        self.inner.mark_done();
    }
}

// ─── 辅助函数 ──────────────────────────────────────────────────────────

fn find_first_kimi_tag(buf: &str) -> usize {
    let tags: &[&str] = &[
        THINK_OPEN,
        THINK_CLOSE,
        TOOL_SECTION_OPEN,
        TOOL_SECTION_CLOSE,
        TOOL_CALL_BEGIN,
        TOOL_ARG_BEGIN,
    ];
    tags.iter()
        .filter_map(|&tag| buf.find(tag))
        .min()
        .unwrap_or(buf.len())
}

fn safe_emit_end(buf: &str) -> usize {
    let tag_end = find_first_kimi_tag(buf);
    let known: &[&str] = &[
        THINK_OPEN,
        THINK_CLOSE,
        TOOL_SECTION_OPEN,
        TOOL_SECTION_CLOSE,
        TOOL_CALL_BEGIN,
        TOOL_ARG_BEGIN,
        TOOL_CALL_END,
    ];
    if let Some(lt_pos) = buf[..tag_end].rfind('<') {
        let rest = &buf[lt_pos..];
        if known.iter().any(|&tag| tag.starts_with(rest)) {
            return lt_pos;
        }
    }
    tag_end
}

fn is_empty_tool_call(tc: &serde_json::Value) -> bool {
    let func = match tc.get("function") {
        Some(f) => f,
        None => return true,
    };
    match func.get("arguments") {
        Some(a) => a.as_str().is_none_or(|s| s.is_empty()),
        None => true,
    }
}

// ─── 公共类型别名 ──────────────────────────────────────────────────────

/// Kimi 提供商：标准 OpenAI HTTP 层 + Kimi 内联解析累积器。
pub type KimiProvider = OpenAiProvider<KimiAccumulator>;

#[cfg(test)]
mod tests {
    use astrcode_core::llm::*;
    use tokio::sync::mpsc;

    use super::*;

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<LlmEvent>) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn parses_think_block_from_content() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = KimiAccumulator::default();

        let event = serde_json::json!({
            "choices": [{"delta": {"content": "<think-block>\n推理内容\n</think-block>\n你好"}}]
        });
        acc.ingest_chat_completion(&event, &tx);

        let events = drain_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LlmEvent::ThinkingDelta { delta } if delta == "推理内容")),
            "should emit ThinkingDelta"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LlmEvent::ContentDelta { delta } if delta.contains("你好"))),
            "should emit clean text without think-block tags"
        );
    }

    #[test]
    fn parses_inline_tool_calls() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = KimiAccumulator::default();

        let event = serde_json::json!({
            "choices": [{"delta": {"content": "<think-block>\n<|tool_calls_section_begin|><|tool_call_begin|>find:7<|tool_call_argument_begin|>{\"pattern\":\"*.rs\"}<|tool_call_end|><|tool_calls_section_end|>\n</think-block>\n搜索中..."}}]
        });
        acc.ingest_chat_completion(&event, &tx);

        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(
                |e| matches!(e, LlmEvent::ToolCallStart { name, arguments, .. }
                    if name == "find" && arguments == "{\"pattern\":\"*.rs\"}"
                )
            ),
            "should emit ToolCallStart with parsed arguments"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LlmEvent::ToolCallDelta { delta, .. }
                    if delta == "{\"pattern\":\"*.rs\"}"
                )),
            "should emit ToolCallDelta with parsed arguments"
        );
    }

    #[test]
    fn suppresses_empty_standard_tool_calls() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = KimiAccumulator::default();

        // 先喂内联工具调用（带参数）
        let content_event = serde_json::json!({
            "choices": [{"delta": {"content": "<|tool_calls_section_begin|><|tool_call_begin|>find:0<|tool_call_argument_begin|>{\"p\":1}<|tool_call_end|><|tool_calls_section_end|>"}}]
        });
        acc.ingest_chat_completion(&content_event, &tx);

        // 再喂标准空参数 tool_call（应被抑制）
        let tc_event = serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "find"}}]}}]
        });
        acc.ingest_chat_completion(&tc_event, &tx);

        // 统计 find 的 ToolCallStart
        let events = drain_events(&mut rx);
        let find_starts: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, LlmEvent::ToolCallStart { name, .. } if name == "find"))
            .collect();
        assert_eq!(
            find_starts.len(),
            1,
            "should only have one ToolCallStart for find (the inline one)"
        );
    }
}
