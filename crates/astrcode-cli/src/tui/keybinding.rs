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

/// 在已注册的 keybindings 中查找匹配的命令。
pub fn find_command_for_key<'a>(
    keybindings: &'a [RegisteredKeybinding],
    key_id: &str,
) -> Option<(&'a str, &'a str)> {
    keybindings
        .iter()
        .find(|kb| kb.key == key_id)
        .map(|kb| (kb.command.as_str(), kb.arguments.as_str()))
}
