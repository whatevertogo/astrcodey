# Extension 系统

扩展是 astrcode 的核心可扩展性机制。Skills、Agent Profiles、自定义工具、slash 命令——全部由扩展实现。

## 生命周期事件（12 个）

| 级别 | 事件 | 说明 |
|------|------|------|
| Session | SessionStart | 会话创建时 |
| Session | SessionBeforeFork | 即将 fork 时 |
| Session | SessionBeforeCompact | 即将压缩时 |
| Session | SessionShutdown | 会话关闭时 |
| Agent | AgentStart | Agent 创建时 |
| Agent | AgentEnd | Agent 销毁时 |
| Turn | TurnStart | Turn 开始时 |
| Turn | TurnEnd | Turn 结束时 |
| Message | MessageDelta | 流式消息增量 |
| Tool | BeforeToolCall | 工具调用前（可阻断） |
| Tool | AfterToolCall | 工具调用后（可修改结果） |
| Input | UserPromptSubmit | 用户提交 prompt 时 |

## HookMode（3 种）

- **Blocking**: 同步执行，可阻断操作。用于安全审查、权限控制
- **NonBlocking**: 异步执行（fire-and-forget），不可阻断。用于日志、分析
- **Advisory**: 结果仅供参考。用于风格建议

## 加载策略

- **全局扩展**：`~/.astrcode/extensions/`，server 启动时加载
- **项目扩展**：`.astrcode/extensions/`，session 创建时加载
- 合并时项目级优先执行

## 如何用扩展实现 Skill

```rust
struct SkillExtension;

impl Extension for SkillExtension {
    fn id(&self) -> &str { "skill-loader" }

    fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)> {
        vec![
            (ExtensionEvent::SessionStart, HookMode::NonBlocking),
            (ExtensionEvent::BeforeToolCall, HookMode::Blocking),
        ]
    }

    async fn on_event(&self, event: ExtensionEvent, ctx: &dyn ExtensionContext) -> Result<HookEffect, ExtensionError> {
        match event {
            ExtensionEvent::SessionStart => {
                // 扫描 skill 目录，注册到 context
                // 注入 skill 摘要到 prompt
                Ok(HookEffect::Allow)
            }
            ExtensionEvent::BeforeToolCall => {
                // 如果是 skillTool 调用，加载 skill guide
                // 返回 Modify 替换工具输出为完整的 skill 指令
                Ok(HookEffect::Allow)
            }
            _ => Ok(HookEffect::Allow),
        }
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![] // skillTool 由核心提供，扩展处理加载逻辑
    }

    fn context_contributions(&self) -> Vec<BlockSpec> {
        vec![] // skill 摘要通过 SessionStart hook 注入
    }
}
```

## 如何用扩展实现 Agent Profiles

同 pattern：扩展在 SessionStart 时扫描 agent 定义文件，通过 `context_contributions()` 注入 agent 摘要到 prompt。
