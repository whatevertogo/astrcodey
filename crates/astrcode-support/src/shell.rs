//! Shell detection for cross-platform command execution.

use std::env;

/// Shell family classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFamily {
    /// POSIX-compatible shells: bash, zsh, sh, etc.
    Posix,
    /// PowerShell (Windows or cross-platform).
    PowerShell,
    /// Windows Command Prompt.
    Cmd,
    /// Windows Subsystem for Linux.
    Wsl,
}

/// Resolved shell information.
#[derive(Debug, Clone)]
pub struct ShellInfo {
    /// The shell family.
    pub family: ShellFamily,
    /// Shell name for display.
    pub name: String,
    /// Shell executable path.
    pub path: String,
}

/// Resolve the current shell.
///
/// Checks ASTRCODE_SHELL env var first, then auto-detects.
pub fn resolve_shell() -> ShellInfo {
    // Allow override via environment variable
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

    // Auto-detect based on platform
    if cfg!(windows) {
        detect_windows_shell()
    } else {
        detect_posix_shell()
    }
}

fn detect_windows_shell() -> ShellInfo {
    // Check for MSYS2 / MinGW / Git Bash
    if env::var("MSYSTEM").is_ok() {
        return ShellInfo {
            family: ShellFamily::Posix,
            name: "bash (MSYS2)".into(),
            path: "bash.exe".into(),
        };
    }
    // Check for PowerShell
    if env::var("PSModulePath").is_ok() {
        return ShellInfo {
            family: ShellFamily::PowerShell,
            name: "powershell".into(),
            path: "powershell.exe".into(),
        };
    }
    // Fall back to cmd
    ShellInfo {
        family: ShellFamily::Cmd,
        name: "cmd".into(),
        path: "cmd.exe".into(),
    }
}

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
        // Should always return something valid
        assert!(!shell.name.is_empty());
        assert!(!shell.path.is_empty());
    }
}
