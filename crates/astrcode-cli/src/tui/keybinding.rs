//! 插件注册的快捷键绑定运行时。

/// 从服务端获取的已注册快捷键绑定。
#[derive(Debug, Clone)]
pub struct RegisteredKeybinding {
    /// 快捷键标识（如 "shift+tab", "ctrl+p"）。
    pub key: String,
    /// 触发的命令名（不含 `/`）。
    pub command: String,
    /// 命令参数。
    pub arguments: String,
}

fn canonical_key_id(key: &str) -> String {
    key.split('+')
        .map(|part| {
            let p = part.trim().to_ascii_lowercase();
            match p.as_str() {
                "meta" | "cmd" | "command" => "ctrl".to_string(),
                _ => p,
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// 在已注册的 keybindings 中查找匹配的命令。
pub fn find_command_for_key<'a>(
    keybindings: &'a [RegisteredKeybinding],
    key_id: &str,
) -> Option<(&'a str, &'a str)> {
    let pressed = canonical_key_id(key_id);
    keybindings
        .iter()
        .find(|kb| canonical_key_id(&kb.key) == pressed)
        .map(|kb| (kb.command.as_str(), kb.arguments.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_command_matches_canonical_key() {
        let bindings = vec![RegisteredKeybinding {
            key: "Shift+Tab".into(),
            command: "mode".into(),
            arguments: String::new(),
        }];
        assert_eq!(
            find_command_for_key(&bindings, "shift+tab"),
            Some(("mode", ""))
        );
    }
}
