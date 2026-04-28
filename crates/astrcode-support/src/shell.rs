//! Shell 检测，用于跨平台命令执行。
//!
//! 根据运行环境自动检测当前 Shell 类型（POSIX、PowerShell、CMD、WSL），
//! 也支持通过 `ASTRCODE_SHELL` 环境变量手动覆盖。

use std::env;

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
pub fn resolve_shell() -> ShellInfo {
    // 允许通过环境变量覆盖
    if let Ok(override_shell) = env::var("ASTRCODE_SHELL") {
        return match override_shell.to_lowercase().as_str() {
            "bash" | "zsh" | "sh" => ShellInfo {
                family: ShellFamily::Posix,
                name: override_shell.clone(),
                path: override_shell,
            },
            "powershell" | "pwsh" => ShellInfo {
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

    // 根据平台自动检测
    if cfg!(windows) {
        detect_windows_shell()
    } else {
        detect_posix_shell()
    }
}

/// 在 Windows 平台上检测 Shell 类型。
///
/// 检测顺序：MSYS2/MinGW/Git Bash → PowerShell → cmd（默认回退）。
fn detect_windows_shell() -> ShellInfo {
    // 检测 MSYS2 / MinGW / Git Bash
    if env::var("MSYSTEM").is_ok() {
        return ShellInfo {
            family: ShellFamily::Posix,
            name: "bash (MSYS2)".into(),
            path: "bash.exe".into(),
        };
    }
    // 检测 PowerShell
    if env::var("PSModulePath").is_ok() {
        return ShellInfo {
            family: ShellFamily::PowerShell,
            name: "powershell".into(),
            path: "powershell.exe".into(),
        };
    }
    // 回退到 cmd
    ShellInfo {
        family: ShellFamily::Cmd,
        name: "cmd".into(),
        path: "cmd.exe".into(),
    }
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
