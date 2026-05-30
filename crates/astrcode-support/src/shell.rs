//! Shell 检测，用于跨平台命令执行。
//!
//! 根据运行环境自动检测当前 Shell 类型（POSIX、PowerShell、CMD、WSL），
//! 也支持通过 `ASTRCODE_SHELL` 环境变量手动覆盖。

use std::{env, sync::OnceLock};

/// Shell 家族分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFamily {
    /// POSIX 兼容 Shell：bash、zsh、sh 等
    Posix,
    /// PowerShell（Windows 或跨平台版本）
    PowerShell,
    /// Windows 命令提示符（cmd.exe）
    Cmd,
    /// Windows Subsystem for Linux
    Wsl,
}

/// 解析后的 Shell 信息。
#[derive(Debug, Clone)]
pub struct ShellInfo {
    /// Shell 家族分类
    pub family: ShellFamily,
    /// Shell 显示名称
    pub name: String,
    /// Shell 可执行文件路径
    pub path: String,
}

/// 解析当前使用的 Shell。
///
/// 优先检查 `ASTRCODE_SHELL` 环境变量，如果未设置则根据平台自动检测。
/// 自动检测结果会被缓存，同一进程内只检测一次。
pub fn resolve_shell() -> ShellInfo {
    // 允许通过环境变量覆盖
    if let Ok(override_shell) = env::var("ASTRCODE_SHELL") {
        return match override_shell.to_lowercase().as_str() {
            "bash" | "zsh" | "sh" => ShellInfo {
                family: ShellFamily::Posix,
                name: override_shell.clone(),
                path: override_shell,
            },
            "pwsh" => ShellInfo {
                family: ShellFamily::PowerShell,
                name: "pwsh".into(),
                path: "pwsh.exe".into(),
            },
            "powershell" => ShellInfo {
                family: ShellFamily::PowerShell,
                name: "powershell".into(),
                path: "powershell.exe".into(),
            },
            "cmd" => ShellInfo {
                family: ShellFamily::Cmd,
                name: "cmd".into(),
                path: "cmd.exe".into(),
            },
            _ => ShellInfo {
                family: ShellFamily::Posix,
                name: override_shell.clone(),
                path: override_shell,
            },
        };
    }

    CACHED_SHELL.get_or_init(detect_shell).clone()
}

static CACHED_SHELL: OnceLock<ShellInfo> = OnceLock::new();

fn detect_shell() -> ShellInfo {
    if cfg!(windows) {
        detect_windows_shell()
    } else {
        detect_posix_shell()
    }
}

/// 在 Windows 平台上检测 Shell 类型。
///
/// 检测顺序：MSYS2 会话 → Git Bash → PowerShell 7 (pwsh) → Windows PowerShell 5.x。
/// 通过 PATH 和常见安装路径查找实际可用的 shell，而非依赖 `PSModulePath`
/// （该变量在几乎所有 Windows 系统上都存在，无法区分 shell 类型）。
fn detect_windows_shell() -> ShellInfo {
    // MSYS2 / MinGW / Git Bash 终端会话
    if env::var("MSYSTEM").is_ok() {
        return ShellInfo {
            family: ShellFamily::Posix,
            name: "bash (MSYS2)".into(),
            path: "bash.exe".into(),
        };
    }

    // Git Bash — 常见开发环境，优先使用
    if let Some(path) = find_git_bash() {
        return ShellInfo {
            family: ShellFamily::Posix,
            name: "bash (Git Bash)".into(),
            path,
        };
    }

    // PowerShell 7+ (pwsh) — 支持 &&，跨平台
    if let Some(path) = find_pwsh() {
        return ShellInfo {
            family: ShellFamily::PowerShell,
            name: "pwsh".into(),
            path,
        };
    }

    // Windows PowerShell 5.x — 现代版本 Windows 始终可用
    ShellInfo {
        family: ShellFamily::PowerShell,
        name: "powershell".into(),
        path: "powershell.exe".into(),
    }
}

static CACHED_GH_CLI: OnceLock<bool> = OnceLock::new();

/// 检测 GitHub CLI (`gh`) 是否在 PATH 中可用。
///
/// 结果在同一进程内缓存，避免重复扫描 PATH。
pub fn is_gh_cli_available() -> bool {
    *CACHED_GH_CLI.get_or_init(|| {
        if cfg!(windows) {
            find_in_path("gh.exe").is_some() || find_in_path("gh").is_some()
        } else {
            find_in_path("gh").is_some()
        }
    })
}

/// 在 PATH 中查找可执行文件。
fn find_in_path(name: &str) -> Option<String> {
    let path_var = env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path_var) {
        let full = dir.join(name);
        if full.exists() {
            return Some(full.to_string_lossy().into_owned());
        }
    }
    None
}

/// 查找 PowerShell 7+ (pwsh)，先搜 PATH 再查默认安装路径。
fn find_pwsh() -> Option<String> {
    if let Some(p) = find_in_path("pwsh.exe") {
        return Some(p);
    }
    env::var("ProgramFiles").ok().and_then(|pf| {
        let p = std::path::Path::new(&pf)
            .join("PowerShell")
            .join("7")
            .join("pwsh.exe");
        p.exists().then(|| p.to_string_lossy().into_owned())
    })
}

/// 查找 Git Bash，检查 ProgramFiles 和 LOCALAPPDATA 下的安装路径。
fn find_git_bash() -> Option<String> {
    for var in &["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Ok(pf) = env::var(var) {
            let p = std::path::Path::new(&pf)
                .join("Git")
                .join("bin")
                .join("bash.exe");
            if p.exists() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    if let Ok(local) = env::var("LOCALAPPDATA") {
        let p = std::path::Path::new(&local)
            .join("Programs")
            .join("Git")
            .join("bin")
            .join("bash.exe");
        if p.exists() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

/// 在 POSIX 平台上检测 Shell 类型。
///
/// 通过 `SHELL` 环境变量判断具体是 zsh、bash 还是通用 sh。
fn detect_posix_shell() -> ShellInfo {
    let shell_path = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let name = if shell_path.contains("zsh") {
        "zsh"
    } else if shell_path.contains("bash") {
        "bash"
    } else {
        "sh"
    };
    ShellInfo {
        family: ShellFamily::Posix,
        name: name.into(),
        path: shell_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_shell_override() {
        env::set_var("ASTRCODE_SHELL", "bash");
        let shell = resolve_shell();
        assert_eq!(shell.family, ShellFamily::Posix);
        assert_eq!(shell.name, "bash");
        env::remove_var("ASTRCODE_SHELL");
    }

    #[test]
    fn test_resolve_shell_default() {
        let shell = resolve_shell();
        // 应始终返回有效的 Shell 信息
        assert!(!shell.name.is_empty());
        assert!(!shell.path.is_empty());
    }
}
