//! 持久化 PTY 终端工具，让 LLM 可以驱动 REPL/调试器等需要多步交互的进程。
//!
//! 与 [`ShellTool`](crate::shell_tool::ShellTool) 一次性执行不同，本工具维持一个
//! 长生命周期的 PTY 子进程：LLM 可以多次写入 stdin、轮询 stdout，直到主动关闭。
//!
//! 典型用例：
//! - 运行 `python -i` / `node` REPL
//! - 驱动 `gdb` / `pdb` 调试器
//! - 跑需要多次 prompt 输入的安装/配置脚本
//!
//! 设计要点：
//! - 进程级 registry：所有 session 共享同一个 `TerminalRegistry`，以 terminal_id 索引
//! - 每个终端独立的 OS 线程同步读取 PTY master，输出推入 ring buffer
//! - read action 返回自上次 read 后的新增字节，避免重复消费
//! - 输出 ring buffer 上限 1MB，超出截断最旧数据并标记 `dropped: true`
//! - LLM 显式 close 释放资源；进程自然退出时 reader 线程检测 EOF 并标记 dead

use std::{
    collections::{BTreeMap, HashMap},
    io::{Read, Write},
    sync::{Arc, OnceLock},
    time::Duration,
};

use astrcode_core::tool::*;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde::Deserialize;
use uuid::Uuid;

use crate::files::{run_blocking, tool_call_id};

const MAX_BUFFER_BYTES: usize = 1_024 * 1_024; // 1 MB
const DEFAULT_READ_WAIT_MS: u64 = 100;
const MAX_READ_WAIT_MS: u64 = 10_000;
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// 终端空闲超时：最后一次 send/read 后超过此时间自动关闭。
const IDLE_TIMEOUT: Duration = Duration::from_secs(600); // 10 分钟
/// 每个 session 允许的最大并发终端数。
const MAX_TERMINALS_PER_SESSION: usize = 5;

/// 清理指定 session 的所有终端。
///
/// 在 session 关闭/销毁时调用，确保不会泄漏 PTY 子进程。
pub fn cleanup_terminals_for_session(session_id: &str) {
    TerminalRegistry::cleanup_session(session_id);
}

// ─── Registry ────────────────────────────────────────────────────────────

/// 单个终端的运行时状态（buffer + 写端 + 进程引用）。
struct TerminalEntry {
    /// 所属 session ID，用于隔离和批量清理。
    session_id: String,
    /// 最后一次 send/read 操作的时间戳，用于空闲超时判定。
    last_activity: Mutex<std::time::Instant>,
    /// 接收读线程写入的 stdout/stderr。每次 read action 取走后清空。
    output: Mutex<TerminalBuffer>,
    /// PTY master 写端（向子进程 stdin 写入）。
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    /// PTY master 必须存活直到终端关闭，否则 reader 会立刻收到 EOF
    /// 而子进程会收到 SIGHUP。drop 此字段等同于关闭终端。
    _master: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
    /// PTY 子进程引用。Drop 时关闭 PTY 终止子进程。
    child: Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>,
    /// 同步 reader 线程句柄；close/cleanup 时 join，避免泄漏。
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// 子进程结束后填入退出码。reader 线程检测到 EOF 后由 wait 线程更新。
    exit_code: Mutex<Option<i32>>,
}

#[derive(Default)]
struct TerminalBuffer {
    bytes: Vec<u8>,
    dropped_bytes: usize,
}

impl TerminalBuffer {
    fn append(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > MAX_BUFFER_BYTES {
            let overflow = self.bytes.len() - MAX_BUFFER_BYTES;
            self.bytes.drain(..overflow);
            self.dropped_bytes += overflow;
        }
    }

    fn take(&mut self) -> (String, usize) {
        let bytes = std::mem::take(&mut self.bytes);
        let dropped = std::mem::take(&mut self.dropped_bytes);
        (String::from_utf8_lossy(&bytes).into_owned(), dropped)
    }
}

/// 全局终端 registry，按 session 隔离并提供自动清理。
struct TerminalRegistry {
    terminals: Mutex<HashMap<String, Arc<TerminalEntry>>>,
}

impl TerminalRegistry {
    fn global() -> &'static TerminalRegistry {
        static REGISTRY: OnceLock<TerminalRegistry> = OnceLock::new();
        REGISTRY.get_or_init(|| TerminalRegistry {
            terminals: Mutex::new(HashMap::new()),
        })
    }

    fn insert(&self, id: String, entry: Arc<TerminalEntry>) {
        self.terminals.lock().insert(id, entry);
    }

    fn get(&self, id: &str) -> Option<Arc<TerminalEntry>> {
        self.terminals.lock().get(id).cloned()
    }

    fn remove(&self, id: &str) -> Option<Arc<TerminalEntry>> {
        self.terminals.lock().remove(id)
    }

    /// 列出指定 session 的活跃终端 ID。
    fn list_for_session(&self, session_id: &str) -> Vec<String> {
        self.terminals
            .lock()
            .iter()
            .filter(|(_, entry)| entry.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 统计指定 session 当前的终端数。
    fn count_for_session(&self, session_id: &str) -> usize {
        self.terminals
            .lock()
            .values()
            .filter(|entry| entry.session_id == session_id)
            .count()
    }

    /// 清理指定 session 的所有终端（session 关闭时调用）。
    pub fn cleanup_session(session_id: &str) {
        let registry = Self::global();
        let ids: Vec<String> = registry
            .terminals
            .lock()
            .iter()
            .filter(|(_, entry)| entry.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect();

        let count = ids.len();
        for id in ids {
            if let Some(entry) = registry.remove(&id) {
                kill_entry(&entry);
            }
        }
        if count > 0 {
            tracing::info!(session_id, count, "cleaned up session terminals");
        }
    }

    /// 清理所有空闲超时和已死亡的终端。
    ///
    /// 由 `list` / `start` action 触发，避免专门的后台线程。
    fn gc(&self) {
        let now = std::time::Instant::now();
        let to_remove: Vec<String> = self
            .terminals
            .lock()
            .iter()
            .filter(|(_, entry)| {
                let idle = now.duration_since(*entry.last_activity.lock()) > IDLE_TIMEOUT;
                let dead = entry.exit_code.lock().is_some();
                idle || dead
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in &to_remove {
            if let Some(entry) = self.remove(id) {
                kill_entry(&entry);
                tracing::debug!(terminal_id = %id, session_id = %entry.session_id, "terminal gc'd");
            }
        }
    }
}

/// 带超时 join reader，避免 Windows ConPTY 上永久阻塞。
fn join_reader_thread(handle: std::thread::JoinHandle<()>) {
    use std::sync::mpsc;
    let (done_tx, done_rx) = mpsc::sync_channel(0);
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(READER_JOIN_TIMEOUT).is_err() {
        tracing::warn!(
            timeout_secs = READER_JOIN_TIMEOUT.as_secs(),
            "terminal reader thread did not exit in time; detaching"
        );
    }
}

/// 杀掉 entry 中的子进程并 join reader 线程。
fn kill_entry(entry: &TerminalEntry) {
    if let Some(mut child) = entry.child.lock().take() {
        let _ = child.kill();
        let exit_code = child
            .wait()
            .ok()
            .map(|status| status.exit_code() as i32)
            .unwrap_or(-1);
        *entry.exit_code.lock() = Some(exit_code);
    }
    entry.writer.lock().take();
    entry._master.lock().take();
    if let Some(handle) = entry.reader.lock().take() {
        join_reader_thread(handle);
    }
}

// ─── Tool ────────────────────────────────────────────────────────────────

/// 持久化 PTY 终端工具。
pub struct TerminalTool {
    pub working_dir: std::path::PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalArgs {
    /// 操作类型：start / send / read / close / list
    action: String,
    /// 终端 ID（send / read / close 必填）
    #[serde(default)]
    id: Option<String>,
    /// 启动命令（start 必填）
    #[serde(default)]
    command: Option<String>,
    /// 命令参数（start 可选）
    #[serde(default)]
    args: Option<Vec<String>>,
    /// 启动时的工作目录（start 可选，默认工具 working_dir）
    #[serde(default)]
    cwd: Option<String>,
    /// 写入 stdin 的内容（send 必填）
    #[serde(default)]
    input: Option<String>,
    /// read 等待新输出的最大毫秒数（默认 100ms，最大 10000ms）
    #[serde(default)]
    wait_ms: Option<u64>,
}

#[async_trait::async_trait]
impl Tool for TerminalTool {
    fn definition(&self) -> ToolDefinition {
        terminal_tool_definition().clone()
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn prompt_metadata(&self) -> Option<ToolPromptMetadata> {
        Some(ToolPromptMetadata::new(String::new()).prompt_tag(ToolPromptTag::System))
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let started_at = std::time::Instant::now();
        let args: TerminalArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid terminal args: {e}")))?;

        let result = match args.action.as_str() {
            "start" => {
                let tool = self.working_dir.clone();
                let session_id = ctx.session_id.as_str().to_string();
                run_blocking(move || action_start(&tool, args, &session_id)).await?
            },
            "send" => action_send(args)?,
            "read" => action_read(args).await?,
            "close" => action_close(args)?,
            "list" => action_list(ctx.session_id.as_str())?,
            other => {
                return Err(ToolError::InvalidArguments(format!(
                    "unknown action '{other}', expected start / send / read / close / list"
                )));
            },
        };

        let (content, metadata, is_error) = result;

        Ok(ToolResult {
            call_id: tool_call_id(ctx),
            content,
            is_error,
            error: None,
            metadata,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}

// ─── Action handlers ─────────────────────────────────────────────────────

fn action_start(
    working_dir: &std::path::Path,
    args: TerminalArgs,
    session_id: &str,
) -> Result<(String, BTreeMap<String, serde_json::Value>, bool), ToolError> {
    let registry = TerminalRegistry::global();

    // 先做一轮 GC 清理空闲/死亡终端
    registry.gc();

    // 检查 session 并发限制
    if registry.count_for_session(session_id) >= MAX_TERMINALS_PER_SESSION {
        return Err(ToolError::Execution(format!(
            "session already has {MAX_TERMINALS_PER_SESSION} active terminals; close one first"
        )));
    }

    let command = args
        .command
        .ok_or_else(|| ToolError::InvalidArguments("'start' requires 'command'".into()))?;
    let cwd = args
        .cwd
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| working_dir.to_path_buf());

    let pty = NativePtySystem::default();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| ToolError::Execution(format!("openpty: {e}")))?;

    let mut cmd = CommandBuilder::new(&command);
    if let Some(extra) = args.args {
        for arg in extra {
            cmd.arg(arg);
        }
    }
    cmd.cwd(cwd);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| ToolError::Execution(format!("spawn: {e}")))?;

    // slave 端在父进程不再需要，关闭以避免 reader 永远读不到 EOF。
    drop(pair.slave);

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| ToolError::Execution(format!("take_writer: {e}")))?;
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| ToolError::Execution(format!("clone_reader: {e}")))?;

    let id = format!("term-{}", Uuid::new_v4());
    let entry = Arc::new(TerminalEntry {
        session_id: session_id.to_string(),
        last_activity: Mutex::new(std::time::Instant::now()),
        output: Mutex::new(TerminalBuffer::default()),
        writer: Mutex::new(Some(writer)),
        _master: Mutex::new(Some(pair.master)),
        child: Mutex::new(Some(child)),
        reader: Mutex::new(None),
        exit_code: Mutex::new(None),
    });

    // 后台同步读线程：把 PTY master 的输出写进 ring buffer。
    // PTY reader 是阻塞同步 IO，必须用 OS 线程而非 tokio task。
    {
        let entry_for_thread = Arc::clone(&entry);
        let handle = std::thread::Builder::new()
            .name(format!("astrcode-terminal-reader-{id}"))
            .spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break, // EOF
                        Ok(n) => entry_for_thread.output.lock().append(&buf[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                if entry_for_thread.exit_code.lock().is_none() {
                    *entry_for_thread.exit_code.lock() = Some(-1);
                }
            })
            .map_err(|e| ToolError::Execution(format!("spawn reader thread: {e}")))?;
        *entry.reader.lock() = Some(handle);
    }

    TerminalRegistry::global().insert(id.clone(), entry);

    let mut metadata = BTreeMap::new();
    metadata.insert("id".into(), serde_json::json!(id.clone()));
    metadata.insert("command".into(), serde_json::json!(command));
    Ok((format!("terminal started: {id}"), metadata, false))
}

fn action_send(
    args: TerminalArgs,
) -> Result<(String, BTreeMap<String, serde_json::Value>, bool), ToolError> {
    let id = args
        .id
        .ok_or_else(|| ToolError::InvalidArguments("'send' requires 'id'".into()))?;
    let input = args
        .input
        .ok_or_else(|| ToolError::InvalidArguments("'send' requires 'input'".into()))?;

    let entry = TerminalRegistry::global()
        .get(&id)
        .ok_or_else(|| ToolError::Execution(format!("terminal '{id}' not found")))?;

    // 更新活跃时间
    *entry.last_activity.lock() = std::time::Instant::now();

    {
        let mut writer = entry.writer.lock();
        let Some(writer) = writer.as_mut() else {
            return Err(ToolError::Execution(format!("terminal '{id}' is closed")));
        };
        writer
            .write_all(input.as_bytes())
            .map_err(|e| ToolError::Execution(format!("write stdin: {e}")))?;
        writer
            .flush()
            .map_err(|e| ToolError::Execution(format!("flush stdin: {e}")))?;
    }

    let mut metadata = BTreeMap::new();
    metadata.insert("id".into(), serde_json::json!(id));
    metadata.insert("bytesSent".into(), serde_json::json!(input.len()));
    Ok(("sent".into(), metadata, false))
}

async fn action_read(
    args: TerminalArgs,
) -> Result<(String, BTreeMap<String, serde_json::Value>, bool), ToolError> {
    let id = args
        .id
        .ok_or_else(|| ToolError::InvalidArguments("'read' requires 'id'".into()))?;
    let wait_ms = args
        .wait_ms
        .unwrap_or(DEFAULT_READ_WAIT_MS)
        .min(MAX_READ_WAIT_MS);

    let entry = TerminalRegistry::global()
        .get(&id)
        .ok_or_else(|| ToolError::Execution(format!("terminal '{id}' not found")))?;

    // 更新活跃时间
    *entry.last_activity.lock() = std::time::Instant::now();

    // 等待新输出：先睡 wait_ms 让 reader 线程有机会写入。
    if wait_ms > 0 {
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    }

    let (output, dropped) = entry.output.lock().take();
    let exit_code = *entry.exit_code.lock();
    let alive = exit_code.is_none();

    let mut metadata = BTreeMap::new();
    metadata.insert("id".into(), serde_json::json!(id));
    metadata.insert("alive".into(), serde_json::json!(alive));
    if dropped > 0 {
        metadata.insert("droppedBytes".into(), serde_json::json!(dropped));
    }
    if let Some(code) = exit_code {
        metadata.insert("exitCode".into(), serde_json::json!(code));
    }

    Ok((output, metadata, false))
}

fn action_close(
    args: TerminalArgs,
) -> Result<(String, BTreeMap<String, serde_json::Value>, bool), ToolError> {
    let id = args
        .id
        .ok_or_else(|| ToolError::InvalidArguments("'close' requires 'id'".into()))?;

    let entry = TerminalRegistry::global()
        .remove(&id)
        .ok_or_else(|| ToolError::Execution(format!("terminal '{id}' not found")))?;

    kill_entry(&entry);

    let exit_code = *entry.exit_code.lock();
    let mut metadata = BTreeMap::new();
    metadata.insert("id".into(), serde_json::json!(id));
    if let Some(code) = exit_code {
        metadata.insert("exitCode".into(), serde_json::json!(code));
    }
    Ok(("closed".into(), metadata, false))
}

fn action_list(
    session_id: &str,
) -> Result<(String, BTreeMap<String, serde_json::Value>, bool), ToolError> {
    let registry = TerminalRegistry::global();
    registry.gc();
    let ids = registry.list_for_session(session_id);
    let mut metadata = BTreeMap::new();
    metadata.insert("count".into(), serde_json::json!(ids.len()));
    metadata.insert("terminals".into(), serde_json::json!(ids.clone()));
    let content = if ids.is_empty() {
        "no active terminals".into()
    } else {
        ids.join("\n")
    };
    Ok((content, metadata, false))
}

// ─── Definition ──────────────────────────────────────────────────────────

fn terminal_tool_definition() -> &'static ToolDefinition {
    static DEFINITION: OnceLock<ToolDefinition> = OnceLock::new();
    DEFINITION.get_or_init(|| ToolDefinition {
        name: "terminal".into(),
        description: concat!(
            "Manages long-lived PTY sessions for interactive REPLs/debuggers.\n\n",
            "When NOT to use:\n",
            "- One-shot commands → `shell`\n\n",
            "Tips:\n",
            "- Lifecycle: `start` → `send`/`read` → `close`. `list` shows active sessions.\n",
            "- Always `close` when finished. Use `waitMs` (up to 10000, default 100) for slow output.\n",
            "- Output is a UTF-8 lossy view of raw PTY bytes (may include ANSI escape codes).",
        )
            .into(),
        origin: ToolOrigin::Builtin,
        execution_mode: ExecutionMode::Sequential,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "send", "read", "close", "list"],
                    "description": "Operation to perform."
                },
                "id": {
                    "type": "string",
                    "description": "Terminal id returned by 'start'. Required for send/read/close."
                },
                "command": {
                    "type": "string",
                    "description": "Executable to launch (start only). e.g. 'python', 'node', 'gdb'."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Arguments passed to the command (start only)."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for the new process (start only). Defaults to tool working_dir."
                },
                "input": {
                    "type": "string",
                    "description": "Bytes to write to stdin. Required for 'send'. Include trailing newline to submit a line."
                },
                "waitMs": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 10000,
                    "description": "Milliseconds to wait before draining output (default 100). Use longer waits when expecting slower output."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    })
}

#[cfg(test)]
mod tests {
    use astrcode_core::tool::{Tool, ToolCapabilities, ToolExecutionContext};

    use super::*;

    fn test_ctx(session_id: &str) -> ToolExecutionContext {
        ToolExecutionContext {
            session_id: session_id.into(),
            working_dir: String::new(),
            tool_call_id: None,
            event_tx: None,
            capabilities: ToolCapabilities::default(),
        }
    }

    #[tokio::test]
    async fn terminal_buffer_drops_oldest_on_overflow() {
        let mut buf = TerminalBuffer::default();
        buf.append(&vec![b'a'; MAX_BUFFER_BYTES]);
        buf.append(b"newest");
        let (text, dropped) = buf.take();
        assert_eq!(dropped, 6);
        assert_eq!(text.len(), MAX_BUFFER_BYTES);
        assert!(text.ends_with("newest"));
    }

    #[tokio::test]
    async fn terminal_start_send_read_close_lifecycle() {
        let session_id = format!("terminal-test-{}", Uuid::new_v4());
        cleanup_terminals_for_session(&session_id);
        let ctx = test_ctx(&session_id);
        let tool = TerminalTool {
            working_dir: std::env::current_dir().expect("cwd"),
        };

        // PTY spawn 直接用 CreateProcessW / execvp，不走 shell。
        // Windows: 用 cmd /c 执行单条命令后退出，避免交互式 cmd 在 ConPTY 上阻塞 close。
        let (command, args) = if cfg!(windows) {
            ("cmd.exe", vec!["/c", "echo hello"])
        } else {
            ("cat", vec![])
        };

        // start a process
        let start = tool
            .execute(
                serde_json::json!({
                    "action": "start",
                    "command": command,
                    "args": args
                }),
                &ctx,
            )
            .await
            .expect("start");
        assert!(!start.is_error, "{start:?}");
        let id = start.metadata["id"].as_str().expect("id").to_string();

        if cfg!(not(windows)) {
            // send some input
            let send = tool
                .execute(
                    serde_json::json!({
                        "action": "send",
                        "id": id,
                        "input": "hello\n"
                    }),
                    &ctx,
                )
                .await
                .expect("send");
            assert!(!send.is_error);
        }

        // read with a small wait to allow output
        let read = tool
            .execute(
                serde_json::json!({
                    "action": "read",
                    "id": id,
                    "waitMs": 500
                }),
                &ctx,
            )
            .await
            .expect("read");
        assert!(!read.is_error);
        assert!(!read.content.is_empty(), "got: {}", read.content);

        // close
        let close = tool
            .execute(
                serde_json::json!({
                    "action": "close",
                    "id": id
                }),
                &ctx,
            )
            .await
            .expect("close");
        assert!(!close.is_error);
        cleanup_terminals_for_session(&session_id);
    }

    #[tokio::test]
    async fn terminal_send_unknown_id_returns_error() {
        let tool = TerminalTool {
            working_dir: std::env::current_dir().expect("cwd"),
        };
        let result = tool
            .execute(
                serde_json::json!({
                    "action": "send",
                    "id": "term-does-not-exist",
                    "input": "x"
                }),
                &test_ctx(""),
            )
            .await;
        assert!(matches!(result, Err(ToolError::Execution(_))));
    }
}
