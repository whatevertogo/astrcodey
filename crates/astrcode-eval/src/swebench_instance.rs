//! 在官方 SWE-bench instance image 中运行单个求解 session。

use std::{
    collections::HashSet,
    fs::File,
    io::Read,
    path::Path,
    process::{Output, Stdio},
    time::{Duration, SystemTime},
};

use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

use crate::{
    EvalError, SweBenchInstanceConfig, SweBenchStreamingHarnessConfig,
    case::{EvalCase, JudgeConfig, Setup},
    client::EvalClient,
    report::SweBenchPrediction,
};

const INSTANCE_WORKDIR: &str = "/testbed";
const SERVER_PORT: &str = "3847/tcp";
const SERVER_AUTH_TOKEN: &str = "astrcode-swebench-local";
const PREDICTION_MODEL_NAME: &str = "astrcode-eval-deepseek-v4-flash";
const PROVIDER_GATEWAY_URL: &str = "http://astrcode-swebench-egress:8080";
const PROVIDER_GATEWAY_PLACEHOLDER_KEY: &str = "gateway-managed";
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_GIT_STDERR_BYTES: usize = 1024 * 1024;
const IMAGE_PULL_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const IMAGE_PULL_RETRY_DELAY: Duration = Duration::from_secs(15);

fn streaming_harness_image_cache_args() -> [&'static str; 4] {
    ["--cache_level", "instance", "--clean", "false"]
}

pub(crate) struct InstanceOutcome {
    pub session_id: String,
    pub prediction: SweBenchPrediction,
    pub has_patch: bool,
}

pub(crate) async fn validate(config: &SweBenchInstanceConfig) -> Result<(), EvalError> {
    validate_file(&config.solver_binary, "solver binary")?;
    validate_file(&config.server_config, "server config")?;
    validate_server_config(&config.server_config)?;

    let control = docker_output([
        "network",
        "inspect",
        &config.control_network,
        "--format",
        "{{.Internal}} {{index .Options \"com.docker.network.bridge.enable_ip_masquerade\"}}",
    ])
    .await?;
    let control_settings = stdout(&control)?.trim().to_string();
    if control_settings != "false false" {
        return Err(EvalError::Setup(format!(
            "control network {} must have internal=false and IP masquerade=false; got \
             {control_settings:?}",
            config.control_network
        )));
    }

    if !container_is_running(&config.provider_gateway_container).await? {
        return Err(EvalError::Setup(format!(
            "provider gateway container {} is not running",
            config.provider_gateway_container
        )));
    }
    if let Some(harness) = config.streaming_harness.as_ref() {
        validate_file(&harness.python, "streaming harness Python")?;
        if harness.dataset_name.trim().is_empty()
            || harness.split.trim().is_empty()
            || harness.run_id.trim().is_empty()
            || harness.timeout_secs == 0
        {
            return Err(EvalError::Setup(
                "streaming harness dataset, split, run ID, and timeout must be configured".into(),
            ));
        }
    }
    std::fs::create_dir_all(&config.audit_dir)
        .map_err(|error| EvalError::Setup(format!("create instance audit directory: {error}")))?;
    Ok(())
}

pub(crate) async fn run_case(
    case: &EvalCase,
    config: &SweBenchInstanceConfig,
) -> Result<InstanceOutcome, EvalError> {
    let instance_id = swe_bench_instance_id(case)?;
    validate_instance_id(instance_id)?;
    let case_audit_dir = config.audit_dir.join(instance_id);
    ensure_path_does_not_exist(&case_audit_dir, "case audit directory")?;
    let solver_name = container_name(&config.container_prefix, instance_id);
    let relay_name = format!("{solver_name}-relay");
    let network_name = format!("{solver_name}-internal");
    ensure_container_does_not_exist(&solver_name).await?;
    ensure_container_does_not_exist(&relay_name).await?;
    ensure_network_does_not_exist(&network_name).await?;

    let image = official_image_name(&config.image_namespace, instance_id);
    pull_image(&image).await?;
    let result = run_case_in_image(
        case,
        config,
        &image,
        &case_audit_dir,
        &solver_name,
        &relay_name,
        &network_name,
    )
    .await;
    if let (Ok(outcome), Some(harness)) = (&result, config.streaming_harness.as_ref()) {
        match run_streaming_harness(
            instance_id,
            &outcome.prediction,
            &case_audit_dir,
            &config.image_namespace,
            harness,
        )
        .await
        {
            Ok(resolved) => {
                tracing::info!(
                    instance_id,
                    resolved,
                    "official streaming harness completed"
                );
            },
            Err(error) => {
                record_streaming_harness_failure(&case_audit_dir, &error);
                tracing::error!(
                    instance_id,
                    %error,
                    "official streaming harness failed; preserving prediction for audited regrade"
                );
            },
        }
    }
    remove_instance_image(&image).await;
    result
}

async fn run_streaming_harness(
    instance_id: &str,
    prediction: &SweBenchPrediction,
    case_audit_dir: &Path,
    image_namespace: &str,
    config: &SweBenchStreamingHarnessConfig,
) -> Result<bool, EvalError> {
    let harness_dir = case_audit_dir.join("harness");
    std::fs::create_dir_all(&harness_dir).map_err(|error| {
        EvalError::Setup(format!("create streaming harness directory: {error}"))
    })?;
    let prediction_path = harness_dir.join("prediction.jsonl");
    let mut prediction_json = serde_json::to_vec(prediction)
        .map_err(|error| EvalError::Other(format!("serialize streaming prediction: {error}")))?;
    prediction_json.push(b'\n');
    std::fs::write(&prediction_path, prediction_json)
        .map_err(|error| EvalError::Setup(format!("write streaming prediction: {error}")))?;

    let python = absolute_path_preserving_symlinks(&config.python)?;
    let prediction_path = std::fs::canonicalize(&prediction_path)
        .map_err(|error| EvalError::Setup(format!("resolve streaming prediction path: {error}")))?;
    let harness_timeout = Duration::from_secs(config.timeout_secs.saturating_add(600));
    let timeout_secs = config.timeout_secs.to_string();
    let mut command = Command::new(python);
    command
        .args([
            "-m",
            "swebench.harness.run_evaluation",
            "--dataset_name",
            &config.dataset_name,
            "--split",
            &config.split,
            "--instance_ids",
            instance_id,
            "--predictions_path",
        ])
        .arg(&prediction_path)
        .args([
            "--max_workers",
            "1",
            "--timeout",
            &timeout_secs,
        ])
        // The evaluator removes this case's exact instance image after scoring.
        // Disabling harness-wide cleanup prevents concurrent cases from deleting
        // images that another solver or harness still needs.
        .args(streaming_harness_image_cache_args())
        .args(["--run_id", &config.run_id, "--namespace", image_namespace])
        .current_dir(&harness_dir)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(harness_timeout, command.output())
        .await
        .map_err(|_| {
            EvalError::Setup(format!(
                "official streaming harness exceeded {harness_timeout:?}"
            ))
        })?
        .map_err(|error| EvalError::Setup(format!("start official streaming harness: {error}")))?;
    std::fs::write(harness_dir.join("stdout.log"), &output.stdout)
        .map_err(|error| EvalError::Setup(format!("write harness stdout: {error}")))?;
    std::fs::write(harness_dir.join("stderr.log"), &output.stderr)
        .map_err(|error| EvalError::Setup(format!("write harness stderr: {error}")))?;
    if !output.status.success() {
        return Err(EvalError::Setup(format!(
            "official streaming harness exited with {}",
            output.status
        )));
    }

    let report_path = streaming_harness_report_path(&harness_dir, instance_id, prediction, config);
    let resolved = read_streaming_harness_result(&report_path, instance_id)?;
    let status = serde_json::json!({
        "instance_id": instance_id,
        "resolved": resolved,
        "report_path": report_path,
    });
    std::fs::write(
        harness_dir.join("status.json"),
        serde_json::to_vec_pretty(&status)
            .map_err(|error| EvalError::Other(format!("serialize harness status: {error}")))?,
    )
    .map_err(|error| EvalError::Setup(format!("write harness status: {error}")))?;
    Ok(resolved)
}

fn absolute_path_preserving_symlinks(path: &Path) -> Result<std::path::PathBuf, EvalError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|current_dir| current_dir.join(path))
        .map_err(|error| {
            EvalError::Setup(format!(
                "resolve absolute path without following symlinks for {}: {error}",
                path.display()
            ))
        })
}

fn streaming_harness_report_path(
    harness_dir: &Path,
    instance_id: &str,
    prediction: &SweBenchPrediction,
    config: &SweBenchStreamingHarnessConfig,
) -> std::path::PathBuf {
    harness_dir
        .join("logs/run_evaluation")
        .join(&config.run_id)
        .join(prediction.model_name_or_path.replace('/', "__"))
        .join(instance_id)
        .join("report.json")
}

fn read_streaming_harness_result(report_path: &Path, instance_id: &str) -> Result<bool, EvalError> {
    let content = std::fs::read(report_path).map_err(|error| {
        EvalError::Setup(format!(
            "read official harness report {}: {error}",
            report_path.display()
        ))
    })?;
    let report: serde_json::Value = serde_json::from_slice(&content)
        .map_err(|error| EvalError::Other(format!("parse official harness report: {error}")))?;
    report
        .get(instance_id)
        .and_then(|result| result.get("resolved"))
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| {
            EvalError::Other(format!(
                "official harness report lacks boolean resolved for {instance_id}"
            ))
        })
}

fn record_streaming_harness_failure(case_audit_dir: &Path, error: &EvalError) {
    let harness_dir = case_audit_dir.join("harness");
    if let Err(write_error) = std::fs::create_dir_all(&harness_dir).and_then(|_| {
        std::fs::write(
            harness_dir.join("failure.json"),
            serde_json::json!({ "error": error.to_string() }).to_string(),
        )
    }) {
        tracing::error!(%write_error, "failed to record streaming harness failure");
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_case_in_image(
    case: &EvalCase,
    config: &SweBenchInstanceConfig,
    image: &str,
    case_audit_dir: &Path,
    solver_name: &str,
    relay_name: &str,
    network_name: &str,
) -> Result<InstanceOutcome, EvalError> {
    prepare_case_state(case_audit_dir, &config.server_config)?;
    create_isolated_network(network_name).await?;
    if let Err(error) = docker_checked([
        "network",
        "connect",
        network_name,
        &config.provider_gateway_container,
    ])
    .await
    {
        let _ = docker_checked(["network", "rm", network_name]).await;
        return Err(error);
    }
    let mut containers = ContainerGuard::new(
        solver_name,
        network_name,
        &config.provider_gateway_container,
    );
    let result = async {
        write_case_metadata(case_audit_dir, image, network_name, config).await?;
        create_solver(
            solver_name,
            image,
            network_name,
            case_audit_dir,
            config,
            &mut containers,
        )
        .await?;
        create_control_relay(
            relay_name,
            solver_name,
            network_name,
            config,
            &mut containers,
        )
        .await?;
        run_session(case, solver_name, relay_name, case_audit_dir).await
    }
    .await;
    containers.stop_solver().await;
    save_server_log(solver_name, case_audit_dir).await;
    finish_case_metadata(case_audit_dir, &result);
    containers.remove().await;
    result
}

async fn run_session(
    case: &EvalCase,
    solver_name: &str,
    relay_name: &str,
    case_audit_dir: &Path,
) -> Result<InstanceOutcome, EvalError> {
    docker_checked(["start", solver_name]).await?;
    docker_checked(["start", relay_name]).await?;
    let server_addr = wait_for_server(solver_name, relay_name).await?;
    sanitize_repository_for_session(case, solver_name, case_audit_dir).await?;
    let baseline_untracked_paths = list_untracked_paths(solver_name).await?;
    let client = EvalClient::new(&server_addr, SERVER_AUTH_TOKEN)?;
    let session_id = client.create_session(INSTANCE_WORKDIR).await?;

    for prompt in &case.prompts {
        client.submit_prompt(&session_id, prompt).await?;
        client
            .wait_completion(&session_id, case.timeout_secs)
            .await?;
    }

    let model_patch = collect_model_patch(solver_name, &baseline_untracked_paths).await?;
    let has_patch = !model_patch.trim().is_empty();
    Ok(InstanceOutcome {
        session_id,
        prediction: prediction(case.id.clone(), model_patch),
        has_patch,
    })
}

async fn sanitize_repository_for_session(
    case: &EvalCase,
    container_name: &str,
    case_audit_dir: &Path,
) -> Result<(), EvalError> {
    let configured_base_commit = match &case.setup {
        Setup::Git { commit, .. } => commit,
        _ => {
            return Err(EvalError::Setup(format!(
                "{}: official instance case does not declare a Git base commit",
                case.id
            )));
        },
    };
    let base_revision = format!("{configured_base_commit}^{{commit}}");
    let base_commit = git_capture(container_name, &["rev-parse", &base_revision], false)
        .await?
        .trim()
        .to_string();
    let original_head = git_capture(container_name, &["rev-parse", "HEAD"], false)
        .await?
        .trim()
        .to_string();
    verify_base_is_ancestor(container_name, &base_commit).await?;

    let tracked_status = git_capture_output(
        container_name,
        &["status", "--porcelain=v1", "-uno", "-z"],
        false,
    )
    .await?;
    git_capture(container_name, &["add", "--update"], false).await?;
    let baseline_tree = git_capture(container_name, &["write-tree"], false)
        .await?
        .trim()
        .to_string();
    let baseline_head = git_capture(
        container_name,
        &[
            "-c",
            "user.name=Astrcode SWE-bench",
            "-c",
            "user.email=astrcode-swebench@invalid",
            "commit-tree",
            &baseline_tree,
            "-m",
            "Astrcode SWE-bench clean session baseline",
        ],
        false,
    )
    .await?
    .trim()
    .to_string();
    git_capture(
        container_name,
        &["checkout", "--detach", &baseline_head],
        false,
    )
    .await?;

    let refs = git_capture(
        container_name,
        &["for-each-ref", "--format=%(refname)"],
        false,
    )
    .await?;
    let refs = refs.lines().filter(|reference| !reference.is_empty());
    let mut removed_ref_count = 0_usize;
    for reference in refs {
        git_capture(container_name, &["update-ref", "-d", reference], false).await?;
        removed_ref_count += 1;
    }
    git_capture(
        container_name,
        &["reflog", "expire", "--expire=now", "--all"],
        false,
    )
    .await?;
    git_capture(container_name, &["repack", "-Ad"], false).await?;
    git_capture(container_name, &["prune-packed"], false).await?;
    git_capture(container_name, &["prune", "--expire=now"], false).await?;

    let remaining_refs = git_capture(
        container_name,
        &["for-each-ref", "--format=%(refname)"],
        false,
    )
    .await?;
    if !remaining_refs.trim().is_empty() {
        return Err(EvalError::Setup(format!(
            "{}: repository isolation left reachable refs: {}",
            case.id,
            remaining_refs.lines().collect::<Vec<_>>().join(", ")
        )));
    }
    let unreachable = git_capture(
        container_name,
        &["fsck", "--unreachable", "--no-reflogs"],
        false,
    )
    .await?;
    if !unreachable.trim().is_empty() {
        return Err(EvalError::Setup(format!(
            "{}: repository isolation left unreachable Git objects",
            case.id
        )));
    }
    verify_single_commit_history(container_name, &baseline_head).await?;

    let audit = serde_json::json!({
        "base_commit": base_commit,
        "original_head": original_head,
        "baseline_head": baseline_head,
        "baseline_had_tracked_changes": !tracked_status.is_empty(),
        "removed_ref_count": removed_ref_count,
        "remaining_ref_count": 0,
        "unreachable_object_count": 0,
        "history_commit_count": 1,
    });
    std::fs::write(
        case_audit_dir.join("repository-isolation.json"),
        serde_json::to_vec_pretty(&audit)
            .map_err(|error| EvalError::Other(format!("serialize repository audit: {error}")))?,
    )
    .map_err(EvalError::Io)
}

async fn verify_base_is_ancestor(container_name: &str, base_commit: &str) -> Result<(), EvalError> {
    let merge_base =
        git_capture(container_name, &["merge-base", base_commit, "HEAD"], false).await?;
    if merge_base.trim() != base_commit {
        return Err(EvalError::Setup(format!(
            "official instance HEAD does not descend from base commit {base_commit}"
        )));
    }
    Ok(())
}

async fn verify_single_commit_history(
    container_name: &str,
    baseline_head: &str,
) -> Result<(), EvalError> {
    let history = git_capture(container_name, &["rev-list", "--parents", "HEAD"], false).await?;
    let commits = history.lines().collect::<Vec<_>>();
    let fields = commits
        .first()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .unwrap_or_default();
    if commits.len() != 1 || fields.as_slice() != [baseline_head] {
        return Err(EvalError::Setup(
            "repository isolation did not produce a single root baseline commit".into(),
        ));
    }
    Ok(())
}

async fn create_solver(
    name: &str,
    image: &str,
    network_name: &str,
    audit_dir: &Path,
    config: &SweBenchInstanceConfig,
    containers: &mut ContainerGuard,
) -> Result<(), EvalError> {
    let state_dir = audit_dir.join("state");
    let solver_mount = bind_mount(&config.solver_binary, "/opt/astrcode", true)?;
    let state_mount = bind_mount(&state_dir, "/astrcode-state", false)?;
    let no_proxy = "127.0.0.1,localhost,astrcode-swebench-egress";
    let output = docker_output([
        "create",
        "--platform",
        "linux/amd64",
        "--name",
        name,
        "--network",
        network_name,
        "--memory",
        "3g",
        "--cpus",
        "2",
        "--pids-limit",
        "2048",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges=true",
        "--workdir",
        INSTANCE_WORKDIR,
        "--mount",
        &solver_mount,
        "--mount",
        &state_mount,
        "--env",
        "ASTRCODE_TEST_HOME=/astrcode-state",
        "--env",
        "ASTRCODE_HTTP_TOKEN=astrcode-swebench-local",
        "--env",
        "PATH=/opt/miniconda3/envs/testbed/bin:/opt/miniconda3/bin:/usr/local/sbin:/usr/local/bin:\
         /usr/sbin:/usr/bin:/sbin:/bin",
        "--env",
        "SHELL=/bin/bash",
        "--env",
        "CONDA_DEFAULT_ENV=testbed",
        "--env",
        "CONDA_PREFIX=/opt/miniconda3/envs/testbed",
        "--env",
        &format!("HTTP_PROXY={}", config.proxy_url),
        "--env",
        &format!("HTTPS_PROXY={}", config.proxy_url),
        "--env",
        &format!("NO_PROXY={no_proxy}"),
        "--env",
        "GIT_CONFIG_GLOBAL=/dev/null",
        "--env",
        "GIT_CONFIG_NOSYSTEM=1",
        "--env",
        "GIT_TERMINAL_PROMPT=0",
        "--entrypoint",
        "/opt/astrcode",
        image,
        "server",
        "--addr",
        "0.0.0.0:3847",
    ])
    .await?;
    stdout(&output)?;
    containers.names.push(name.to_string());
    Ok(())
}

async fn create_control_relay(
    relay_name: &str,
    solver_name: &str,
    network_name: &str,
    config: &SweBenchInstanceConfig,
    containers: &mut ContainerGuard,
) -> Result<(), EvalError> {
    let output = docker_output([
        "create",
        "--name",
        relay_name,
        "--network",
        &config.control_network,
        "--publish",
        "127.0.0.1::3847",
        "--memory",
        "128m",
        "--cpus",
        "0.25",
        "--pids-limit",
        "64",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges=true",
        "--read-only",
        "--tmpfs",
        "/tmp:size=16m",
        "--entrypoint",
        "python3",
        &config.control_relay_image,
        "/usr/local/bin/swebench-control-relay.py",
        solver_name,
        "3847",
    ])
    .await?;
    stdout(&output)?;
    containers.names.push(relay_name.to_string());
    docker_checked(["network", "connect", network_name, relay_name]).await
}

async fn wait_for_server(solver_name: &str, relay_name: &str) -> Result<String, EvalError> {
    let port_output = docker_output(["port", relay_name, SERVER_PORT]).await?;
    let mapping = stdout(&port_output)?.trim();
    let port = mapping
        .rsplit_once(':')
        .map(|(_, port)| port)
        .filter(|port| !port.is_empty())
        .ok_or_else(|| EvalError::Server(format!("invalid Docker port mapping: {mapping:?}")))?;
    let server_addr = format!("http://127.0.0.1:{port}");
    let client = EvalClient::new(&server_addr, SERVER_AUTH_TOKEN)?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if client.health_check().await.is_ok() {
            return Ok(server_addr);
        }
        if !container_is_running(solver_name).await? {
            return Err(EvalError::Server(format!(
                "instance server {solver_name} exited before becoming healthy"
            )));
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(EvalError::Server(format!(
        "instance server {solver_name} did not become healthy within 120 seconds"
    )))
}

async fn collect_model_patch(
    container_name: &str,
    baseline_untracked_paths: &HashSet<String>,
) -> Result<String, EvalError> {
    git_with_paths(
        container_name,
        &["reset", "--quiet", "HEAD", "--"],
        baseline_untracked_paths,
    )
    .await?;

    let current_untracked_paths = list_untracked_paths(container_name).await?;
    let new_untracked_paths =
        new_untracked_paths(baseline_untracked_paths, &current_untracked_paths);
    git_with_paths(
        container_name,
        &["add", "--intent-to-add", "--"],
        &new_untracked_paths,
    )
    .await?;

    git_capture(
        container_name,
        &["diff", "--no-ext-diff", "--binary", "HEAD", "--"],
        false,
    )
    .await
}

async fn list_untracked_paths(container_name: &str) -> Result<HashSet<String>, EvalError> {
    let bytes = git_capture_output(
        container_name,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        false,
    )
    .await?;
    parse_untracked_paths(&bytes)
}

fn parse_untracked_paths(bytes: &[u8]) -> Result<HashSet<String>, EvalError> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec()).map_err(|error| {
                EvalError::Other(format!("untracked path is not valid UTF-8: {error}"))
            })
        })
        .collect()
}

fn new_untracked_paths(baseline: &HashSet<String>, current: &HashSet<String>) -> HashSet<String> {
    current
        .difference(baseline)
        .filter(|path| !is_generated_artifact_path(path))
        .cloned()
        .collect()
}

fn is_generated_artifact_path(path: &str) -> bool {
    const GENERATED_DIRECTORIES: &[&str] = &[
        ".mypy_cache",
        ".nox",
        ".pytest_cache",
        ".ruff_cache",
        ".tox",
        ".venv",
        "__pycache__",
        "build",
        "dist",
        "node_modules",
        "target",
        "venv",
    ];

    let mut components = path.split('/');
    let file_name = components.next_back().unwrap_or(path);
    components.any(|component| GENERATED_DIRECTORIES.contains(&component))
        || GENERATED_DIRECTORIES.contains(&file_name)
        || file_name == ".DS_Store"
        || file_name == "core"
        || file_name
            .strip_prefix("core.")
            .is_some_and(|suffix| suffix.chars().all(|character| character.is_ascii_digit()))
        || file_name.ends_with(".pyc")
        || file_name.ends_with(".pyo")
}

async fn git_with_paths(
    container_name: &str,
    prefix: &[&str],
    paths: &HashSet<String>,
) -> Result<(), EvalError> {
    let mut paths = paths.iter().map(String::as_str).collect::<Vec<_>>();
    paths.sort_unstable();
    for chunk in paths.chunks(256) {
        let mut args = Vec::with_capacity(prefix.len() + chunk.len());
        args.extend_from_slice(prefix);
        args.extend_from_slice(chunk);
        git_capture(container_name, &args, false).await?;
    }
    Ok(())
}

async fn git_capture(
    container_name: &str,
    args: &[&str],
    accept_diff_exit: bool,
) -> Result<String, EvalError> {
    let bytes = git_capture_output(container_name, args, accept_diff_exit).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn git_capture_output(
    container_name: &str,
    args: &[&str],
    accept_diff_exit: bool,
) -> Result<Vec<u8>, EvalError> {
    let mut command = Command::new("docker");
    command.args([
        "exec",
        "--workdir",
        INSTANCE_WORKDIR,
        "--env",
        "GIT_CONFIG_GLOBAL=/dev/null",
        "--env",
        "GIT_CONFIG_NOSYSTEM=1",
        container_name,
        "git",
    ]);
    command.args(args);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(docker_io_error)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| EvalError::Other("failed to capture git stdout".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| EvalError::Other("failed to capture git stderr".to_string()))?;
    let mut stdout_task = tokio::spawn(read_bounded(stdout, MAX_PATCH_BYTES, "model patch"));
    let mut stderr_task = tokio::spawn(read_bounded(stderr, MAX_GIT_STDERR_BYTES, "git stderr"));

    let (status, stdout, stderr) = tokio::select! {
        stdout = &mut stdout_task => {
            let stdout = match join_capture(stdout) {
                Ok(stdout) => stdout,
                Err(error) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    stderr_task.abort();
                    return Err(error);
                },
            };
            let status = child.wait().await.map_err(docker_io_error)?;
            let stderr = join_capture(stderr_task.await)?;
            (status, stdout, stderr)
        },
        stderr = &mut stderr_task => {
            let stderr = match join_capture(stderr) {
                Ok(stderr) => stderr,
                Err(error) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    stdout_task.abort();
                    return Err(error);
                },
            };
            let status = child.wait().await.map_err(docker_io_error)?;
            let stdout = join_capture(stdout_task.await)?;
            (status, stdout, stderr)
        },
        status = child.wait() => {
            let status = status.map_err(docker_io_error)?;
            let stdout = join_capture(stdout_task.await)?;
            let stderr = join_capture(stderr_task.await)?;
            (status, stdout, stderr)
        },
    };
    let code = status.code().unwrap_or(-1);
    if status.success() || (accept_diff_exit && code == 1) {
        Ok(stdout)
    } else {
        Err(EvalError::Other(format!(
            "git command in instance failed ({code}): {}",
            String::from_utf8_lossy(&stderr)
        )))
    }
}

fn join_capture(
    result: Result<Result<Vec<u8>, EvalError>, tokio::task::JoinError>,
) -> Result<Vec<u8>, EvalError> {
    result.map_err(|error| EvalError::Other(format!("git output task failed: {error}")))?
}

async fn read_bounded<R>(
    mut reader: R,
    max_bytes: usize,
    label: &'static str,
) -> Result<Vec<u8>, EvalError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| EvalError::Other(format!("read {label}: {error}")))?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(count) > max_bytes {
            return Err(EvalError::Other(format!(
                "{label} exceeds the {max_bytes}-byte audit limit"
            )));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

async fn pull_image(image: &str) -> Result<(), EvalError> {
    if docker_output(["image", "inspect", image])
        .await?
        .status
        .success()
    {
        tracing::info!(%image, "using cached official instance image");
        return Ok(());
    }

    let mut attempt = 0_u64;
    loop {
        attempt += 1;
        match docker_output_with_timeout(
            ["pull", "--platform", "linux/amd64", image],
            IMAGE_PULL_TIMEOUT,
        )
        .await
        .and_then(|output| stdout(&output).map(|_| ()))
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::warn!(
                    %error,
                    %image,
                    attempt,
                    delay = ?IMAGE_PULL_RETRY_DELAY,
                    "official image pull failed or stalled; retrying without failing the case"
                );
                tokio::time::sleep(IMAGE_PULL_RETRY_DELAY).await;
            },
        }
    }
}

async fn remove_instance_image(image: &str) {
    let Ok(inspect) = docker_output(["image", "inspect", image]).await else {
        return;
    };
    if !inspect.status.success() {
        return;
    }
    if let Err(error) = docker_checked(["image", "rm", image]).await {
        tracing::warn!(%error, %image, "failed to remove completed instance image");
    }
}

async fn ensure_container_does_not_exist(name: &str) -> Result<(), EvalError> {
    let output = docker_output(["container", "inspect", name]).await?;
    if output.status.success() {
        Err(EvalError::Setup(format!(
            "container {name} already exists; use a fresh run ID"
        )))
    } else {
        Ok(())
    }
}

async fn ensure_network_does_not_exist(name: &str) -> Result<(), EvalError> {
    let output = docker_output(["network", "inspect", name]).await?;
    if output.status.success() {
        Err(EvalError::Setup(format!(
            "network {name} already exists; use a fresh run ID"
        )))
    } else {
        Ok(())
    }
}

async fn create_isolated_network(name: &str) -> Result<(), EvalError> {
    docker_checked(["network", "create", "--internal", name]).await
}

fn ensure_path_does_not_exist(path: &Path, label: &str) -> Result<(), EvalError> {
    if path.exists() {
        Err(EvalError::Setup(format!(
            "{label} already exists at {}; use a fresh run directory",
            path.display()
        )))
    } else {
        Ok(())
    }
}

fn prepare_case_state(audit_dir: &Path, server_config: &Path) -> Result<(), EvalError> {
    std::fs::create_dir(audit_dir).map_err(|error| {
        EvalError::Setup(format!(
            "create fresh case audit directory {}: {error}",
            audit_dir.display()
        ))
    })?;
    let config_dir = audit_dir.join("state").join(".astrcode");
    std::fs::create_dir_all(&config_dir)
        .map_err(|error| EvalError::Setup(format!("create case state: {error}")))?;
    std::fs::copy(server_config, config_dir.join("config.toml"))
        .map_err(|error| EvalError::Setup(format!("copy server config: {error}")))?;
    Ok(())
}

async fn save_server_log(container_name: &str, audit_dir: &Path) {
    let path = audit_dir.join("server.log");
    let Ok(stdout) = File::create(&path) else {
        return;
    };
    let Ok(stderr) = stdout.try_clone() else {
        return;
    };
    let result = Command::new("docker")
        .args(["logs", container_name])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status()
        .await;
    if let Err(error) = result {
        tracing::warn!(%error, %container_name, "failed to save instance server log");
    }
}

fn swe_bench_instance_id(case: &EvalCase) -> Result<&str, EvalError> {
    match case.judges.as_slice() {
        [JudgeConfig::SweBenchPatch { instance_id }] if instance_id == &case.id => Ok(instance_id),
        _ => Err(EvalError::Setup(format!(
            "{} is not an official SWE-bench prediction case",
            case.id
        ))),
    }
}

fn validate_instance_id(instance_id: &str) -> Result<(), EvalError> {
    let valid_character =
        |character: char| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-');
    let Some((repository, issue)) = instance_id.split_once("__") else {
        return Err(EvalError::Setup(format!(
            "invalid SWE-bench instance ID: {instance_id:?}"
        )));
    };
    let Some((project, issue_number)) = issue.rsplit_once('-') else {
        return Err(EvalError::Setup(format!(
            "invalid SWE-bench instance ID: {instance_id:?}"
        )));
    };
    let valid = !repository.is_empty()
        && !project.is_empty()
        && !issue_number.is_empty()
        && repository.chars().all(valid_character)
        && project.chars().all(valid_character)
        && issue_number
            .chars()
            .all(|character| character.is_ascii_digit())
        && !instance_id.contains("../")
        && !instance_id.contains("/..");
    if valid {
        Ok(())
    } else {
        Err(EvalError::Setup(format!(
            "invalid SWE-bench instance ID: {instance_id:?}"
        )))
    }
}

fn official_image_name(namespace: &str, instance_id: &str) -> String {
    let image_id = instance_id.to_ascii_lowercase().replace("__", "_1776_");
    format!("{namespace}/sweb.eval.x86_64.{image_id}:latest")
}

fn container_name(prefix: &str, instance_id: &str) -> String {
    let safe_id: String = instance_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    format!("{prefix}-{safe_id}")
}

fn bind_mount(source: &Path, target: &str, readonly: bool) -> Result<String, EvalError> {
    let source = source
        .canonicalize()
        .map_err(|error| EvalError::Setup(format!("resolve {}: {error}", source.display())))?;
    let readonly = if readonly { ",readonly" } else { "" };
    Ok(format!(
        "type=bind,src={},dst={target}{readonly}",
        source.display()
    ))
}

fn validate_file(path: &Path, label: &str) -> Result<(), EvalError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(EvalError::Setup(format!(
            "{label} not found: {}",
            path.display()
        )))
    }
}

fn validate_server_config(path: &Path) -> Result<(), EvalError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| EvalError::Setup(format!("read server config: {error}")))?;
    let config: astrcode_core::config::Config = toml::from_str(&raw)
        .map_err(|error| EvalError::Setup(format!("parse server config: {error}")))?;
    if config.active_model != "deepseek-v4-flash" {
        return Err(EvalError::Setup(
            "server config activeModel must be deepseek-v4-flash".to_string(),
        ));
    }
    if config.profiles.len() != 1 || config.extensions.is_some() {
        return Err(EvalError::Setup(
            "solver config must contain exactly one provider profile and no extensions".to_string(),
        ));
    }
    let profile = config
        .profiles
        .iter()
        .find(|profile| profile.name == config.active_profile)
        .ok_or_else(|| EvalError::Setup("active server profile was not found".to_string()))?;
    if profile.base_url != PROVIDER_GATEWAY_URL
        || profile.api_key.as_deref() != Some(PROVIDER_GATEWAY_PLACEHOLDER_KEY)
    {
        return Err(EvalError::Setup(
            "active server profile must use the trusted provider gateway and its non-secret \
             placeholder key"
                .to_string(),
        ));
    }
    if config
        .active_small_profile
        .as_deref()
        .is_some_and(|name| name != profile.name)
        || config
            .active_small_model
            .as_deref()
            .is_some_and(|model| model != "deepseek-v4-flash")
    {
        return Err(EvalError::Setup(
            "solver config small model must use the same audited provider and model".to_string(),
        ));
    }
    Ok(())
}

async fn container_is_running(name: &str) -> Result<bool, EvalError> {
    let output = docker_output(["inspect", name, "--format", "{{.State.Running}}"]).await?;
    Ok(stdout(&output)?.trim() == "true")
}

async fn write_case_metadata(
    audit_dir: &Path,
    image: &str,
    network_name: &str,
    config: &SweBenchInstanceConfig,
) -> Result<(), EvalError> {
    let instance_image_identity = image_identity(image).await?;
    let relay_image_identity = image_identity(&config.control_relay_image).await?;
    let gateway_inspect = docker_output([
        "container",
        "inspect",
        &config.provider_gateway_container,
        "--format",
        "{{.Image}}",
    ])
    .await?;
    let provider_gateway_image_id = stdout(&gateway_inspect)?.trim();
    let metadata = serde_json::json!({
        "image": image,
        "image_identity": instance_image_identity,
        "solver_binary_sha256": sha256_file(&config.solver_binary)?,
        "server_config_sha256": sha256_file(&config.server_config)?,
        "provider_network": network_name,
        "provider_gateway_container": config.provider_gateway_container,
        "provider_gateway_image_id": provider_gateway_image_id,
        "control_relay_image": config.control_relay_image,
        "control_relay_image_identity": relay_image_identity,
        "started_at_unix_ms": SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|error| EvalError::Other(format!("system time: {error}")))?
            .as_millis(),
    });
    let bytes = serde_json::to_vec_pretty(&metadata)
        .map_err(|error| EvalError::Other(format!("serialize case metadata: {error}")))?;
    std::fs::write(audit_dir.join("metadata.json"), bytes).map_err(EvalError::Io)
}

async fn image_identity(image: &str) -> Result<String, EvalError> {
    let inspect = docker_output([
        "image",
        "inspect",
        image,
        "--format",
        "{{.Id}} {{join .RepoDigests \",\"}}",
    ])
    .await?;
    Ok(stdout(&inspect)?.trim().to_string())
}

fn finish_case_metadata(audit_dir: &Path, result: &Result<InstanceOutcome, EvalError>) {
    let path = audit_dir.join("metadata.json");
    let update = (|| -> Result<(), EvalError> {
        let raw = std::fs::read_to_string(&path)?;
        let mut metadata: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|error| EvalError::Other(format!("parse case metadata: {error}")))?;
        let object = metadata
            .as_object_mut()
            .ok_or_else(|| EvalError::Other("case metadata is not an object".to_string()))?;
        object.insert(
            "finished_at_unix_ms".to_string(),
            serde_json::json!(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_err(|error| EvalError::Other(format!("system time: {error}")))?
                    .as_millis()
            ),
        );
        object.insert(
            "solve_status".to_string(),
            serde_json::Value::String(if result.is_ok() { "ok" } else { "error" }.to_string()),
        );
        let bytes = serde_json::to_vec_pretty(&metadata)
            .map_err(|error| EvalError::Other(format!("serialize case metadata: {error}")))?;
        std::fs::write(&path, bytes)?;
        Ok(())
    })();
    if let Err(error) = update {
        tracing::warn!(%error, path = %path.display(), "failed to finalize case metadata");
    }
}

fn sha256_file(path: &Path) -> Result<String, EvalError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn prediction(instance_id: String, model_patch: String) -> SweBenchPrediction {
    SweBenchPrediction {
        instance_id,
        model_name_or_path: PREDICTION_MODEL_NAME.to_string(),
        model_patch,
    }
}

async fn docker_checked<const N: usize>(args: [&str; N]) -> Result<(), EvalError> {
    let output = docker_output(args).await?;
    stdout(&output).map(|_| ())
}

async fn docker_output<I, S>(args: I) -> Result<Output, EvalError>
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

async fn docker_output_with_timeout<I, S>(args: I, timeout: Duration) -> Result<Output, EvalError>
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

fn stdout(output: &Output) -> Result<&str, EvalError> {
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

fn docker_io_error(error: std::io::Error) -> EvalError {
    EvalError::Setup(format!("start Docker CLI: {error}"))
}

struct ContainerGuard {
    solver_name: String,
    names: Vec<String>,
    network_name: String,
    provider_gateway_container: String,
    removed: bool,
}

impl ContainerGuard {
    fn new(solver_name: &str, network_name: &str, provider_gateway_container: &str) -> Self {
        Self {
            solver_name: solver_name.to_string(),
            names: Vec::new(),
            network_name: network_name.to_string(),
            provider_gateway_container: provider_gateway_container.to_string(),
            removed: false,
        }
    }

    async fn stop_solver(&self) {
        if let Err(error) = docker_checked(["stop", "--time", "30", &self.solver_name]).await {
            tracing::warn!(%error, container = %self.solver_name, "failed to stop instance server gracefully");
        }
    }

    async fn remove(&mut self) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_image_and_container_names_are_deterministic() {
        assert_eq!(
            official_image_name("swebench", "django__django-11620"),
            "swebench/sweb.eval.x86_64.django_1776_django-11620:latest"
        );
        assert_eq!(
            container_name("astrcode-run", "django__django-11620"),
            "astrcode-run-django__django-11620"
        );
    }

    #[test]
    fn official_image_pull_has_bounded_attempt_duration() {
        assert_eq!(IMAGE_PULL_TIMEOUT, Duration::from_secs(60 * 60));
        assert_eq!(IMAGE_PULL_RETRY_DELAY, Duration::from_secs(15));
    }

    #[test]
    fn patch_collection_includes_only_untracked_paths_created_during_session() {
        let baseline = parse_untracked_paths(b"build/lib/generated.py\0cache file\0").unwrap();
        let current = parse_untracked_paths(
            b"build/lib/generated.py\0cache file\0new source.py\0new/module.py\0\
              generated/build/output.py\0core.123\0package/__pycache__/module.pyc\0",
        )
        .unwrap();

        assert_eq!(
            new_untracked_paths(&baseline, &current),
            HashSet::from(["new source.py".to_string(), "new/module.py".to_string()])
        );
    }

    #[test]
    fn streaming_harness_does_not_clean_concurrent_instance_images() {
        assert_eq!(
            streaming_harness_image_cache_args(),
            ["--cache_level", "instance", "--clean", "false"]
        );
    }

    #[test]
    fn streaming_harness_report_path_and_result_are_strict() {
        let directory = tempfile::tempdir().unwrap();
        let config = SweBenchStreamingHarnessConfig {
            python: std::path::PathBuf::from("python"),
            dataset_name: "SWE-bench/SWE-bench_Lite".to_string(),
            split: "test".to_string(),
            run_id: "audit-run".to_string(),
            timeout_secs: 1800,
        };
        let prediction = prediction("django__django-11620".to_string(), "patch".to_string());
        let report_path = streaming_harness_report_path(
            directory.path(),
            "django__django-11620",
            &prediction,
            &config,
        );
        assert!(report_path.ends_with(
            "logs/run_evaluation/audit-run/astrcode-eval-deepseek-v4-flash/django__django-11620/\
             report.json"
        ));

        std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
        std::fs::write(
            &report_path,
            r#"{"django__django-11620":{"resolved":true}}"#,
        )
        .unwrap();
        assert!(read_streaming_harness_result(&report_path, "django__django-11620").unwrap());

        std::fs::write(&report_path, r#"{"django__django-11620":{}}"#).unwrap();
        let error =
            read_streaming_harness_result(&report_path, "django__django-11620").unwrap_err();
        assert!(error.to_string().contains("lacks boolean resolved"));
    }

    #[test]
    fn streaming_harness_python_path_preserves_virtualenv_symlink() {
        let relative = Path::new("target/harness-venv/bin/python");
        let absolute = absolute_path_preserving_symlinks(relative).unwrap();

        assert!(absolute.is_absolute());
        assert!(absolute.ends_with(relative));
    }

    #[test]
    fn server_config_requires_trusted_gateway_and_requested_model() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
activeProfile = "deepseek"
activeModel = "deepseek-v4-flash"
[[profiles]]
name = "deepseek"
providerKind = "openai"
wireFormat = "openai_chat_completions"
authScheme = "bearer"
baseUrl = "http://astrcode-swebench-egress:8080"
apiKey = "gateway-managed"
"#,
        )
        .unwrap();
        validate_server_config(&path).unwrap();

        std::fs::write(
            &path,
            r#"
activeProfile = "deepseek"
activeModel = "deepseek-v4-flash"
[[profiles]]
name = "deepseek"
providerKind = "openai"
wireFormat = "openai_chat_completions"
authScheme = "bearer"
baseUrl = "https://api.deepseek.com"
apiKey = "plaintext-secret"
"#,
        )
        .unwrap();
        let error = validate_server_config(&path).unwrap_err();
        assert!(error.to_string().contains("trusted provider gateway"));

        std::fs::write(
            &path,
            r#"
activeProfile = "deepseek"
activeModel = "deepseek-v4-flash"
[[profiles]]
name = "deepseek"
providerKind = "openai"
wireFormat = "openai_chat_completions"
authScheme = "bearer"
baseUrl = "http://astrcode-swebench-egress:8080"
apiKey = "gateway-managed"
[[profiles]]
name = "inactive"
providerKind = "openai"
wireFormat = "openai_chat_completions"
authScheme = "bearer"
baseUrl = "https://example.invalid"
apiKey = "inactive-secret"
"#,
        )
        .unwrap();
        let error = validate_server_config(&path).unwrap_err();
        assert!(error.to_string().contains("exactly one provider profile"));
    }

    #[test]
    fn instance_id_validation_rejects_path_traversal_and_accepts_official_ids() {
        validate_instance_id("django__django-11620").unwrap();
        validate_instance_id("scikit-learn__scikit-learn-10297").unwrap();
        for invalid in [
            "../escape__repo-1",
            "/absolute__repo-1",
            "owner/repo__repo-1",
            "django__django-not-a-number",
        ] {
            assert!(validate_instance_id(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[tokio::test]
    async fn bounded_capture_rejects_output_over_limit() {
        assert_eq!(
            read_bounded(&b"1234"[..], 4, "test").await.unwrap(),
            b"1234"
        );
        let error = read_bounded(&b"12345"[..], 4, "test").await.unwrap_err();
        assert!(error.to_string().contains("4-byte audit limit"));
    }
}
