//! FFI 边界 — 扩展与宿主之间通信的 C ABI vtable。
//!
//! 扩展是 `.dll`/`.so` 文件。它们导出一个单一函数：
//! ```c
//! void extension_factory(const ExtensionApi *api);
//! ```
//!
//! 字符串以 `(ptr: *const u8, len: u32)` 对的形式穿越 FFI 边界。

use std::ffi::c_void;

use astrcode_core::extension::{ExtensionEvent, HookMode};

/// 扩展注册的钩子处理回调函数签名。
///
/// `effect_out` 取值: 0=Allow, 1=Block, 2=ModifiedResult, 3=ModifiedInput。
/// - Block (1): `output_ptr/len` 携带阻止原因字符串。
/// - ModifiedResult (2): `output_ptr/len` 携带修改后的内容字符串。
/// - ModifiedInput (3): `output_ptr/len` 携带替换的工具输入 JSON。
/// - PromptContributions (4): `output_ptr/len` 携带 PromptContributions JSON。
/// - CompactContributions (5): `output_ptr/len` 携带 CompactContributions JSON。
pub type EventCallback = unsafe extern "C" fn(
    event: u8,
    ctx: *const c_void,
    effect_out: *mut u8,
    output_ptr_out: *mut *const u8,
    output_len_out: *mut u32,
);

/// 扩展注册的可执行工具回调函数签名。
///
/// 返回值语义 (u8):
///   `0` — 纯文本成功。`output_ptr/len` 携带结果内容字符串。
///   `1` — 工具错误。`error_ptr/len` 携带错误消息。
///   `2` — JSON 结果。`output_ptr/len` 携带序列化的 `ExtensionToolOutcome`。
///
/// 返回码 0 是历史默认值 — 现有回调无需修改即可兼容。
pub type ToolCallback = unsafe extern "C" fn(
    ctx: *const c_void,
    output_ptr_out: *mut *const u8,
    output_len_out: *mut u32,
    error_ptr_out: *mut *const u8,
    error_len_out: *mut u32,
) -> u8;

/// 扩展注册的输出释放回调。
///
/// 插件通过它释放自己写入 `output_ptr/error_ptr` 的字符串内存。
/// 宿主只在读完输出后调用，不跨动态库直接释放插件内存。
pub type OutputFreeCallback = unsafe extern "C" fn(ptr: *const u8, len: u32);

/// 将 `ToolCallback` 的返回值解析为 `ExtensionToolOutcome`。
///
/// - 返回 0 → `Text { content, is_error: false }`
/// - 返回 1 → `Text { content: error, is_error: true }`
/// - 返回 2 → 将 `output_ptr/len` 反序列化为 JSON `ExtensionToolOutcome`
///
/// # Safety
///
/// 传入的任何非空指针/长度对必须在此调用期间指向有效的 UTF-8 字节。
pub unsafe fn parse_tool_outcome(
    ret: u8,
    output_ptr: *const u8,
    output_len: u32,
    error_ptr: *const u8,
    error_len: u32,
) -> Result<astrcode_core::extension::ExtensionToolOutcome, String> {
    match ret {
        0 => {
            let content = if output_ptr.is_null() || output_len == 0 {
                String::new()
            } else {
                unsafe { read_ffi_str(output_ptr, output_len) }.to_string()
            };
            Ok(astrcode_core::extension::ExtensionToolOutcome::Text {
                content,
                is_error: false,
            })
        },
        1 => {
            let error = if error_ptr.is_null() || error_len == 0 {
                String::new()
            } else {
                unsafe { read_ffi_str(error_ptr, error_len) }.to_string()
            };
            Ok(astrcode_core::extension::ExtensionToolOutcome::Text {
                content: error,
                is_error: true,
            })
        },
        2 => {
            let json = if output_ptr.is_null() || output_len == 0 {
                return Err("return code 2 but output is empty".into());
            } else {
                unsafe { read_ffi_str(output_ptr, output_len) }
            };
            serde_json::from_str(json).map_err(|e| format!("parse outcome JSON: {e}"))
        },
        other => Err(format!("unknown ToolCallback return code: {other}")),
    }
}

/// 传递给 `extension_factory()` 的 vtable。
#[repr(C)]
pub struct ExtensionApi {
    /// 用于事件处理器注册的不透明宿主数据
    pub user_data: *mut c_void,

    /// 注册事件处理器
    pub on: unsafe extern "C" fn(
        api: *const ExtensionApi,
        event: u8,
        mode: u8,
        callback: EventCallback,
    ),

    /// 注册工具定义
    pub register_tool: unsafe extern "C" fn(
        api: *const ExtensionApi,
        name_ptr: *const u8,
        name_len: u32,
        desc_ptr: *const u8,
        desc_len: u32,
        params_json_ptr: *const u8,
        params_json_len: u32,
    ),

    /// 为先前声明的工具注册可执行处理器
    pub register_tool_handler: unsafe extern "C" fn(
        api: *const ExtensionApi,
        name_ptr: *const u8,
        name_len: u32,
        callback: ToolCallback,
    ),

    /// 注册斜杠命令
    pub register_command: unsafe extern "C" fn(
        api: *const ExtensionApi,
        name_ptr: *const u8,
        name_len: u32,
        desc_ptr: *const u8,
        desc_len: u32,
    ),

    /// 注册插件侧输出释放回调。
    pub register_output_free_handler:
        unsafe extern "C" fn(api: *const ExtensionApi, callback: OutputFreeCallback),
}

// ─── 判别值辅助函数 ────────────────────────────────────────────────

/// 将 [`ExtensionEvent`] 转换为 FFI 层的 u8 判别值。
pub const fn event_discriminant(event: ExtensionEvent) -> u8 {
    match event {
        ExtensionEvent::SessionStart => 0,
        ExtensionEvent::SessionShutdown => 1,
        ExtensionEvent::TurnStart => 2,
        ExtensionEvent::TurnEnd => 3,
        ExtensionEvent::PreToolUse => 4,
        ExtensionEvent::PostToolUse => 5,
        ExtensionEvent::BeforeProviderRequest => 6,
        ExtensionEvent::AfterProviderResponse => 7,
        ExtensionEvent::UserPromptSubmit => 8,
        ExtensionEvent::PromptBuild => 9,
        ExtensionEvent::PreCompact => 10,
        ExtensionEvent::PostCompact => 11,
    }
}

/// 将 u8 判别值转换回 [`ExtensionEvent`]。
pub fn event_from_discriminant(d: u8) -> Option<ExtensionEvent> {
    match d {
        0 => Some(ExtensionEvent::SessionStart),
        1 => Some(ExtensionEvent::SessionShutdown),
        2 => Some(ExtensionEvent::TurnStart),
        3 => Some(ExtensionEvent::TurnEnd),
        4 => Some(ExtensionEvent::PreToolUse),
        5 => Some(ExtensionEvent::PostToolUse),
        6 => Some(ExtensionEvent::BeforeProviderRequest),
        7 => Some(ExtensionEvent::AfterProviderResponse),
        8 => Some(ExtensionEvent::UserPromptSubmit),
        9 => Some(ExtensionEvent::PromptBuild),
        10 => Some(ExtensionEvent::PreCompact),
        11 => Some(ExtensionEvent::PostCompact),
        _ => None,
    }
}

/// 将 [`HookMode`] 转换为 FFI 层的 u8 判别值。
pub const fn mode_discriminant(mode: HookMode) -> u8 {
    match mode {
        HookMode::Blocking => 0,
        HookMode::NonBlocking => 1,
        HookMode::Advisory => 2,
    }
}

/// 将 u8 判别值转换回 [`HookMode`]。
pub fn mode_from_discriminant(d: u8) -> Option<HookMode> {
    match d {
        0 => Some(HookMode::Blocking),
        1 => Some(HookMode::NonBlocking),
        2 => Some(HookMode::Advisory),
        _ => None,
    }
}

/// 从 FFI 的 (ptr, len) 对读取 Rust &str。
///
/// # Safety
/// `ptr` 必须指向 `len` 字节的有效 UTF-8 数据。
pub unsafe fn read_ffi_str<'a>(ptr: *const u8, len: u32) -> &'a str {
    if ptr.is_null() || len == 0 {
        return "";
    }
    let bytes = std::slice::from_raw_parts(ptr, len as usize);
    std::str::from_utf8_unchecked(bytes)
}

// ─── FFI 上下文 ────────────────────────────────────────────────

/// 传递给扩展事件和工具回调的上下文。
///
/// 包含会话的最小只读视图和当前执行数据。
/// 所有字符串都是 (ptr, len) 对，指向宿主拥有的 UTF-8 数据，
/// 在回调期间保持有效。
#[repr(C)]
pub struct FfiCtx {
    /// 会话 ID (ptr, len)
    pub session_id_ptr: *const u8,
    pub session_id_len: u32,
    /// 工作目录 (ptr, len)
    pub working_dir_ptr: *const u8,
    pub working_dir_len: u32,
    /// 工具名称 (ptr, len) — 仅在 PreToolUse/PostToolUse 和工具执行时设置
    pub tool_name_ptr: *const u8,
    pub tool_name_len: u32,
    /// 工具输入 JSON (ptr, len) — 仅在 PreToolUse/PostToolUse 和工具执行时设置
    pub tool_input_ptr: *const u8,
    pub tool_input_len: u32,
    /// 当前模型 ID (ptr, len) — 在工具执行时设置
    pub model_id_ptr: *const u8,
    pub model_id_len: u32,
    /// 可用工具 JSON (ptr, len) — 序列化的 Vec<ToolDefinition>，在工具执行时设置
    pub tools_json_ptr: *const u8,
    pub tools_json_len: u32,
}

impl FfiCtx {
    /// 从各字符串部分构建 FFI 上下文。
    fn from_parts(
        session_id: &str,
        working_dir: &str,
        tool_name: &str,
        tool_input_json: &str,
        model_id: &str,
        tools_json: &str,
    ) -> Self {
        let sid = session_id.as_bytes();
        let wd = working_dir.as_bytes();
        let tn = tool_name.as_bytes();
        let input = tool_input_json.as_bytes();
        let mid = model_id.as_bytes();
        let tj = tools_json.as_bytes();
        Self {
            session_id_ptr: sid.as_ptr(),
            session_id_len: sid.len() as u32,
            working_dir_ptr: wd.as_ptr(),
            working_dir_len: wd.len() as u32,
            tool_name_ptr: tn.as_ptr(),
            tool_name_len: tn.len() as u32,
            tool_input_ptr: input.as_ptr(),
            tool_input_len: input.len() as u32,
            model_id_ptr: mid.as_ptr(),
            model_id_len: mid.len() as u32,
            tools_json_ptr: tj.as_ptr(),
            tools_json_len: tj.len() as u32,
        }
    }
}

/// `FfiCtx` 的拥有型后备存储。
///
/// `FfiCtx` 本身是原始 C 视图，因此此包装器保持所有被指向的
/// 字符串在回调期间存活。
pub struct FfiCtxOwned {
    session_id: String,
    working_dir: String,
    tool_name: String,
    tool_input_json: String,
    model_id: String,
    tools_json: String,
    raw: FfiCtx,
}

impl FfiCtxOwned {
    /// 从扩展上下文构建 FFI 上下文（用于事件钩子）。
    pub fn from_ext_ctx(ctx: &dyn astrcode_core::extension::ExtensionContext) -> Self {
        let session_id = ctx.session_id().to_string();
        let working_dir = ctx.working_dir().to_string();
        let pre = ctx.pre_tool_use_input();
        let post = ctx.post_tool_use_input();
        let model_id = ctx.model_selection().model;
        // 优先使用 PreToolUse 输入，其次使用 PostToolUse 输入
        let (tool_name, tool_input) = if let Some(input) = pre {
            (input.tool_name, input.tool_input)
        } else if let Some(input) = post {
            (input.tool_name, input.tool_input)
        } else {
            (String::new(), serde_json::Value::Null)
        };
        let tool_input_json = if tool_input.is_null() {
            String::new()
        } else {
            tool_input.to_string()
        };
        Self::new(
            session_id,
            working_dir,
            tool_name,
            tool_input_json,
            model_id,
            String::new(), // tools_json — 事件钩子不需要
        )
    }

    /// 从工具执行上下文构建 FFI 上下文（用于工具回调）。
    pub fn from_tool_execution(
        working_dir: &str,
        tool_name: &str,
        tool_input: &serde_json::Value,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Self {
        let tools_json = serde_json::to_string(&ctx.available_tools).unwrap_or_default();
        Self::new(
            ctx.session_id.clone(),
            working_dir.to_string(),
            tool_name.to_string(),
            tool_input.to_string(),
            ctx.model_id.clone(),
            tools_json,
        )
    }

    /// 内部构造函数：先创建拥有型字符串，再构建指向它们的原始 FfiCtx。
    fn new(
        session_id: String,
        working_dir: String,
        tool_name: String,
        tool_input_json: String,
        model_id: String,
        tools_json: String,
    ) -> Self {
        let mut owned = Self {
            session_id,
            working_dir,
            tool_name,
            tool_input_json,
            model_id,
            tools_json,
            raw: FfiCtx {
                session_id_ptr: std::ptr::null(),
                session_id_len: 0,
                working_dir_ptr: std::ptr::null(),
                working_dir_len: 0,
                tool_name_ptr: std::ptr::null(),
                tool_name_len: 0,
                tool_input_ptr: std::ptr::null(),
                tool_input_len: 0,
                model_id_ptr: std::ptr::null(),
                model_id_len: 0,
                tools_json_ptr: std::ptr::null(),
                tools_json_len: 0,
            },
        };
        // 让 raw 中的指针指向 owned 中的字符串数据
        owned.raw = FfiCtx::from_parts(
            &owned.session_id,
            &owned.working_dir,
            &owned.tool_name,
            &owned.tool_input_json,
            &owned.model_id,
            &owned.tools_json,
        );
        owned
    }

    /// 返回底层 `FfiCtx` 的原始指针，用于传递给 FFI 回调。
    pub fn as_ptr(&self) -> *const c_void {
        &self.raw as *const FfiCtx as *const c_void
    }
}
