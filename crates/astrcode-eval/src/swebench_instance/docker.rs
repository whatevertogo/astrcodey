//! Docker execution and owned container/network cleanup for one evaluation case.

use std::{
    process::{Output, Stdio},
    time::Duration,
};

use tokio::process::Command;

use crate::EvalError;

pub(super) async fn docker_checked<const N: usize>(args: [&str; N]) -> Result<(), EvalError> {
    let output = docker_output(args).await?;
    stdout(&output).map(|_| ())
}

pub(super) async fn docker_output<I, S>(args: I) -> Result<Output, EvalError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(docker_io_error)
}

pub(super) async fn docker_output_with_timeout<I, S>(
    args: I,
    timeout: Duration,
) -> Result<Output, EvalError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("docker");
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| EvalError::Setup(format!("Docker command exceeded {timeout:?}")))?
        .map_err(docker_io_error)
}

pub(super) fn stdout(output: &Output) -> Result<&str, EvalError> {
    if !output.status.success() {
        return Err(EvalError::Setup(format!(
            "Docker command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    std::str::from_utf8(&output.stdout)
        .map_err(|error| EvalError::Other(format!("Docker output is not UTF-8: {error}")))
}

pub(super) fn docker_io_error(error: std::io::Error) -> EvalError {
    EvalError::Setup(format!("start Docker CLI: {error}"))
}

pub(super) struct ContainerGuard {
    solver_name: String,
    pub(super) names: Vec<String>,
    network_name: String,
    provider_gateway_container: String,
    removed: bool,
}

impl ContainerGuard {
    pub(super) fn new(
        solver_name: &str,
        network_name: &str,
        provider_gateway_container: &str,
    ) -> Self {
        Self {
            solver_name: solver_name.to_string(),
            names: Vec::new(),
            network_name: network_name.to_string(),
            provider_gateway_container: provider_gateway_container.to_string(),
            removed: false,
        }
    }

    pub(super) async fn stop_solver(&self) {
        if let Err(error) = docker_checked(["stop", "--time", "30", &self.solver_name]).await {
            tracing::warn!(%error, container = %self.solver_name, "failed to stop instance server gracefully");
        }
    }

    pub(super) async fn remove(&mut self) {
        let mut removed_all = true;
        for name in self.names.iter().rev() {
            if let Err(error) = docker_checked(["rm", "--force", name]).await {
                removed_all = false;
                tracing::warn!(%error, %name, "failed to remove SWE-bench container");
            }
        }
        if let Err(error) = docker_checked([
            "network",
            "disconnect",
            &self.network_name,
            &self.provider_gateway_container,
        ])
        .await
        {
            removed_all = false;
            tracing::warn!(%error, network = %self.network_name, "failed to disconnect provider gateway");
        }
        if let Err(error) = docker_checked(["network", "rm", &self.network_name]).await {
            removed_all = false;
            tracing::warn!(%error, network = %self.network_name, "failed to remove case network");
        }
        self.removed = removed_all;
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if !self.removed {
            for name in self.names.iter().rev() {
                let _ = std::process::Command::new("docker")
                    .args(["rm", "--force", name])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = std::process::Command::new("docker")
                .args([
                    "network",
                    "disconnect",
                    &self.network_name,
                    &self.provider_gateway_container,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = std::process::Command::new("docker")
                .args(["network", "rm", &self.network_name])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
