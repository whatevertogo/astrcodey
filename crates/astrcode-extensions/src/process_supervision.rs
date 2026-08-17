//! Cross-platform ownership of a spawned child process tree.

use std::{future::Future, io, process::ExitStatus, time::Duration};

use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

const FORCE_KILL_WAIT: Duration = Duration::from_secs(2);

/// A command configured so its child process tree remains owned by the host.
pub(crate) struct SupervisedCommand {
    #[cfg(windows)]
    inner: process_wrap::tokio::CommandWrap,
    #[cfg(not(windows))]
    inner: Command,
}

impl SupervisedCommand {
    pub(crate) fn new(command: Command) -> Self {
        #[cfg(not(windows))]
        let mut command = command;

        #[cfg(unix)]
        configure_unix_child(&mut command);

        #[cfg(windows)]
        {
            use process_wrap::tokio::{JobObject, KillOnDrop};

            let mut inner = process_wrap::tokio::CommandWrap::from(command);
            inner.wrap(KillOnDrop).wrap(JobObject);
            Self { inner }
        }

        #[cfg(not(windows))]
        {
            command.kill_on_drop(true);
            Self { inner: command }
        }
    }

    pub(crate) fn spawn(mut self) -> io::Result<SupervisedChild> {
        #[cfg(windows)]
        {
            let inner = self.inner.spawn()?;
            Ok(SupervisedChild { inner })
        }

        #[cfg(not(windows))]
        {
            let inner = self.inner.spawn()?;
            #[cfg(unix)]
            let process_group_id = inner
                .id()
                .map(libc::pid_t::try_from)
                .transpose()
                .map_err(|_| io::Error::other("child process id exceeds pid_t"))?;
            Ok(SupervisedChild {
                inner,
                #[cfg(unix)]
                process_group_id,
            })
        }
    }
}

/// Owns the direct child and the platform process-tree primitive attached to it.
pub(crate) struct SupervisedChild {
    #[cfg(windows)]
    inner: Box<dyn process_wrap::tokio::ChildWrapper>,
    #[cfg(not(windows))]
    inner: tokio::process::Child,
    #[cfg(unix)]
    process_group_id: Option<libc::pid_t>,
}

impl SupervisedChild {
    #[cfg(test)]
    pub(crate) fn id(&self) -> Option<u32> {
        #[cfg(windows)]
        {
            self.inner.id()
        }

        #[cfg(not(windows))]
        {
            self.inner.id()
        }
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        #[cfg(windows)]
        {
            self.inner.stdin().take()
        }

        #[cfg(not(windows))]
        {
            self.inner.stdin.take()
        }
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        #[cfg(windows)]
        {
            self.inner.stdout().take()
        }

        #[cfg(not(windows))]
        {
            self.inner.stdout.take()
        }
    }

    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        #[cfg(windows)]
        {
            self.inner.stderr().take()
        }

        #[cfg(not(windows))]
        {
            self.inner.stderr.take()
        }
    }

    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.inner.wait().await
    }

    /// Terminates the complete child process tree and waits for the direct child to be reaped.
    pub(crate) async fn terminate(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        {
            self.terminate_unix(grace).await
        }

        #[cfg(not(unix))]
        {
            let _ = grace;
            self.inner.start_kill()?;
            wait_with_timeout(self.inner.wait(), FORCE_KILL_WAIT).await
        }
    }

    #[cfg(unix)]
    async fn terminate_unix(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        let Some(process_group_id) = self.process_group_id else {
            return self.inner.wait().await;
        };

        signal_process_group(process_group_id, libc::SIGTERM)?;
        let deadline = tokio::time::Instant::now() + grace;
        let mut direct_status = None;
        loop {
            if direct_status.is_none() {
                direct_status = self.inner.try_wait()?;
            }
            if !process_group_exists(process_group_id) {
                self.process_group_id = None;
                return match direct_status {
                    Some(status) => Ok(status),
                    None => self.inner.wait().await,
                };
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        signal_process_group(process_group_id, libc::SIGKILL)?;
        self.process_group_id = None;
        match direct_status {
            Some(status) => Ok(status),
            None => wait_with_timeout(self.inner.wait(), FORCE_KILL_WAIT).await,
        }
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(process_group_id) = self.process_group_id.take() {
            let _ = signal_process_group(process_group_id, libc::SIGKILL);
        }

        let _ = self.inner.start_kill();
    }
}

async fn wait_with_timeout<F>(wait: F, timeout: Duration) -> io::Result<ExitStatus>
where
    F: Future<Output = io::Result<ExitStatus>>,
{
    tokio::time::timeout(timeout, wait)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "child did not exit after kill"))?
}

#[cfg(unix)]
fn configure_unix_child(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.as_std_mut().process_group(0);

    #[cfg(target_os = "linux")]
    {
        // SAFETY: getpid has no preconditions.
        let parent_pid = unsafe { libc::getpid() };

        // SAFETY: only async-signal-safe libc calls run after fork and before exec.
        unsafe {
            command.as_std_mut().pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    libc::raise(libc::SIGTERM);
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }
}

#[cfg(unix)]
fn signal_process_group(process_group_id: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: the child was spawned as the leader of this process group.
    let result = unsafe { libc::kill(-process_group_id, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn process_group_exists(process_group_id: libc::pid_t) -> bool {
    // SAFETY: signal 0 performs existence and permission checking without delivering a signal.
    let result = unsafe { libc::kill(-process_group_id, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::process::Stdio;

    #[cfg(unix)]
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    async fn spawn_process_tree() -> (SupervisedChild, libc::pid_t) {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "sleep 30 & descendant=$!; printf '%s\\n' \"$descendant\"; wait",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = SupervisedCommand::new(command).spawn().unwrap();
        let child_pid = libc::pid_t::try_from(child.id().unwrap()).unwrap();
        let mut descendant = String::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            BufReader::new(child.take_stdout().unwrap()).read_line(&mut descendant),
        )
        .await
        .unwrap()
        .unwrap();
        let descendant_pid = descendant.trim().parse().unwrap();

        // SAFETY: both PIDs were reported by processes owned by this test.
        assert_eq!(unsafe { libc::getpgid(child_pid) }, child_pid);
        // SAFETY: both PIDs were reported by processes owned by this test.
        assert_eq!(unsafe { libc::getpgid(descendant_pid) }, child_pid);
        (child, descendant_pid)
    }

    #[cfg(unix)]
    async fn assert_process_exits(pid: libc::pid_t) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while process_exists(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant process should exit");
    }

    #[cfg(unix)]
    fn process_exists(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 performs existence and permission checking without delivering a signal.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_and_drop_kill_the_owned_process_group() {
        let (mut child, descendant_pid) = spawn_process_tree().await;
        child.terminate(Duration::from_millis(50)).await.unwrap();
        assert_process_exits(descendant_pid).await;

        let (child, descendant_pid) = spawn_process_tree().await;
        drop(child);
        assert_process_exits(descendant_pid).await;
    }
}
