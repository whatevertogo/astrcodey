//! SSE 行缓冲器与 UTF-8 流式解码器。
//!
//! 这两个组件是所有 HTTP 流式 LLM provider 的基础设施：
//! - [`SseLineReader`]：跨 TCP chunk 拼接完整的 SSE 行。
//! - [`Utf8StreamDecoder`]：跨 chunk 处理多字节 UTF-8 边界和坏字节。

// ─── SseLineReader ───────────────────────────────────────────────────────

/// SSE 行缓冲器。
///
/// TCP 是字节流协议，一个完整的 SSE 行可能被分成多个 chunk。
/// 本结构在内部拼接不完整的行，每遇到换行符时产出一条完整行。
pub struct SseLineReader {
    buffer: String,
}

impl SseLineReader {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
        }
    }

    /// 追加一个文本 chunk，返回本次产出的完整行列表。
    pub fn push_chunk(&mut self, text: &str) -> Vec<String> {
        self.buffer.push_str(text);
        let mut lines = Vec::new();
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].trim_end_matches('\r').to_string();
            self.buffer.drain(..=pos);
            lines.push(line);
        }
        lines
    }

    /// 流结束后刷新缓冲区，返回残留的最后一行（如果有）。
    pub fn flush(&mut self) -> Option<String> {
        let remaining = std::mem::take(&mut self.buffer);
        let trimmed = remaining.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }
}

impl Default for SseLineReader {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Utf8StreamDecoder ──────────────────────────────────────────────────

/// 流式 UTF-8 解码器，处理分块字节流中的多字节字符边界和坏字节。
///
/// - `push()` 追加新字节块并返回已确认完整的 UTF-8 文本
/// - `finish()` 在流结束时刷新尾部缓冲，对坏字节做容错恢复（替换为 U+FFFD）
pub struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// 追加一个新的字节块，并返回当前已经确认完整的 UTF-8 文本。
    pub fn push(&mut self, chunk: &[u8]) -> Option<String> {
        if chunk.is_empty() {
            return None;
        }
        self.pending.extend_from_slice(chunk);
        self.decode_available()
    }

    /// 在流结束时刷新尾部缓冲。
    ///
    /// 如果尾部是损坏/不完整 UTF-8，替换为 U+FFFD 并继续。
    pub fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }

        let mut decoded = String::new();

        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    decoded.push_str(text);
                    self.pending.clear();
                    break;
                },
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if valid_up_to > 0 {
                        decoded.push_str(valid_utf8_prefix(&self.pending[..valid_up_to]));
                    }

                    if let Some(invalid_len) = error.error_len() {
                        tracing::warn!(
                            "stream decoder recovered invalid utf-8 sequence at stream end: \
                             valid_up_to={}, invalid_len={}, bytes={}",
                            valid_up_to,
                            invalid_len,
                            debug_utf8_bytes(&self.pending, valid_up_to, Some(invalid_len))
                        );
                        decoded.push(char::REPLACEMENT_CHARACTER);
                        self.pending.drain(..valid_up_to + invalid_len);
                        if self.pending.is_empty() {
                            break;
                        }
                    } else {
                        tracing::warn!(
                            "stream decoder recovered incomplete utf-8 tail at stream end: \
                             valid_up_to={}, bytes={}",
                            valid_up_to,
                            debug_utf8_bytes(&self.pending, valid_up_to, None)
                        );
                        decoded.push(char::REPLACEMENT_CHARACTER);
                        self.pending.clear();
                        break;
                    }
                },
            }
        }

        (!decoded.is_empty()).then_some(decoded)
    }

    fn decode_available(&mut self) -> Option<String> {
        let mut decoded = String::new();

        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    decoded.push_str(text);
                    self.pending.clear();
                    return (!decoded.is_empty()).then_some(decoded);
                },
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if valid_up_to > 0 {
                        decoded.push_str(valid_utf8_prefix(&self.pending[..valid_up_to]));
                    }

                    let Some(invalid_len) = error.error_len() else {
                        if decoded.is_empty() {
                            return None;
                        }
                        let tail = self.pending.split_off(valid_up_to);
                        self.pending = tail;
                        return Some(decoded);
                    };

                    tracing::warn!(
                        "stream decoder recovered invalid utf-8 sequence: valid_up_to={}, \
                         invalid_len={}, bytes={}",
                        valid_up_to,
                        invalid_len,
                        debug_utf8_bytes(&self.pending, valid_up_to, Some(invalid_len))
                    );

                    decoded.push(char::REPLACEMENT_CHARACTER);
                    self.pending.drain(..valid_up_to + invalid_len);
                    if self.pending.is_empty() {
                        return Some(decoded);
                    }
                },
            }
        }
    }
}

impl Default for Utf8StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────

fn valid_utf8_prefix(bytes: &[u8]) -> &str {
    // SAFETY: slice comes from `Utf8Error::valid_up_to()`, which guarantees valid UTF-8.
    unsafe { std::str::from_utf8_unchecked(bytes) }
}

/// 格式化 UTF-8 字节片段用于日志输出。
fn debug_utf8_bytes(bytes: &[u8], valid_up_to: usize, invalid_len: Option<usize>) -> String {
    let start = valid_up_to.saturating_sub(8);
    let end = invalid_len
        .map(|len| (valid_up_to + len + 8).min(bytes.len()))
        .unwrap_or(bytes.len().min(valid_up_to + 8));

    bytes[start..end]
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if start + i == valid_up_to {
                format!("[{b:02x}")
            } else if invalid_len.is_some_and(|len| start + i == valid_up_to + len - 1) {
                format!("{b:02x}]")
            } else {
                format!("{b:02x}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 清理 JSON 片段，去除控制字符但保留所有可打印字符（包括 Unicode）。
pub(crate) fn clean_json_fragment(fragment: &str) -> String {
    fragment
        .chars()
        .filter(|&c| !c.is_control() || c.is_whitespace())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_line_reader_splits_on_newline() {
        let mut reader = SseLineReader::new();
        let lines = reader.push_chunk("data: hello\ndata: world\n");
        assert_eq!(lines, vec!["data: hello", "data: world"]);
    }

    #[test]
    fn sse_line_reader_buffers_partial_line() {
        let mut reader = SseLineReader::new();
        let lines = reader.push_chunk("data: hel");
        assert!(lines.is_empty());
        let lines = reader.push_chunk("lo\n");
        assert_eq!(lines, vec!["data: hello"]);
    }

    #[test]
    fn sse_line_reader_flush_returns_remaining() {
        let mut reader = SseLineReader::new();
        reader.push_chunk("data: last");
        assert_eq!(reader.flush(), Some("data: last".to_string()));
    }

    #[test]
    fn sse_line_reader_flush_returns_none_when_empty() {
        let mut reader = SseLineReader::new();
        reader.push_chunk("data: done\n");
        assert_eq!(reader.flush(), None);
    }

    #[test]
    fn sse_line_reader_handles_crlf() {
        let mut reader = SseLineReader::new();
        let lines = reader.push_chunk("data: hello\r\ndata: world\r\n");
        assert_eq!(lines, vec!["data: hello", "data: world"]);
    }

    #[test]
    fn utf8_decoder_handles_multibyte_boundary() {
        let mut decoder = Utf8StreamDecoder::new();
        // "你好" = e4 bd a0 e5 a5 bd
        let first = decoder.push(&[0xe4, 0xbd]);
        assert!(first.is_none());
        let second = decoder.push(&[0xa0, 0xe5, 0xa5, 0xbd]);
        assert_eq!(second.as_deref(), Some("你好"));
    }

    #[test]
    fn utf8_decoder_finish_replaces_incomplete_tail() {
        let mut decoder = Utf8StreamDecoder::new();
        decoder.push(&[0xe4, 0xbd]);
        let result = decoder.finish();
        assert!(result.is_some());
        assert!(result.unwrap().contains('\u{FFFD}'));
    }
}
