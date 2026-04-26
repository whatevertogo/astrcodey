# 通信协议

## JSON-RPC 2.0 over stdio

每行一个完整的 JSON 对象，以 `\n` 分隔。Stderr 保留给诊断输出。

## ClientCommand（前端 → 后端）

| 方法 | 参数 | 说明 |
|------|------|------|
| create_session | working_dir | 创建新 session |
| resume_session | session_id | 恢复已有 session |
| fork_session | session_id, at_cursor? | 分叉 session |
| delete_session | session_id | 删除 session |
| list_sessions | - | 列出所有 session |
| submit_prompt | text, attachments? | 提交 prompt |
| abort | - | 中断当前 turn |
| set_model | model_id | 切换模型 |
| set_thinking_level | level | 设置思考级别 |
| compact | - | 触发上下文压缩 |
| switch_mode | mode | 切换模式 |
| get_state | - | 获取 session 状态 |
| ui_response | request_id, value | 响应 UI 请求 |

## ServerEvent（后端 → 前端）

| 事件 | 说明 |
|------|------|
| session_created | 新 session 创建 |
| session_resumed | session 恢复 |
| session_deleted | session 删除 |
| session_list | session 列表 |
| agent_started | Agent 开始 |
| agent_ended | Agent 结束 |
| turn_started | Turn 开始 |
| turn_ended | Turn 结束 |
| message_start | 消息开始 |
| message_delta | 消息增量（流式） |
| message_end | 消息结束 |
| tool_call_start | 工具调用开始 |
| tool_call_delta | 工具输出增量 |
| tool_call_end | 工具调用结束 |
| compaction_started | 压缩开始 |
| compaction_ended | 压缩结束 |
| ui_request | UI 交互请求 |
| error | 错误 |

## UI 请求子协议

当 server 需要用户交互时：

1. Server 发送 `ui_request { request_id, kind: "confirm"|"select"|"input"|"notify", ... }`
2. Client 展示 UI，收集用户输入
3. Client 发送 `ui_response { request_id, value }`
4. Server 根据响应继续处理

UI 请求支持超时（默认 60s），超时后返回默认值。
