//! astrcode-support：宿主环境工具集。
//!
//! 提供路径解析、Shell 检测和工具结果持久化等与宿主操作系统相关的功能。
//! 这些功能需要访问宿主 OS，但不属于核心逻辑层。
pub mod hostpaths;
pub mod shell;
pub mod tool_results;
