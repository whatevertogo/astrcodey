//! 内置扩展注册 — 按优先级顺序注册所有内置扩展。
//!
//! 先注册的扩展在同名工具冲突时优先。

use crate::runner::ExtensionRunner;

impl ExtensionRunner {
    /// 按优先级顺序注册所有内置扩展。
    ///
    /// 内置扩展是 astrcode 的核心能力，不依赖磁盘上的扩展文件。
    /// 它们与磁盘加载的外置扩展共享同一个 `ExtensionRunner`，
    /// 区别仅在于发现方式：内置扩展在编译期已知，外置扩展在运行时扫描。
    pub async fn register_builtins(&self) {
        self.register(astrcode_extension_agent_tools::extension())
            .await;
        self.register(astrcode_extension_mcp::extension()).await;
        self.register(astrcode_extension_skill::extension()).await;
        self.register(astrcode_extension_todo_tool::extension())
            .await;
        self.register(astrcode_extension_mode::extension()).await;
    }
}
