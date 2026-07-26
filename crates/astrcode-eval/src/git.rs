//! Git 子进程的隔离配置。

/// 创建不读取宿主全局/系统配置、也不交互请求凭据的 Git 命令。
pub(crate) fn isolated_git_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command
        .env("GIT_CONFIG_GLOBAL", null_git_config_path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn null_git_config_path() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}
