//! Transcript 前缀指纹：rewrite 乐观并发校验用的跨进程稳定哈希。

use crate::llm::LlmMessage;

const FNV1A64_OFFSET: u64 = 0xcbf29ce484222325;
const FNV1A64_PRIME: u64 = 0x100000001b3;
/// 段间分隔符，防止字段边界处的拼接歧义。
const SEGMENT_SEPARATOR: u8 = 0xff;

/// 稳定的十六进制段哈希：对每段逐字节做 FNV-1a（64 位），段间插入 `0xff` 分隔。
///
/// 手写 FNV-1a 而非 `DefaultHasher`：哈希结果进入持久化指纹与 prompt cache key
/// 等跨边界契约，必须跨进程、跨构建稳定；`DefaultHasher` 不保证跨版本稳定。
pub fn stable_hash_hex(parts: &[&str]) -> String {
    let mut hash = FNV1A64_OFFSET;
    for part in parts {
        hash = fnv1a_update(hash, part.as_bytes());
        hash = fnv1a_update(hash, &[SEGMENT_SEPARATOR]);
    }
    format!("{hash:016x}")
}

/// 覆盖 system prompt 文本与将被替换的 transcript 前缀（provider 视角消息）。
///
/// 指纹持久化在 `TranscriptRewritten` 事件里，由提交时的 projection 重算比较。
///
/// 跨版本不变式：消息段取自 `serde_json` 派生序列化，字段名与字段顺序直接决定
/// 哈希输入。`LlmMessage` / `LlmContent` 的序列化形状变更（改名、重排、新增无
/// `skip_serializing_if` 的字段）会静默改变指纹，使新旧进程对同一前缀算出不同
/// 值；这类变更必须显式决策，不能当作普通重构。
///
/// 当前 `LlmContent` 只含 `String`/`bool`/`serde_json::Value`，序列化实际不会失败；
/// 但该不变量未被类型系统编码——未来新增字段（如裸 `f64`）可能引入 JSON 无法编码
/// 的值。因此返回 `Result`，把潜在失败交由调用方显式处理，而非在持久化路径上 panic。
pub fn transcript_prefix_fingerprint(
    system_prompt: &str,
    messages: &[LlmMessage],
) -> Result<String, serde_json::Error> {
    let serialized_messages: Vec<String> = messages
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<_, _>>()?;
    let mut parts: Vec<&str> = Vec::with_capacity(serialized_messages.len() + 1);
    parts.push(system_prompt);
    parts.extend(serialized_messages.iter().map(String::as_str));
    Ok(stable_hash_hex(&parts))
}

fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV1A64_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_content_sensitive() {
        let messages = vec![LlmMessage::user("hello")];
        let first = transcript_prefix_fingerprint("system", &messages).unwrap();
        assert_eq!(
            first,
            transcript_prefix_fingerprint("system", &messages).unwrap()
        );
        assert_ne!(
            first,
            transcript_prefix_fingerprint("system changed", &messages).unwrap()
        );
        assert_ne!(
            first,
            transcript_prefix_fingerprint("system", &[LlmMessage::user("changed")]).unwrap()
        );
    }

    #[test]
    fn fingerprint_matches_reference_fnv1a() {
        // 独立参考实现，锁定持久化格式：字段序或算法变动都必须显式决策。
        fn reference(parts: &[&str]) -> String {
            let mut hash = FNV1A64_OFFSET;
            for part in parts {
                for &byte in part.as_bytes() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(FNV1A64_PRIME);
                }
                hash ^= u64::from(SEGMENT_SEPARATOR);
                hash = hash.wrapping_mul(FNV1A64_PRIME);
            }
            format!("{hash:016x}")
        }

        let message = LlmMessage::user("hello");
        let serialized = serde_json::to_string(&message).unwrap();
        assert_eq!(
            transcript_prefix_fingerprint("system", &[message]).unwrap(),
            reference(&["system", &serialized])
        );
    }

    #[test]
    fn fingerprint_inputs_are_locked_to_llm_message_serialization() {
        // 锁定指纹的全部输入：派生字段序与十六进制输出。任一变化都意味着
        // 已持久化事件的指纹校验语义改变，必须显式决策而非顺手修改。
        let message = LlmMessage::user("hello");
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"user","content":[{"type":"text","text":"hello"}]}"#
        );
        assert_eq!(
            transcript_prefix_fingerprint("system", &[message]).unwrap(),
            "8cadff38c396770a"
        );
    }
}
