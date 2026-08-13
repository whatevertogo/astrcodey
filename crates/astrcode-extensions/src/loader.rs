//! 扩展加载器 — 从全局和项目目录发现并加载 s5r 子进程扩展。

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use astrcode_core::config::defaults::astrcode_dir;
use astrcode_extension_contract::protocol::S5R_VERSION;
use astrcode_extension_sdk::{
    extension::{Extension, ExtensionPackageManifest, StopReason},
    manifest::validate_extension_id,
};
use sha2::{Digest, Sha256};

use crate::{
    host_router::HostRouter,
    runner::{ExtensionRunner, RegisteredSourceExtension},
};

type CandidateLoadFuture = Pin<Box<dyn Future<Output = Result<Arc<dyn Extension>, String>> + Send>>;
type CandidateLoader = Box<dyn FnOnce() -> CandidateLoadFuture + Send>;
type CurrentSourceMap<'a> = HashMap<&'a str, &'a RegisteredSourceExtension>;

/// 来源发现出的扩展候选。构造候选不会启动扩展。
///
/// `source_key` 必须在所有来源间稳定且唯一；`fingerprint` 必须覆盖会改变扩展
/// 运行行为的来源内容；`extension_id` 必须是无需执行 loader 即可读取的权威身份。
pub struct ExtensionCandidate {
    source_key: String,
    fingerprint: String,
    extension_id: String,
    load: CandidateLoader,
}

impl ExtensionCandidate {
    /// 构造已经存在于当前进程中的候选，例如 bundled extension。
    pub fn ready(
        source_key: impl Into<String>,
        fingerprint: impl Into<String>,
        extension: Arc<dyn Extension>,
    ) -> Self {
        let extension_id = extension.manifest().id().to_owned();
        Self::lazy(source_key, fingerprint, extension_id, move || async move {
            Ok(extension)
        })
    }

    /// 构造仅在 reconcile 判定来源已启用且新增或变化时才执行 loader 的候选。
    pub fn lazy<F, Fut>(
        source_key: impl Into<String>,
        fingerprint: impl Into<String>,
        extension_id: impl Into<String>,
        load: F,
    ) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<Arc<dyn Extension>, String>> + Send + 'static,
    {
        Self {
            source_key: source_key.into(),
            fingerprint: fingerprint.into(),
            extension_id: extension_id.into(),
            load: Box::new(move || Box::pin(load())),
        }
    }
}

/// 扩展来源的发现结果。
#[derive(Default)]
pub struct DiscoverExtensionsResult {
    pub candidates: Vec<ExtensionCandidate>,
    pub failures: Vec<ExtensionLoadFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionLoadFailure {
    pub source_key: Option<String>,
    pub extension_id: Option<String>,
    pub message: String,
    pub duration_ms: Option<u64>,
}

/// 扩展加载上下文。
pub struct ExtensionLoadContext {
    pub working_dir: Option<String>,
    /// 磁盘 s5r 扩展加载时必需。
    pub host_router: Option<Arc<HostRouter>>,
}

#[async_trait::async_trait]
pub trait ExtensionSource: Send + Sync {
    async fn discover(&self, ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult;

    fn is_enabled(&self, _extension_id: &str) -> bool {
        true
    }

    /// 判断已注册来源键是否属于当前来源，用于发现阶段整体失败时保留运行实例。
    fn owns_source_key(&self, _source_key: &str) -> bool {
        false
    }
}

pub async fn sync_extension_sources(
    runner: &Arc<ExtensionRunner>,
    ctx: &ExtensionLoadContext,
    sources: &[&dyn ExtensionSource],
) -> Vec<String> {
    let _reconcile = runner.lock_source_reconcile().await;
    let current_ids = runner.registered_extension_ids().await;
    let current_sources = runner.registered_source_extensions().await;
    let current_by_source: CurrentSourceMap<'_> = current_sources
        .iter()
        .map(|current| (current.key.as_str(), current))
        .collect();
    let current_by_id: CurrentSourceMap<'_> = current_sources
        .iter()
        .map(|current| (current.id.as_str(), current))
        .collect();
    let plan = build_reconcile_plan(
        runner,
        ctx,
        sources,
        &current_ids,
        &current_sources,
        &current_by_source,
    )
    .await;
    apply_reconcile_plan(
        runner,
        ctx,
        &current_ids,
        &current_by_source,
        &current_by_id,
        plan,
    )
    .await
}

struct ReconcilePlan {
    desired_extensions: Vec<DesiredExtension>,
    desired_ids: HashSet<String>,
    protected_ids: HashSet<String>,
    errors: Vec<String>,
}

async fn build_reconcile_plan(
    runner: &ExtensionRunner,
    ctx: &ExtensionLoadContext,
    sources: &[&dyn ExtensionSource],
    current_ids: &[String],
    current_sources: &[RegisteredSourceExtension],
    current_by_source: &CurrentSourceMap<'_>,
) -> ReconcilePlan {
    let mut desired_extensions = Vec::new();
    let mut desired_ids = HashSet::new();
    let mut source_keys = HashSet::new();
    let mut protected_ids = HashSet::new();
    let mut errors = Vec::new();

    for source in sources {
        let discovery = source.discover(ctx).await;
        for failure in discovery.failures {
            let current_source = failure
                .source_key
                .as_deref()
                .and_then(|source_key| current_by_source.get(source_key).copied());
            let extension_id = current_source
                .map(|current| current.id.as_str())
                .or(failure.extension_id.as_deref());
            if let Some(extension_id) = extension_id {
                if source.is_enabled(extension_id)
                    && current_ids.iter().any(|id| id == extension_id)
                {
                    protected_ids.insert(extension_id.to_string());
                }
                runner.record_extension_load_failure(
                    extension_id,
                    failure.message.clone(),
                    failure.duration_ms.map(Duration::from_millis),
                );
            } else {
                protected_ids.extend(
                    current_sources
                        .iter()
                        .filter(|current| {
                            source.owns_source_key(&current.key) && source.is_enabled(&current.id)
                        })
                        .map(|current| current.id.clone()),
                );
            }
            errors.push(failure.message);
        }

        for candidate in discovery.candidates {
            let ExtensionCandidate {
                source_key,
                fingerprint,
                extension_id,
                load,
            } = candidate;
            if !source_keys.insert(source_key.clone()) {
                errors.push(format!("duplicate extension source key: {source_key}"));
                continue;
            }
            if !source.is_enabled(&extension_id) {
                continue;
            }
            let current_source = current_by_source.get(source_key.as_str()).copied();
            if let Some(current) = current_source
                .filter(|current| current.id == extension_id && current.fingerprint == fingerprint)
            {
                if desired_ids.insert(current.id.clone()) {
                    desired_extensions.push(DesiredExtension::Retain {
                        id: current.id.clone(),
                    });
                }
                continue;
            }

            if desired_ids.insert(extension_id.clone()) {
                desired_extensions.push(DesiredExtension::Start {
                    id: extension_id,
                    source_key,
                    fingerprint,
                    load,
                });
            }
        }
    }

    ReconcilePlan {
        desired_extensions,
        desired_ids,
        protected_ids,
        errors,
    }
}

async fn apply_reconcile_plan(
    runner: &ExtensionRunner,
    ctx: &ExtensionLoadContext,
    current_ids: &[String],
    current_by_source: &CurrentSourceMap<'_>,
    current_by_id: &CurrentSourceMap<'_>,
    plan: ReconcilePlan,
) -> Vec<String> {
    let ReconcilePlan {
        desired_extensions,
        desired_ids,
        protected_ids,
        mut errors,
    } = plan;
    let desired_order = desired_extensions
        .iter()
        .map(DesiredExtension::id)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut pending_task_activations = Vec::new();
    let batch_publication = runner.begin_source_batch_publication();

    for desired in desired_extensions {
        let DesiredExtension::Start {
            id,
            source_key,
            fingerprint,
            load,
        } = desired
        else {
            continue;
        };
        let mut replaced_ids = Vec::new();
        if let Some(current) = current_by_source.get(source_key.as_str()) {
            replaced_ids.push(current.id.as_str());
        }
        if let Some(current) = current_by_id.get(id.as_str()) {
            replaced_ids.push(current.id.as_str());
        } else if current_ids.iter().any(|current_id| current_id == &id) {
            replaced_ids.push(id.as_str());
        }
        replaced_ids.sort_unstable();
        replaced_ids.dedup();

        let mut replacement_blocked = false;
        let mut retirements = Vec::new();
        for replaced_id in &replaced_ids {
            match runner
                .unregister_with_retirement(replaced_id, StopReason::Reload)
                .await
            {
                Ok(Some(retirement)) => retirements.push((*replaced_id, retirement)),
                Ok(None) => {},
                Err(error) => {
                    errors.push(format!("failed to reload extension {replaced_id}: {error}"));
                    replacement_blocked = true;
                },
            }
        }
        for (replaced_id, retirement) in retirements {
            if let Err(error) = retirement.wait().await {
                errors.push(format!(
                    "failed to retire extension {replaced_id} before starting {id}: {error}"
                ));
                replacement_blocked = true;
            }
        }
        if replacement_blocked {
            continue;
        }
        let started = Instant::now();
        let extension = match load()
            .await
            .and_then(|extension| ensure_candidate_identity(&id, extension))
        {
            Ok(extension) => extension,
            Err(error) => {
                runner.record_extension_load_failure(&id, error.clone(), Some(started.elapsed()));
                errors.push(error);
                continue;
            },
        };
        runner.record_extension_load_success(&id, Some(started.elapsed()));
        match runner
            .register_deferred(
                extension,
                ctx.working_dir.as_deref(),
                source_key,
                fingerprint,
            )
            .await
        {
            Ok(Some(activation)) => pending_task_activations.push(activation),
            Ok(None) => {},
            Err(error) => {
                errors.push(format!("failed to start extension {id}: {error}"));
            },
        }
    }

    for id in current_ids
        .iter()
        .filter(|id| !desired_ids.contains(*id) && !protected_ids.contains(*id))
    {
        if let Err(error) = runner.unregister(id, StopReason::Disabled).await {
            errors.push(format!("failed to stop extension {id}: {error}"));
        }
    }
    let mut publication_order = desired_order;
    publication_order.extend(
        current_ids
            .iter()
            .filter(|id| protected_ids.contains(*id) && !desired_ids.contains(*id))
            .cloned(),
    );
    runner.reorder_source_extensions(&publication_order).await;
    drop(batch_publication);
    for activation in pending_task_activations {
        activation.activate();
    }

    errors
}

/// 磁盘 s5r 扩展源（`~/.astrcode/extensions/` 与项目 `.astrcode/extensions/`）。
pub struct DiskExtensionSource {
    extension_states: BTreeMap<String, bool>,
}

impl DiskExtensionSource {
    pub fn new(extension_states: BTreeMap<String, bool>) -> Self {
        Self { extension_states }
    }
}

#[async_trait::async_trait]
impl ExtensionSource for DiskExtensionSource {
    async fn discover(&self, ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult {
        discover_all(ctx.working_dir.as_deref(), ctx.host_router.clone()).await
    }

    fn is_enabled(&self, extension_id: &str) -> bool {
        self.extension_states
            .get(extension_id)
            .copied()
            .unwrap_or(true)
    }

    fn owns_source_key(&self, source_key: &str) -> bool {
        source_key.starts_with("disk:")
    }
}

enum DesiredExtension {
    Retain {
        id: String,
    },
    Start {
        id: String,
        source_key: String,
        fingerprint: String,
        load: CandidateLoader,
    },
}

impl DesiredExtension {
    fn id(&self) -> &str {
        match self {
            Self::Retain { id } | Self::Start { id, .. } => id,
        }
    }
}

async fn discover_all(
    working_dir: Option<&str>,
    host_router: Option<Arc<HostRouter>>,
) -> DiscoverExtensionsResult {
    let mut result = DiscoverExtensionsResult::default();

    let global_dir = astrcode_dir().join("extensions");
    if global_dir.exists() {
        let global = discover_from_dir(&global_dir, host_router.clone()).await;
        result.candidates.extend(global.candidates);
        result.failures.extend(global.failures);
    }

    if let Some(wd) = working_dir {
        let project_dir = PathBuf::from(wd).join(".astrcode").join("extensions");
        if project_dir.exists() {
            let project = discover_from_dir(&project_dir, host_router).await;
            result.candidates.splice(0..0, project.candidates);
            result.failures.extend(project.failures);
        }
    }

    result
}

#[doc(hidden)]
pub async fn load_extensions_from_dir_for_test(
    dir: &Path,
    host_router: &Option<Arc<HostRouter>>,
) -> (Vec<Arc<dyn Extension>>, Vec<String>) {
    let discovery = discover_from_dir(dir, host_router.clone()).await;
    load_discovered(discovery).await
}

async fn discover_from_dir(
    dir: &Path,
    host_router: Option<Arc<HostRouter>>,
) -> DiscoverExtensionsResult {
    let mut result = DiscoverExtensionsResult::default();
    let paths = match extension_dirs(dir).await {
        Ok(paths) => paths,
        Err(e) => {
            result.failures.push(ExtensionLoadFailure {
                source_key: None,
                extension_id: None,
                message: e.clone(),
                duration_ms: None,
            });
            return result;
        },
    };

    for path in paths {
        let started = Instant::now();
        match discover_extension(&path, host_router.clone()).await {
            Ok(candidate) => result.candidates.push(candidate),
            Err(message) => {
                result.failures.push(ExtensionLoadFailure {
                    source_key: disk_source_key(&path).await,
                    extension_id: None,
                    message: message.clone(),
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                });
            },
        }
    }

    result
}

async fn load_discovered(
    discovery: DiscoverExtensionsResult,
) -> (Vec<Arc<dyn Extension>>, Vec<String>) {
    let mut extensions = Vec::new();
    let mut errors = discovery
        .failures
        .into_iter()
        .map(|failure| failure.message)
        .collect::<Vec<_>>();
    for candidate in discovery.candidates {
        let ExtensionCandidate {
            extension_id, load, ..
        } = candidate;
        match load()
            .await
            .and_then(|extension| ensure_candidate_identity(&extension_id, extension))
        {
            Ok(extension) => extensions.push(extension),
            Err(message) => {
                errors.push(message);
            },
        }
    }
    (extensions, errors)
}

async fn extension_dirs(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| format!("Cannot read extensions dir {}: {e}", dir.display()))?;
    let mut paths = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("read dir entry: {e}"))?
    {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .await
            .map_err(|e| format!("read file type: {e}"))?;
        if file_type.is_dir()
            && tokio::fs::metadata(path.join("extension.json"))
                .await
                .is_ok()
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

async fn discover_extension(
    ext_dir: &Path,
    host_router: Option<Arc<HostRouter>>,
) -> Result<ExtensionCandidate, String> {
    let manifest_path = ext_dir.join("extension.json");
    let manifest_bytes = tokio::fs::read(&manifest_path)
        .await
        .map_err(|e| format!("{}: read manifest: {e}", ext_dir.display()))?;
    let entry: ExtensionPackageManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| format!("{}: parse manifest: {e}", ext_dir.display()))?;
    validate_extension_id(&entry.extension_id)
        .map_err(|error| format!("{}: {error}", ext_dir.display()))?;

    if entry.protocol.s5r != S5R_VERSION {
        return Err(format!(
            "{}: extension.json must set protocol.s5r to \"{}\"",
            ext_dir.display(),
            S5R_VERSION
        ));
    }

    if entry.command.is_empty() {
        return Err(format!(
            "{}: extension.json 'command' must contain an executable",
            ext_dir.display()
        ));
    }

    let canonical_dir = tokio::fs::canonicalize(ext_dir)
        .await
        .map_err(|error| format!("{}: canonicalize: {error}", ext_dir.display()))?;
    let source_key = format!("disk:{}", canonical_dir.display());
    let fingerprint = disk_source_fingerprint(&canonical_dir, manifest_bytes, &entry).await?;
    let extension_id = entry.extension_id.clone();
    let display_path = canonical_dir.display().to_string();

    Ok(ExtensionCandidate::lazy(
        source_key,
        fingerprint,
        extension_id,
        move || async move {
            let router = host_router.ok_or_else(|| {
                format!(
                    "{display_path}: ExtensionLoadContext.host_router is required for disk \
                     extensions"
                )
            })?;
            crate::s5r_ext::S5rExtension::load(&canonical_dir, &entry, router)
                .await
                .map(|extension| extension as Arc<dyn Extension>)
                .map_err(|error| format!("{display_path}: {error}"))
        },
    ))
}

async fn disk_source_fingerprint(
    ext_dir: &Path,
    manifest_bytes: Vec<u8>,
    manifest: &ExtensionPackageManifest,
) -> Result<String, String> {
    let (program, args) = crate::s5r_ext::parse_command(manifest, ext_dir)
        .map_err(|error| format!("{}: {error}", ext_dir.display()))?;
    let ext_dir = ext_dir.to_path_buf();
    let display_path = ext_dir.display().to_string();
    tokio::task::spawn_blocking(move || {
        hash_disk_source(&ext_dir, &manifest_bytes, &program, &args)
    })
    .await
    .map_err(|error| format!("{display_path}: fingerprint task failed: {error}"))?
    .map_err(|error| format!("{display_path}: {error}"))
}

async fn disk_source_key(ext_dir: &Path) -> Option<String> {
    tokio::fs::canonicalize(ext_dir)
        .await
        .ok()
        .map(|path| format!("disk:{}", path.display()))
}

fn ensure_candidate_identity(
    expected_id: &str,
    extension: Arc<dyn Extension>,
) -> Result<Arc<dyn Extension>, String> {
    let actual_id = extension.manifest().id().to_owned();
    if actual_id != expected_id {
        return Err(format!(
            "extension loader returned manifest id {actual_id:?}, expected {expected_id:?}"
        ));
    }
    Ok(extension)
}

fn hash_disk_source(
    ext_dir: &Path,
    manifest_bytes: &[u8],
    program: &str,
    args: &[String],
) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hash_fingerprint_component(&mut hasher, manifest_bytes);

    let mut artifacts = Vec::new();
    if let Some(program_path) = resolve_program_path(program) {
        artifacts.push(program_path);
    }
    artifacts.extend(args.iter().filter_map(|arg| {
        let path = Path::new(arg);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            ext_dir.join(path)
        };
        path.is_file().then_some(path)
    }));
    artifacts.sort();
    artifacts.dedup();

    for artifact in artifacts {
        let bytes = std::fs::read(&artifact)
            .map_err(|error| format!("read command artifact {}: {error}", artifact.display()))?;
        hash_fingerprint_component(&mut hasher, artifact.to_string_lossy().as_bytes());
        hash_fingerprint_component(&mut hasher, &bytes);
    }

    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn resolve_program_path(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    (path.is_absolute() && path.is_file()).then(|| path.to_path_buf())
}

fn hash_fingerprint_component(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[tokio::test]
    async fn extension_dirs_are_sorted_and_manifest_bound() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("astrcode-ext-loader-{suffix}"));
        fs::create_dir_all(root.join("zeta")).unwrap();
        fs::create_dir_all(root.join("alpha")).unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join("zeta").join("extension.json"), "{}").unwrap();
        fs::write(root.join("alpha").join("extension.json"), "{}").unwrap();

        let dirs = extension_dirs(&root).await.unwrap();
        let names = dirs
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();

        fs::remove_dir_all(&root).unwrap();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn disk_fingerprint_tracks_manifest_and_command_artifact_content() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("extension");
        fs::write(&program, b"binary-v1").unwrap();
        let manifest =
            br#"{"extension_id":"test","protocol":{"s5r":"3.0"},"command":["./extension"]}"#;

        let first =
            hash_disk_source(root.path(), manifest, program.to_str().unwrap(), &[]).unwrap();
        let unchanged =
            hash_disk_source(root.path(), manifest, program.to_str().unwrap(), &[]).unwrap();
        fs::write(&program, b"binary-v2").unwrap();
        let changed_binary =
            hash_disk_source(root.path(), manifest, program.to_str().unwrap(), &[]).unwrap();
        let changed_manifest = hash_disk_source(
            root.path(),
            br#"{"extension_id":"test","protocol":{"s5r":"3.0"},"command":["./extension"],"env":{"MODE":"test"}}"#,
            program.to_str().unwrap(),
            &[],
        )
        .unwrap();

        assert_eq!(first, unchanged);
        assert_ne!(first, changed_binary);
        assert_ne!(changed_binary, changed_manifest);
    }
}
