#[cfg(windows)]
use std::process::Stdio;
use std::sync::OnceLock;

use astrcode_support::shell::{ShellFamily, ShellInfo};
use regex::Regex;
use tokio::process::Command;

/// Windows 上在 POSIX shell（Git Bash / MSYS）里，将 `>nul` / `2>nul` 改写为 `/dev/null`，
/// 避免创建名为 `nul` 的 literal 文件（Windows 保留设备名）。
pub(crate) fn preprocess_shell_command(command: &str, shell: &ShellInfo) -> String {
    if !cfg!(windows) || shell.family != ShellFamily::Posix {
        return command.to_string();
    }
    static NUL_REDIRECT: OnceLock<Option<Regex>> = OnceLock::new();
    let re = NUL_REDIRECT.get_or_init(|| {
        match Regex::new(r"(\d?&?>+\s*)[Nn][Uu][Ll](\s|$|[|&;)\n\r])") {
            Ok(regex) => Some(regex),
            Err(error) => {
                tracing::error!(%error, "failed to compile nul redirect regex");
                None
            },
        }
    });
    let Some(re) = re else {
        return command.to_string();
    };
    re.replace_all(command, "${1}/dev/null${2}").into_owned()
}

/// 根据 Shell 类型构建命令行参数。
///
/// 不同 Shell 的调用方式：
/// - PowerShell: `-NoProfile -Command <cmd>`
/// - cmd: `/d /s /c <cmd>`
/// - POSIX: `-lc <cmd>`
/// - WSL: `bash -lc <cmd>`
pub(crate) fn command_args(shell: &ShellInfo, command: &str) -> Vec<String> {
    match shell.family {
        ShellFamily::PowerShell => vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            command.to_string(),
        ],
        ShellFamily::Cmd => vec![
            "/d".to_string(),
            "/s".to_string(),
            "/c".to_string(),
            command.to_string(),
        ],
        ShellFamily::Posix => vec!["-lc".to_string(), command.to_string()],
        ShellFamily::Wsl => vec!["bash".to_string(), "-lc".to_string(), command.to_string()],
    }
}

#[cfg(windows)]
pub(crate) fn hide_command_window(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn hide_command_window(_: &mut Command) {}

/// 在 Unix 上设置子进程为独立进程组（pgid == pid），
/// 以便超时时可以通过 `kill(-pgid, SIGTERM)` 杀掉整棵子进程树。
#[cfg(unix)]
pub(crate) fn setup_process_group(command: &mut Command) {
    // SAFETY: setsid() 是 async-signal-safe 的 POSIX 调用。
    // 在 fork 后 exec 前执行，让子进程成为新 session leader。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn setup_process_group(_command: &mut Command) {}

#[cfg(windows)]
pub(crate) async fn terminate_child_tree(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        return;
    };
    let mut taskkill = Command::new("taskkill.exe");
    taskkill
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    hide_command_window(&mut taskkill);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), taskkill.status()).await;
    let _ = child.start_kill();
}

/// 杀掉子进程及其整个进程组。
///
/// 先发 SIGTERM 给进程组（graceful），等 2 秒后若仍存活则 SIGKILL。
#[cfg(unix)]
pub(crate) async fn terminate_child_tree(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        return;
    };
    let pgid = pid as i32;

    // 先 SIGTERM 整个进程组
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }

    // 等待 2 秒让进程优雅退出
    if tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
        .await
        .is_ok()
    {
        return;
    }

    // 仍未退出，SIGKILL 进程组
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    let _ = child.start_kill();
}

/// 非 Unix、非 Windows 平台的 fallback。
#[cfg(all(not(unix), not(windows)))]
pub(crate) async fn terminate_child_tree(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}
