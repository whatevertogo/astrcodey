//! Cross-platform shell detection for extensions and tools.
//!
//! Automatically detects the current shell type (POSIX, PowerShell, CMD, WSL) from the running
//! environment, and also supports manual override via the `ASTRCODE_SHELL` environment variable.

use std::{env, sync::OnceLock};

/// Shell family classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFamily {
    /// POSIX-compatible shells: bash, zsh, sh, etc.
    Posix,
    /// PowerShell (Windows or cross-platform version)
    PowerShell,
    /// Windows command prompt (cmd.exe)
    Cmd,
    /// Windows Subsystem for Linux
    Wsl,
}

/// Resolved shell information.
#[derive(Debug, Clone)]
pub struct ShellInfo {
    /// Shell family classification
    pub family: ShellFamily,
    /// Shell display name
    pub name: String,
    /// Path to the shell executable
    pub path: String,
}

/// Resolve the shell currently in use.
///
/// Checks the `ASTRCODE_SHELL` environment variable first; if unset, auto-detects based on the
/// platform. The auto-detected result is cached, so detection runs only once per process.
pub fn resolve_shell() -> ShellInfo {
    // Allow override via the environment variable
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

/// Detect the shell type on Windows.
///
/// Detection order: MSYS2 session → Git Bash → PowerShell 7 (pwsh) → Windows PowerShell 5.x.
/// Looks for an actually available shell via PATH and common install paths instead of relying on
/// `PSModulePath` (which exists on almost every Windows system and cannot distinguish shell
/// types).
fn detect_windows_shell() -> ShellInfo {
    // MSYS2 / MinGW / Git Bash terminal session
    if env::var("MSYSTEM").is_ok() {
        return ShellInfo {
            family: ShellFamily::Posix,
            name: "bash (MSYS2)".into(),
            path: "bash.exe".into(),
        };
    }

    // Git Bash — common in dev environments; prefer it when present
    if let Some(path) = find_git_bash() {
        return ShellInfo {
            family: ShellFamily::Posix,
            name: "bash (Git Bash)".into(),
            path,
        };
    }

    // PowerShell 7+ (pwsh) — supports &&, cross-platform
    if let Some(path) = find_pwsh() {
        return ShellInfo {
            family: ShellFamily::PowerShell,
            name: "pwsh".into(),
            path,
        };
    }

    // Windows PowerShell 5.x — always available on modern Windows
    ShellInfo {
        family: ShellFamily::PowerShell,
        name: "powershell".into(),
        path: "powershell.exe".into(),
    }
}

/// Find an executable in PATH.
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

/// Find PowerShell 7+ (pwsh), searching PATH first and then the default install path.
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

/// Find Git Bash, checking install paths under ProgramFiles and LOCALAPPDATA.
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

/// Detect the shell type on POSIX platforms.
///
/// Uses the `SHELL` environment variable to determine whether it is zsh, bash, or generic sh.
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
    use std::sync::{Mutex, OnceLock};

    use super::*;

    #[test]
    fn test_resolve_shell_override() {
        let _guard = env_lock().lock().unwrap();
        // SAFETY: every test in this process that reads or writes this variable holds `env_lock`.
        unsafe { env::set_var("ASTRCODE_SHELL", "bash") };
        let shell = resolve_shell();
        assert_eq!(shell.family, ShellFamily::Posix);
        assert_eq!(shell.name, "bash");
        // SAFETY: every test in this process that reads or writes this variable holds `env_lock`.
        unsafe { env::remove_var("ASTRCODE_SHELL") };
    }

    #[test]
    fn test_resolve_shell_default() {
        let _guard = env_lock().lock().unwrap();
        let shell = resolve_shell();
        // Should always return valid shell information
        assert!(!shell.name.is_empty());
        assert!(!shell.path.is_empty());
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}
