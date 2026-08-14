use std::{
    cell::Cell, collections::BTreeSet, future::Future, process::Stdio, sync::Arc, time::Duration,
};

use astrcode_extension_sdk::wire::{
    FeatureName, HostInitialization, InboundInvoke, InvocationResponse, InvokeError, Peer,
    PeerInfo, PeerInvokeHandler, StdioFrameTransport, WireErrorCode,
    frame::MAX_FRAME_BYTES,
    protocol::{
        CAP_RUNTIME_PING, CONFORMANCE_HOST_ECHO, CONFORMANCE_NESTED, CONFORMANCE_STREAM,
        CONFORMANCE_UNARY, CONFORMANCE_UNKNOWN_ERROR, CONFORMANCE_WAIT_FOR_CANCEL, ErrorPayload,
        ModelStreamEvent,
    },
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
    task::AbortHandle,
};
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const BEHAVIOR_SUITE_TIMEOUT: Duration = Duration::from_secs(60);

struct DriverAbortGuard(AbortHandle);

impl Drop for DriverAbortGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct HostConformanceHandler;

#[async_trait::async_trait]
impl PeerInvokeHandler for HostConformanceHandler {
    async fn invoke(
        &self,
        invocation: InboundInvoke,
    ) -> std::result::Result<InvocationResponse, ErrorPayload> {
        if invocation.request.operation == CONFORMANCE_HOST_ECHO {
            Ok(InvocationResponse::Unary(invocation.request.input))
        } else {
            Err(ErrorPayload::new(
                WireErrorCode::UnknownCapability,
                format!(
                    "conformance host does not provide {}",
                    invocation.request.operation
                ),
            ))
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let (extension_id, command) = worker_command()?;
    run_behavior_suite(&extension_id, &command).await?;
    run_rejection_probe(&command, b"1\n{").await?;
    run_rejection_probe(&command, format!("{}\n", MAX_FRAME_BYTES + 1).as_bytes()).await?;
    println!("S5R 3.0 conformance passed");
    Ok(())
}

fn worker_command() -> Result<(String, Vec<String>)> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 4 || args[0] != "--extension-id" || args[2] != "--" {
        return Err(
            "usage: s5r-conformance --extension-id <id> -- <worker command> [args...]".into(),
        );
    }
    let extension_id = args.remove(1);
    args.drain(..2);
    if extension_id.is_empty() || args.is_empty() {
        return Err(
            "usage: s5r-conformance --extension-id <id> -- <worker command> [args...]".into(),
        );
    }
    Ok((extension_id, args))
}

fn spawn_worker(command: &[String]) -> Result<tokio::process::Child> {
    let mut child = Command::new(&command[0]);
    child
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    Ok(child.spawn()?)
}

async fn run_behavior_suite(extension_id: &str, command: &[String]) -> Result<()> {
    let mut child = spawn_worker(command)?;
    let stage = Cell::new("worker initialization");
    let result = run_with_timeout(
        &stage,
        BEHAVIOR_SUITE_TIMEOUT,
        run_behavior_suite_with_child(extension_id, &mut child, &stage),
    )
    .await;
    cleanup_failed_worker(&mut child, result).await
}

async fn run_behavior_suite_with_child(
    extension_id: &str,
    child: &mut Child,
    stage: &Cell<&'static str>,
) -> Result<()> {
    announce_check(stage, "initialize, negotiate, and activate");
    let stdin = child.stdin.take().ok_or("worker stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("worker stdout unavailable")?;
    let transport = StdioFrameTransport::new(stdin, stdout);
    let supported = BTreeSet::from([
        FeatureName::nested_invoke_v1(),
        FeatureName::model_stream_v1(),
        FeatureName::custom_event_v1(),
    ]);
    let mut initialization = HostInitialization::new("conformance-initialize", extension_id);
    initialization.supported_features = supported.clone();
    initialization.required_features = supported;
    initialization.host_operations = vec![CONFORMANCE_HOST_ECHO.into()];
    let (peer, _worker, _manifest) = Peer::new(
        transport,
        PeerInfo {
            name: "s5r-conformance".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        },
    )
    .initialize(initialization)
    .await?;
    let peer = peer.activate("conformance-activate").await?;
    let (handle, driver) = peer.into_runtime();
    let shutdown = CancellationToken::new();
    let driver_task =
        tokio::spawn(driver.run_until(Arc::new(HostConformanceHandler), shutdown.clone()));
    let _driver_abort_guard = DriverAbortGuard(driver_task.abort_handle());

    announce_check(stage, "unary invoke");
    let ping = handle.invoke(CAP_RUNTIME_PING, Value::Null).await?;
    ensure(
        ping["ok"] == true,
        "runtime ping returned an invalid response",
    )?;

    let fixture = json!({ "fixture": "echo" });
    ensure(
        handle.invoke(CONFORMANCE_UNARY, fixture.clone()).await? == fixture,
        "unary echo did not preserve its input",
    )?;

    announce_check(stage, "streaming and terminal ordering");
    let mut stream = handle
        .invoke_stream(CONFORMANCE_STREAM, fixture.clone())
        .await?;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    ensure(
        matches!(events.first(), Some(ModelStreamEvent::Started)),
        "stream did not start with started",
    )?;
    ensure(
        matches!(
            events.as_slice(),
            [
                ModelStreamEvent::Started,
                ModelStreamEvent::ContentDelta { content: first },
                ModelStreamEvent::ContentDelta { content: second },
                ModelStreamEvent::Completed { output },
            ] if first == "first" && second == "second" && output == &fixture
        ),
        "stream ordering or terminal semantics are invalid",
    )?;

    announce_check(stage, "nested invoke");
    ensure(
        handle.invoke(CONFORMANCE_NESTED, fixture.clone()).await? == fixture,
        "nested invoke did not round-trip through the host",
    )?;

    announce_check(stage, "cancellation cleanup");
    let cancel_handle = handle.clone();
    let cancelled = tokio::spawn(async move {
        cancel_handle
            .invoke(CONFORMANCE_WAIT_FOR_CANCEL, Value::Null)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancelled.abort();
    let _ = cancelled.await;
    tokio::time::timeout(
        Duration::from_secs(2),
        handle.invoke(CAP_RUNTIME_PING, Value::Null),
    )
    .await??;

    announce_check(stage, "unknown error passthrough");
    match handle.invoke(CONFORMANCE_UNKNOWN_ERROR, Value::Null).await {
        Err(InvokeError::Remote(error)) if error.code == "future_conformance_error" => {},
        result => return Err(format!("unknown error code was not preserved: {result:?}").into()),
    }

    announce_check(stage, "clean shutdown");
    shutdown.cancel();
    driver_task.await??;
    drop(handle);
    let status = tokio::time::timeout(Duration::from_secs(2), child.wait()).await??;
    ensure(
        status.success(),
        "worker did not shut down cleanly after EOF",
    )
}

async fn run_rejection_probe(command: &[String], frame: &[u8]) -> Result<()> {
    eprintln!("checking malformed or oversized frame rejection");
    let mut child = spawn_worker(command)?;
    let mut stdin = child.stdin.take().ok_or("worker stdin unavailable")?;
    stdin.write_all(frame).await?;
    stdin.shutdown().await?;
    let status = tokio::time::timeout(Duration::from_secs(2), child.wait()).await??;
    ensure(
        !status.success(),
        "worker accepted a malformed or oversized frame",
    )
}

fn announce_check(stage: &Cell<&'static str>, check: &'static str) {
    stage.set(check);
    eprintln!("checking {check}");
}

async fn run_with_timeout(
    stage: &Cell<&'static str>,
    timeout: Duration,
    future: impl Future<Output = Result<()>>,
) -> Result<()> {
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "S5R conformance timed out during {} after {timeout:?}",
            stage.get()
        )
        .into()),
    }
}

async fn cleanup_failed_worker(child: &mut Child, result: Result<()>) -> Result<()> {
    let Err(error) = result else {
        return Ok(());
    };
    if let Err(cleanup_error) = kill_and_reap_worker(child).await {
        return Err(format!("{error}; failed to kill and reap worker: {cleanup_error}").into());
    }
    Err(error)
}

async fn kill_and_reap_worker(child: &mut Child) -> std::io::Result<()> {
    if child.try_wait()?.is_none() {
        child.kill().await?;
    }
    Ok(())
}

fn ensure(condition: bool, message: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, future::pending};

    use super::*;

    #[tokio::test]
    async fn timeout_reports_stage_and_worker_cleanup_reaps_child() {
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let mut child = Command::new(rustc)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        assert!(child.try_wait().unwrap().is_none());

        let stage = Cell::new("test stalled stage");
        let result =
            run_with_timeout(&stage, Duration::from_millis(10), pending::<Result<()>>()).await;
        let error = cleanup_failed_worker(&mut child, result).await.unwrap_err();

        assert!(error.to_string().contains("test stalled stage"));
        assert!(child.try_wait().unwrap().is_some());
    }
}
