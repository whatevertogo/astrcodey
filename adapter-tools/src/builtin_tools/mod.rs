//! # 工具模块
//!
//! 所有内置工具的具体实现，每个工具对应一个独立模块。
//!
//! 工具通过实现 `astrcode_runtime_contract::tool::Tool` trait 提供：
//! - `definition()`: 工具名称、描述、JSON Schema 参数定义
//! - `capability_metadata()`: 权限、副作用级别、Prompt 元数据
//! - `execute()`: 实际执行逻辑

/// 统一 diff 补丁应用工具：多文件 patch
pub mod apply_patch;
/// 文件编辑工具：唯一字符串替换
pub mod edit_file;
/// 进入 plan mode：让模型显式切换到规划阶段
pub mod enter_plan_mode;
/// 退出 plan mode：把计划正式呈递给前端并切回 code
pub mod exit_plan_mode;
/// 文件查找工具：glob 模式匹配
pub mod find_files;
/// 文件系统公共工具：路径解析、取消检查、diff 生成
pub mod fs_common;
/// 内容搜索工具：正则匹配
pub mod grep;
/// mode 切换共享辅助
pub mod mode_transition;
/// 文件读取工具：UTF-8 文本读取
pub mod read_file;
/// session 计划工件共享读写辅助
pub mod session_plan;
/// Shell 命令执行工具：流式 stdout/stderr
pub mod shell;
/// 执行期 task 快照写入工具：维护当前 owner 的工作清单
pub mod task_write;
/// 外部工具搜索：按需展开 MCP/plugin 工具 schema
pub mod tool_search;
/// session 计划工件写工具：仅允许写当前 session 的 plan 目录
pub mod upsert_session_plan;
/// 文件写入工具：创建/覆盖文本文件
pub mod write_file;
