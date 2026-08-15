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
use astrcode_extension_sdk::{
    extension::{Extension, ExtensionPackageManifest},
    manifest::validate_extension_id,
    transport::{TransportFeature, TransportProfile},
    wire::protocol::S5R_VERSION,
};
use sha2::{Digest, Sha256};

use crate::{
    host_router::HostRouter,
    runner::{ExtensionRunner, PreparedExtensionGeneration, SourceGenerationEntry},
};

type CandidateLoadFuture = Pin<Box<dyn Future<Output = Result<Arc<dyn Extension>, String>> + Send>>;
type CandidateLoader = Box<dyn FnOnce() -> CandidateLoadFuture + Send>;

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
    pub transport_profile: TransportProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtensionAdmissionError {
    extension_id: String,
    missing: Vec<TransportFeature>,
}

impl std::fmt::Display for ExtensionAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let missing = self
            .missing
            .iter()
            .map(|feature| feature.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            formatter,
            "extension {} requires unavailable transport features: {missing}",
            self.extension_id
        )
    }
}

impl std::error::Error for ExtensionAdmissionError {}

#[async_trait::async_trait]
pub trait ExtensionSource: Send + Sync {
    async fn discover(&self, ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult;

    fn is_enabled(&self, _extension_id: &str) -> bool {
        true
    }
}

pub async fn prepare_extension_generation(
    runner: &Arc<ExtensionRunner>,
    ctx: &ExtensionLoadContext,
    sources: &[&dyn ExtensionSource],
    configs: &BTreeMap<String, serde_json::Value>,
) -> Result<PreparedExtensionGeneration, Vec<String>> {
    let source_transaction = runner.begin_source_transaction().await;
    let current_sources = runner.registered_source_extensions().await;
    let current_by_source = current_sources
        .iter()
        .map(|current| (current.key.as_str(), current))
        .collect::<HashMap<_, _>>();
    let mut discovered = Vec::new();
    let mut source_keys = HashSet::new();
    let mut extension_ids = HashSet::new();
    let mut errors = Vec::new();

    for source in sources {
        let discovery = source.discover(ctx).await;
        for failure in discovery.failures {
            let extension_id = failure.extension_id.as_deref().or_else(|| {
                failure
                    .source_key
                    .as_deref()
                    .and_then(|key| current_by_source.get(key))
                    .map(|current| current.id.as_str())
            });
            if let Some(extension_id) = extension_id {
                runner.record_extension_load_failure(
                    extension_id,
                    failure.message.clone(),
                    failure.duration_ms.map(Duration::from_millis),
                );
            }
            errors.push(failure.message);
        }

        for candidate in discovery.candidates {
            if !source.is_enabled(&candidate.extension_id) {
                continue;
            }
            if !source_keys.insert(candidate.source_key.clone()) {
                errors.push(format!(
                    "duplicate extension source key: {}",
                    candidate.source_key
                ));
                continue;
            }
            if !extension_ids.insert(candidate.extension_id.clone()) {
                errors.push(format!(
                    "duplicate extension id: {}",
                    candidate.extension_id
                ));
                continue;
            }
            discovered.push(candidate);
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut entries = Vec::with_capacity(discovered.len());
    for candidate in discovered {
        let ExtensionCandidate {
            source_key,
            fingerprint,
            extension_id,
            load,
        } = candidate;
        let config = configs
            .get(&extension_id)
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let fingerprint = configured_source_fingerprint(&fingerprint, &config);
        if current_by_source
            .get(source_key.as_str())
            .is_some_and(|current| current.id == extension_id && current.fingerprint == fingerprint)
        {
            entries.push(SourceGenerationEntry::Retain {
                id: extension_id,
                key: source_key,
                fingerprint,
            });
            continue;
        }
        let started = Instant::now();
        let extension = match load()
            .await
            .and_then(|extension| ensure_candidate_identity(&extension_id, extension))
        {
            Ok(extension) => extension,
            Err(error) => {
                runner.record_extension_load_failure(
                    &extension_id,
                    error.clone(),
                    Some(started.elapsed()),
                );
                errors.push(error);
                continue;
            },
        };
        if let Err(error) = admit_candidate(&extension, &ctx.transport_profile) {
            runner.record_extension_load_failure(
                &extension_id,
                error.to_string(),
                Some(started.elapsed()),
            );
            tracing::warn!(%error, "extension candidate rejected by transport admission");
            continue;
        }
        runner.record_extension_load_success(&extension_id, Some(started.elapsed()));
        entries.push(SourceGenerationEntry::Start {
            extension,
            key: source_key,
            fingerprint,
            config,
        });
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    runner
        .prepare_source_generation(source_transaction, entries, ctx.working_dir.as_deref())
        .await
        .map_err(|error| vec![error.to_string()])
}

fn admit_candidate(
    extension: &Arc<dyn Extension>,
    transport_profile: &TransportProfile,
) -> Result<(), ExtensionAdmissionError> {
    let manifest = extension.manifest();
    let missing = transport_profile.missing(manifest.required_transport_features().iter().copied());
    if missing.is_empty() {
        return Ok(());
    }
    Err(ExtensionAdmissionError {
        extension_id: manifest.id().to_owned(),
        missing,
    })
}

fn configured_source_fingerprint(source: &str, config: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hash_fingerprint_component(&mut hasher, source.as_bytes());
    hash_canonical_json(&mut hasher, config);
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_canonical_json(hasher: &mut Sha256, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => hasher.update(b"null"),
        serde_json::Value::Bool(value) => {
            hasher.update(b"bool");
            hasher.update([u8::from(*value)]);
        },
        serde_json::Value::Number(value) => {
            hasher.update(b"number");
            hash_fingerprint_component(hasher, value.to_string().as_bytes());
        },
        serde_json::Value::String(value) => {
            hasher.update(b"string");
            hash_fingerprint_component(hasher, value.as_bytes());
        },
        serde_json::Value::Array(values) => {
            hasher.update(b"array");
            hasher.update((values.len() as u64).to_le_bytes());
            for value in values {
                hash_canonical_json(hasher, value);
            }
        },
        serde_json::Value::Object(values) => {
            hasher.update(b"object");
            hasher.update((values.len() as u64).to_le_bytes());
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            for (key, value) in entries {
                hash_fingerprint_component(hasher, key.as_bytes());
                hash_canonical_json(hasher, value);
            }
        },
    }
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

    #[test]
    fn configured_fingerprint_is_canonical_and_tracks_extension_owned_values() {
        let first = configured_source_fingerprint(
            "bundled:test",
            &serde_json::json!({ "maxOutputTokens": 2048, "nested": { "b": 2, "a": 1 } }),
        );
        let reordered = configured_source_fingerprint(
            "bundled:test",
            &serde_json::json!({ "nested": { "a": 1, "b": 2 }, "maxOutputTokens": 2048 }),
        );
        let changed = configured_source_fingerprint(
            "bundled:test",
            &serde_json::json!({ "nested": { "a": 1, "b": 2 }, "maxOutputTokens": 4096 }),
        );

        assert_eq!(first, reordered);
        assert_ne!(first, changed);
    }
}
