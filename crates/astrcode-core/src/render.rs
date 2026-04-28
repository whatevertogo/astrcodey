//! 结构化 UI 渲染协议。
//!
//! 插件和工具只描述语义化的渲染意图，不直接控制终端。
//! 具体皮肤和终端布局由 CLI 适配层决定。

use serde::{Deserialize, Serialize};

/// 工具结果 metadata 中携带结构化渲染描述的键名。
pub const UI_RENDER_METADATA_KEY: &str = "ui_render";

/// 渲染语气，由具体 TUI 皮肤映射到颜色和样式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RenderTone {
    /// 默认正文。
    #[default]
    Default,
    /// 次要文本。
    Muted,
    /// 强调文本。
    Accent,
    /// 成功状态。
    Success,
    /// 警告状态。
    Warning,
    /// 错误状态。
    Error,
}

/// 键值渲染项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderKeyValue {
    /// 键名。
    pub key: String,
    /// 值文本。
    pub value: String,
    /// 可选语气。
    #[serde(default)]
    pub tone: RenderTone,
}

/// 语义化渲染节点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RenderSpec {
    /// 普通文本。
    Text {
        /// 文本内容。
        text: String,
        /// 可选语气。
        #[serde(default)]
        tone: RenderTone,
    },
    /// Markdown 文本，CLI v1 以安全纯文本方式展示。
    Markdown {
        /// Markdown 内容。
        text: String,
        /// 可选语气。
        #[serde(default)]
        tone: RenderTone,
    },
    /// 分组容器。
    Box {
        /// 可选标题。
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// 可选语气。
        #[serde(default)]
        tone: RenderTone,
        /// 子节点。
        #[serde(default)]
        children: Vec<RenderSpec>,
    },
    /// 列表。
    List {
        /// 是否有序。
        #[serde(default)]
        ordered: bool,
        /// 列表项。
        #[serde(default)]
        items: Vec<RenderSpec>,
        /// 可选语气。
        #[serde(default)]
        tone: RenderTone,
    },
    /// 键值表。
    KeyValue {
        /// 键值项。
        #[serde(default)]
        entries: Vec<RenderKeyValue>,
        /// 可选语气。
        #[serde(default)]
        tone: RenderTone,
    },
    /// 进度状态。
    Progress {
        /// 进度标签。
        label: String,
        /// 状态文本。
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// 0.0 到 1.0 的进度值。
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<f32>,
        /// 可选语气。
        #[serde(default)]
        tone: RenderTone,
    },
    /// Diff 文本。
    Diff {
        /// Diff 内容。
        text: String,
        /// 可选语气。
        #[serde(default)]
        tone: RenderTone,
    },
    /// 代码块。
    Code {
        /// 语言标识。
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        /// 代码内容。
        text: String,
        /// 可选语气。
        #[serde(default)]
        tone: RenderTone,
    },
    /// 图片引用。
    ImageRef {
        /// 图片 URI。
        uri: String,
        /// 替代文本。
        #[serde(skip_serializing_if = "Option::is_none")]
        alt: Option<String>,
        /// 可选语气。
        #[serde(default)]
        tone: RenderTone,
    },
    /// 受限 ANSI 文本。CLI 可选择去除或裁剪控制序列。
    RawAnsiLimited {
        /// 文本内容。
        text: String,
        /// 可选语气。
        #[serde(default)]
        tone: RenderTone,
    },
}

impl RenderSpec {
    /// 生成安全的纯文本回退，供旧渲染路径或错误恢复使用。
    pub fn plain_text_fallback(&self) -> String {
        match self {
            Self::Text { text, .. }
            | Self::Markdown { text, .. }
            | Self::Diff { text, .. }
            | Self::Code { text, .. }
            | Self::RawAnsiLimited { text, .. } => text.clone(),
            Self::Box {
                title, children, ..
            } => {
                let mut parts = title.iter().cloned().collect::<Vec<_>>();
                parts.extend(children.iter().map(Self::plain_text_fallback));
                parts.join("\n")
            },
            Self::List { items, .. } => items
                .iter()
                .map(Self::plain_text_fallback)
                .collect::<Vec<_>>()
                .join("\n"),
            Self::KeyValue { entries, .. } => entries
                .iter()
                .map(|entry| format!("{}: {}", entry.key, entry.value))
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Progress {
                label,
                status,
                value,
                ..
            } => {
                let mut text = label.clone();
                if let Some(status) = status {
                    text.push_str(" · ");
                    text.push_str(status);
                }
                if let Some(value) = value {
                    text.push_str(&format!(" · {:.0}%", value.clamp(0.0, 1.0) * 100.0));
                }
                text
            },
            Self::ImageRef { uri, alt, .. } => {
                format!("[image: {}]", alt.as_deref().unwrap_or(uri))
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_spec_progress_deserializes_with_defaults() {
        let spec: RenderSpec = serde_json::from_value(serde_json::json!({
            "type": "progress",
            "label": "agent",
            "status": "running",
            "value": 0.5
        }))
        .unwrap();

        assert_eq!(
            spec,
            RenderSpec::Progress {
                label: "agent".into(),
                status: Some("running".into()),
                value: Some(0.5),
                tone: RenderTone::Default,
            }
        );
    }

    #[test]
    fn render_spec_plain_text_fallback_is_stable() {
        let spec = RenderSpec::Box {
            title: Some("Tool".into()),
            tone: RenderTone::Accent,
            children: vec![RenderSpec::KeyValue {
                entries: vec![RenderKeyValue {
                    key: "status".into(),
                    value: "done".into(),
                    tone: RenderTone::Success,
                }],
                tone: RenderTone::Default,
            }],
        };

        assert_eq!(spec.plain_text_fallback(), "Tool\nstatus: done");
    }
}
