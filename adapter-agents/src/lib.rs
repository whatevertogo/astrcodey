//! # Agent Loader
//!
//! 负责把 Agent 定义从多种来源收敛成统一的 `AgentProfileRegistry`：
//! - 内置 builtin agents
//! - 用户级 `~/.claude/agents` / `~/.astrcode/agents`
//! - 项目级 `<working_dir>/.claude/agents` / `<working_dir>/.astrcode/agents`
//!
//! 文件格式支持：
//! - Claude Code sub-agents 的 Markdown + YAML frontmatter
//! - 纯 YAML agent 定义（`.yml` / `.yaml`）

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use astrcode_core::{AgentMode, AgentProfile, AstrError};
use astrcode_support::hostpaths::resolve_home_dir;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentLoaderError {
    #[error("failed to resolve agent search roots: {0}")]
    ResolvePath(#[from] AstrError),
    #[error("failed to read agent directory '{path}': {source}")]
    ReadDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read agent file '{path}': {source}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("agent file '{path}' is missing YAML frontmatter")]
    MissingFrontmatter { path: String },
    #[error("failed to parse agent frontmatter for '{path}': {source}")]
    ParseFrontmatter {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("agent file '{path}' has invalid frontmatter: {message}")]
    InvalidFrontmatter { path: String, message: String },
}

#[derive(Debug, Clone, Default)]
pub struct AgentProfileRegistry {
    profiles: BTreeMap<String, AgentProfile>,
}

impl AgentProfileRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建带内置默认 agents 的注册表。
    ///
    /// builtin agents 使用与外部同一套 Markdown frontmatter 格式，
    /// 避免“内置一套、外部一套”的双轨逻辑继续扩散。
    pub fn with_builtin_defaults() -> Self {
        let mut registry = Self::new();
        for builtin in builtin_agents() {
            let profile = parse_agent_markdown(Path::new(builtin.path), builtin.content)
                .expect("builtin agent definition should always be valid");
            registry.insert(profile);
        }
        registry
    }

    pub fn insert(&mut self, profile: AgentProfile) -> Option<AgentProfile> {
        self.profiles.insert(profile.id.clone(), profile)
    }

    pub fn get(&self, profile_id: &str) -> Option<&AgentProfile> {
        self.profiles.get(profile_id)
    }

    pub fn list(&self) -> Vec<&AgentProfile> {
        self.profiles.values().collect()
    }

    pub fn list_subagent_profiles(&self) -> Vec<&AgentProfile> {
        self.profiles
            .values()
            .filter(|profile| matches!(profile.mode, AgentMode::SubAgent | AgentMode::All))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct AgentProfileLoader {
    user_agent_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWatchPath {
    pub path: PathBuf,
    pub recursive: bool,
}

impl AgentProfileLoader {
    /// 创建默认 loader。
    ///
    /// 用户级目录固定为：
    /// - `~/.claude/agents`
    /// - `~/.astrcode/agents`
    pub fn new() -> Result<Self, AstrError> {
        let home = resolve_home_dir()?;
        Ok(Self::new_with_home_dir(home))
    }

    /// 基于显式 home 目录创建 loader。
    ///
    /// server 测试和嵌入式组合根需要按调用方提供的隔离目录解析 agents，
    /// 不能再偷偷依赖进程级 home 覆盖或全局环境变量。
    pub fn new_with_home_dir(home_dir: impl AsRef<Path>) -> Self {
        let home_dir = home_dir.as_ref();
        Self {
            user_agent_dirs: vec![
                home_dir.join(".claude").join("agents"),
                home_dir.join(".astrcode").join("agents"),
            ],
        }
    }

    /// 加载不绑定项目 scope 的 agents。
    ///
    /// 只包含 builtin + 用户级目录；项目级 agent 必须通过显式 working_dir 注入，
    /// 避免解析语义偷偷依赖进程 cwd。
    pub fn load(&self) -> Result<AgentProfileRegistry, AgentLoaderError> {
        self.load_for_working_dirs(std::iter::empty::<&Path>())
    }

    /// 加载指定工作目录可见的全部 agents。
    ///
    /// 优先级从低到高：
    /// 1. builtin agents
    /// 2. 用户级 `~/.claude/agents`
    /// 3. 用户级 `~/.astrcode/agents`
    /// 4. 项目祖先链上的 `.claude/agents`
    /// 5. 项目祖先链上的 `.astrcode/agents`
    ///
    /// 越靠后的目录优先级越高，同名 agent 会覆盖前面的定义。
    pub fn load_for_working_dir(
        &self,
        working_dir: Option<&Path>,
    ) -> Result<AgentProfileRegistry, AgentLoaderError> {
        match working_dir {
            Some(working_dir) => self.load_for_working_dirs([working_dir]),
            None => self.load(),
        }
    }

    pub fn load_for_working_dirs<'a, I>(
        &self,
        working_dirs: I,
    ) -> Result<AgentProfileRegistry, AgentLoaderError>
    where
        I: IntoIterator<Item = &'a Path>,
    {
        let mut registry = AgentProfileRegistry::with_builtin_defaults();
        for dir in self.search_dirs_for_working_dirs(working_dirs) {
            merge_agents_dir(&mut registry, &dir)?;
        }
        Ok(registry)
    }

    pub fn search_dirs(&self, working_dir: Option<&Path>) -> Vec<PathBuf> {
        match working_dir {
            Some(working_dir) => self.search_dirs_for_working_dirs([working_dir]),
            None => self.search_dirs_for_working_dirs(std::iter::empty::<&Path>()),
        }
    }

    pub fn search_dirs_for_working_dirs<'a, I>(&self, working_dirs: I) -> Vec<PathBuf>
    where
        I: IntoIterator<Item = &'a Path>,
    {
        let mut dirs = self
            .user_agent_dirs
            .iter()
            .filter(|dir| dir.exists())
            .cloned()
            .collect::<Vec<_>>();

        let mut seen = dirs.iter().cloned().collect::<HashSet<_>>();
        for working_dir in working_dirs {
            for dir in project_agent_dirs(working_dir) {
                if seen.insert(dir.clone()) {
                    dirs.push(dir);
                }
            }
        }

        dirs
    }

    /// 返回需要监听的目录集合。
    ///
    /// 规则：
    /// - `agents/` 已存在时，直接递归监听该目录
    /// - `agents/` 不存在时，监听最近的已存在父目录，等待目录被创建
    pub fn watch_paths(&self, working_dir: Option<&Path>) -> Vec<AgentWatchPath> {
        match working_dir {
            Some(working_dir) => self.watch_paths_for_working_dirs([working_dir]),
            None => self.watch_paths_for_working_dirs(std::iter::empty::<&Path>()),
        }
    }

    pub fn watch_paths_for_working_dirs<'a, I>(&self, working_dirs: I) -> Vec<AgentWatchPath>
    where
        I: IntoIterator<Item = &'a Path>,
    {
        let mut watch_paths = Vec::new();
        let mut seen = HashSet::new();

        for dir in &self.user_agent_dirs {
            if let Some(target) = watch_path_for_agent_dir(dir) {
                let key = (target.path.clone(), target.recursive);
                if seen.insert(key) {
                    watch_paths.push(target);
                }
            }
        }

        for working_dir in working_dirs {
            for dir in project_agent_dir_candidates(working_dir) {
                if let Some(target) = watch_path_for_agent_dir(&dir) {
                    let key = (target.path.clone(), target.recursive);
                    if seen.insert(key) {
                        watch_paths.push(target);
                    }
                }
            }
        }

        watch_paths
    }
}

struct BuiltinAgent {
    path: &'static str,
    content: &'static str,
}

fn builtin_agents() -> &'static [BuiltinAgent] {
    &[
        BuiltinAgent {
            path: "builtin://explore.md",
            content: include_str!("builtin_agents/explore.md"),
        },
        BuiltinAgent {
            path: "builtin://reviewer.md",
            content: include_str!("builtin_agents/reviewer.md"),
        },
        BuiltinAgent {
            path: "builtin://execute.md",
            content: include_str!("builtin_agents/execute.md"),
        },
    ]
}

fn project_agent_dirs(working_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in project_agent_dir_candidates(working_dir) {
        if dir.exists() {
            dirs.push(dir);
        }
    }
    dirs
}

fn project_agent_dir_candidates(working_dir: &Path) -> Vec<PathBuf> {
    let mut ancestors = working_dir.ancestors().collect::<Vec<_>>();
    ancestors.reverse();

    let mut dirs = Vec::new();
    for ancestor in ancestors {
        dirs.push(ancestor.join(".claude").join("agents"));
        dirs.push(ancestor.join(".astrcode").join("agents"));
    }
    dirs
}

fn watch_path_for_agent_dir(agent_dir: &Path) -> Option<AgentWatchPath> {
    if agent_dir.exists() {
        return Some(AgentWatchPath {
            path: agent_dir.to_path_buf(),
            recursive: true,
        });
    }

    let parent = agent_dir.parent()?;
    if parent.exists() {
        return Some(AgentWatchPath {
            path: parent.to_path_buf(),
            recursive: false,
        });
    }

    let grand_parent = parent.parent()?;
    if grand_parent.exists() {
        return Some(AgentWatchPath {
            path: grand_parent.to_path_buf(),
            recursive: false,
        });
    }

    None
}

fn merge_agents_dir(
    registry: &mut AgentProfileRegistry,
    dir: &Path,
) -> Result<(), AgentLoaderError> {
    if !dir.exists() {
        return Ok(());
    }

    let mut entries = fs::read_dir(dir)
        .map_err(|source| AgentLoaderError::ReadDir {
            path: dir.display().to_string(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AgentLoaderError::ReadDir {
            path: dir.display().to_string(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if !is_supported_agent_file(&path) {
            continue;
        }

        let content = fs::read_to_string(&path).map_err(|source| AgentLoaderError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
        let profile = parse_agent_file(&path, &content)?;
        registry.insert(profile);
    }

    Ok(())
}

fn is_supported_agent_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("md") | Some("markdown") | Some("yml") | Some("yaml")
    )
}

fn parse_agent_file(path: &Path, content: &str) -> Result<AgentProfile, AgentLoaderError> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("md") | Some("markdown") => parse_agent_markdown(path, content),
        Some("yml") | Some("yaml") => parse_agent_yaml(path, content),
        _ => Err(AgentLoaderError::InvalidFrontmatter {
            path: path.display().to_string(),
            message: "unsupported agent file extension".to_string(),
        }),
    }
}

fn parse_agent_markdown(path: &Path, content: &str) -> Result<AgentProfile, AgentLoaderError> {
    let (metadata, body) = split_frontmatter(path, content)?;
    build_agent_profile(path, metadata, Some(body))
}

fn parse_agent_yaml(path: &Path, content: &str) -> Result<AgentProfile, AgentLoaderError> {
    let metadata: AgentFrontmatter =
        serde_yaml::from_str(&normalize_text(content)).map_err(|source| {
            AgentLoaderError::ParseFrontmatter {
                path: path.display().to_string(),
                source,
            }
        })?;
    build_agent_profile(path, metadata, None)
}

fn build_agent_profile(
    path: &Path,
    metadata: AgentFrontmatter,
    markdown_body: Option<String>,
) -> Result<AgentProfile, AgentLoaderError> {
    let AgentFrontmatter {
        name,
        description,
        prompt,
        system_prompt,
    } = metadata;

    let fallback_name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("agent")
        .trim()
        .to_string();
    let declared_name = name.unwrap_or(fallback_name).trim().to_string();
    if declared_name.is_empty() {
        return Err(AgentLoaderError::InvalidFrontmatter {
            path: path.display().to_string(),
            message: "name cannot be empty".to_string(),
        });
    }

    let id = normalize_agent_id(&declared_name);
    if id.is_empty() {
        return Err(AgentLoaderError::InvalidFrontmatter {
            path: path.display().to_string(),
            message: "name must contain at least one visible character".to_string(),
        });
    }

    let description = description.unwrap_or_default().trim().to_string();
    if description.is_empty() {
        return Err(AgentLoaderError::InvalidFrontmatter {
            path: path.display().to_string(),
            message: "description cannot be empty".to_string(),
        });
    }

    let system_prompt = markdown_body
        .map(|body| body.trim().to_string())
        .filter(|body| !body.is_empty())
        .or_else(|| {
            system_prompt
                .or(prompt)
                .map(|prompt| prompt.trim().to_string())
                .filter(|prompt| !prompt.is_empty())
        });

    Ok(AgentProfile {
        id,
        name: declared_name,
        description,
        mode: AgentMode::SubAgent,
        system_prompt,
        // Loader 只消费 Claude 风格 agent 定义里的稳定字段；
        // 模型选择继续交给上层 runtime 配置，避免把私有 frontmatter 扩散成事实标准。
        model_preference: None,
    })
}

fn normalize_text(content: &str) -> String {
    content
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn split_frontmatter(
    path: &Path,
    content: &str,
) -> Result<(AgentFrontmatter, String), AgentLoaderError> {
    let normalized = normalize_text(content);
    let mut lines = normalized.lines();
    if lines.next() != Some("---") {
        return Err(AgentLoaderError::MissingFrontmatter {
            path: path.display().to_string(),
        });
    }

    let rest = lines.collect::<Vec<_>>();
    let mut parse_error = None;
    for (index, line) in rest.iter().enumerate() {
        if *line != "---" && *line != "..." {
            continue;
        }
        if rest
            .get(index + 1)
            .is_some_and(|next_line| next_line.starts_with(' ') || next_line.starts_with('\t'))
        {
            continue;
        }

        let frontmatter = rest[..index].join("\n");
        let metadata: AgentFrontmatter = match serde_yaml::from_str(&frontmatter) {
            Ok(metadata) => metadata,
            Err(error) => {
                parse_error = Some(error);
                continue;
            },
        };
        let body = rest[index + 1..].join("\n");
        return Ok((metadata, body));
    }

    if let Some(source) = parse_error {
        return Err(AgentLoaderError::ParseFrontmatter {
            path: path.display().to_string(),
            source,
        });
    }

    Err(AgentLoaderError::MissingFrontmatter {
        path: path.display().to_string(),
    })
}

fn normalize_agent_id(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut last_was_separator = false;

    for ch in value.chars() {
        if ch.is_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
            last_was_separator = false;
            continue;
        }

        if !last_was_separator {
            normalized.push('-');
            last_was_separator = true;
        }
    }

    normalized.trim_matches('-').to_string()
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AgentFrontmatter {
    name: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
    system_prompt: Option<String>,
}

#[cfg(test)]
mod tests {
    use astrcode_core::{AgentMode, AgentProfile, test_support::TestEnvGuard};

    use super::{AgentProfileLoader, AgentProfileRegistry};

    #[test]
    fn builtin_defaults_are_available() {
        let registry = AgentProfileRegistry::with_builtin_defaults();
        assert!(registry.get("explore").is_some());
        assert!(registry.get("reviewer").is_some());
    }

    #[test]
    fn list_subagent_profiles_filters_out_primary_only_profiles() {
        let mut registry = AgentProfileRegistry::new();
        registry.insert(AgentProfile {
            id: "primary".to_string(),
            name: "Primary".to_string(),
            description: "root only".to_string(),
            mode: AgentMode::Primary,
            system_prompt: None,
            model_preference: None,
        });
        registry.insert(AgentProfile {
            id: "subagent".to_string(),
            name: "Subagent".to_string(),
            description: "subagent".to_string(),
            mode: AgentMode::SubAgent,
            system_prompt: None,
            model_preference: None,
        });
        registry.insert(AgentProfile {
            id: "all".to_string(),
            name: "All".to_string(),
            description: "all modes".to_string(),
            mode: AgentMode::All,
            system_prompt: None,
            model_preference: None,
        });

        let ids = registry
            .list_subagent_profiles()
            .into_iter()
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["all", "subagent"]);
    }

    #[test]
    fn load_for_working_dir_merges_user_and_project_agent_dirs() {
        let guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");

        let user_claude_dir = guard.home_dir().join(".claude").join("agents");
        std::fs::create_dir_all(&user_claude_dir).expect("user claude agents dir should exist");
        std::fs::write(
            user_claude_dir.join("reviewer.md"),
            r#"---
name: reviewer
description: User-level reviewer
tools: ["Read", "Grep"]
---
Check the patch carefully.
"#,
        )
        .expect("user agent should be written");

        let project_astrcode_dir = project.path().join(".astrcode").join("agents");
        std::fs::create_dir_all(&project_astrcode_dir)
            .expect("project astrcode agents dir should exist");
        std::fs::write(
            project_astrcode_dir.join("reviewer.md"),
            r#"---
name: reviewer
description: Project-level reviewer
tools: ["Read", "Grep", "Bash"]
disallowedTools: ["Bash"]
---
Prefer repository-local conventions first.
"#,
        )
        .expect("project agent should be written");

        let loader = AgentProfileLoader::new().expect("loader should initialize");
        let registry = loader
            .load_for_working_dir(Some(project.path()))
            .expect("agent definitions should load");

        let reviewer = registry.get("reviewer").expect("reviewer should exist");
        assert_eq!(reviewer.description, "Project-level reviewer");
        assert_eq!(reviewer.model_preference, None);
        assert!(
            reviewer
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("repository-local conventions"))
        );
    }

    #[test]
    fn load_for_working_dir_walks_project_ancestor_chain() {
        let _guard = TestEnvGuard::new();
        let workspace = tempfile::tempdir().expect("tempdir should be created");
        let nested = workspace.path().join("apps").join("desktop");
        std::fs::create_dir_all(&nested).expect("nested dir should exist");

        let repo_agents = workspace.path().join(".astrcode").join("agents");
        std::fs::create_dir_all(&repo_agents).expect("repo agents dir should exist");
        std::fs::write(
            repo_agents.join("planner.md"),
            r#"---
name: planner
description: Repo planner
tools: ["readFile"]
---
Prefer the repo root defaults.
"#,
        )
        .expect("repo agent should be written");

        let nested_agents = nested.join(".astrcode").join("agents");
        std::fs::create_dir_all(&nested_agents).expect("nested agents dir should exist");
        std::fs::write(
            nested_agents.join("planner.md"),
            r#"---
name: planner
description: Nested planner
tools: ["readFile", "grep"]
---
Prefer the nested project defaults.
"#,
        )
        .expect("nested agent should be written");

        let loader = AgentProfileLoader::new().expect("loader should initialize");
        let registry = loader
            .load_for_working_dir(Some(&nested))
            .expect("ancestor chain should load");
        let planner = registry.get("planner").expect("planner should exist");

        assert_eq!(planner.description, "Nested planner");
    }

    #[test]
    fn load_for_working_dir_keeps_builtins_when_no_external_agents_exist() {
        let _guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");
        // loader 必须在 guard 之后创建，这样才能读取到测试环境的 home 目录
        let loader = AgentProfileLoader::new().expect("loader should initialize");

        let registry = loader
            .load_for_working_dir(Some(project.path()))
            .expect("builtin agents should still load");

        assert!(registry.get("explore").is_some());
        assert!(registry.get("execute").is_some());

        // search_dirs 可能包含项目祖先链中存在的 agents 目录
        // 但不应该包含不存在的临时测试目录
        let search_dirs = loader.search_dirs(Some(project.path()));
        for dir in &search_dirs {
            assert!(
                dir.exists(),
                "search_dirs should only include existing directories, but got non-existent: \
                 {dir:?}"
            );
        }
    }

    #[test]
    fn watch_paths_include_project_ancestor_chain() {
        let _guard = TestEnvGuard::new();
        let workspace = tempfile::tempdir().expect("tempdir should be created");
        let nested = workspace.path().join("apps").join("desktop");
        std::fs::create_dir_all(&nested).expect("nested dir should exist");

        let repo_agents = workspace.path().join(".astrcode").join("agents");
        std::fs::create_dir_all(&repo_agents).expect("repo agents dir should exist");

        let loader = AgentProfileLoader::new().expect("loader should initialize");
        let watch_paths = loader.watch_paths(Some(&nested));

        assert!(
            watch_paths
                .iter()
                .any(|target| target.path == repo_agents && target.recursive)
        );
        assert!(
            watch_paths
                .iter()
                .any(|target| { target.path == nested && !target.recursive })
        );
    }

    #[test]
    fn load_rejects_missing_description() {
        let _guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");
        let project_claude_dir = project.path().join(".claude").join("agents");
        std::fs::create_dir_all(&project_claude_dir)
            .expect("project claude agents dir should exist");
        std::fs::write(
            project_claude_dir.join("broken.md"),
            r#"---
name: broken
tools: ["Read"]
---
No description here.
"#,
        )
        .expect("broken agent should be written");

        let loader = AgentProfileLoader::new().expect("loader should initialize");
        let error = loader
            .load_for_working_dir(Some(project.path()))
            .expect_err("missing description should fail");
        assert!(
            error.to_string().contains("description cannot be empty"),
            "unexpected loader error: {error}"
        );
    }

    #[test]
    fn load_accepts_claude_style_csv_tools_frontmatter() {
        let _guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");
        let project_claude_dir = project.path().join(".claude").join("agents");
        std::fs::create_dir_all(&project_claude_dir)
            .expect("project claude agents dir should exist");
        std::fs::write(
            project_claude_dir.join("safe-researcher.md"),
            r#"---
name: safe-researcher
description: Research agent with restricted capabilities
tools: Read, Grep, Glob, Bash
---
"#,
        )
        .expect("agent definition should be written");

        let loader = AgentProfileLoader::new().expect("loader should initialize");
        let registry = loader
            .load_for_working_dir(Some(project.path()))
            .expect("claude-style frontmatter should load");
        let agent = registry
            .get("safe-researcher")
            .expect("safe-researcher profile should exist");

        assert_eq!(agent.name, "safe-researcher");
    }

    #[test]
    fn load_accepts_yaml_list_tools_with_quotes() {
        let _guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");
        let project_claude_dir = project.path().join(".claude").join("agents");
        std::fs::create_dir_all(&project_claude_dir)
            .expect("project claude agents dir should exist");
        std::fs::write(
            project_claude_dir.join("executor.md"),
            r#"---
name: executor
description: Executes targeted changes
tools: ["readFile", "writeFile", "editFile", "shell"]
---
"#,
        )
        .expect("agent definition should be written");

        let loader = AgentProfileLoader::new().expect("loader should initialize");
        let registry = loader
            .load_for_working_dir(Some(project.path()))
            .expect("quoted YAML tool list should load");
        let agent = registry
            .get("executor")
            .expect("executor profile should exist");
        assert_eq!(agent.name, "executor");
    }

    #[test]
    fn load_accepts_pure_yaml_agent_file() {
        let _guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");
        let project_astrcode_dir = project.path().join(".astrcode").join("agents");
        std::fs::create_dir_all(&project_astrcode_dir)
            .expect("project astrcode agents dir should exist");
        std::fs::write(
            project_astrcode_dir.join("planner.yaml"),
            r#"name: planner
description: Plans work before editing
tools: ["readFile", "grep"]
systemPrompt: |
  Read the codebase first.
  Then write a plan.
"#,
        )
        .expect("yaml agent definition should be written");

        let loader = AgentProfileLoader::new().expect("loader should initialize");
        let registry = loader
            .load_for_working_dir(Some(project.path()))
            .expect("yaml agent definition should load");
        let agent = registry
            .get("planner")
            .expect("planner profile should exist");
        assert!(
            agent
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("Then write a plan"))
        );
    }

    #[test]
    fn markdown_frontmatter_allows_literal_rule_lines_inside_yaml_blocks() {
        let _guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");
        let project_claude_dir = project.path().join(".claude").join("agents");
        std::fs::create_dir_all(&project_claude_dir)
            .expect("project claude agents dir should exist");
        std::fs::write(
            project_claude_dir.join("writer.md"),
            r#"---
name: writer
description: Handles prompts with literal separators
systemPrompt: |
  Keep this literal rule:
  ---
  Do not truncate here.
tools: ["readFile"]
---
"#,
        )
        .expect("markdown agent definition should be written");

        let loader = AgentProfileLoader::new().expect("loader should initialize");
        let registry = loader
            .load_for_working_dir(Some(project.path()))
            .expect("frontmatter with literal separators should load");
        let agent = registry.get("writer").expect("writer profile should exist");

        assert!(
            agent
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("Do not truncate here."))
        );
    }

    #[test]
    fn load_rejects_unclosed_frontmatter() {
        let _guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");
        let project_claude_dir = project.path().join(".claude").join("agents");
        std::fs::create_dir_all(&project_claude_dir)
            .expect("project claude agents dir should exist");
        std::fs::write(
            project_claude_dir.join("broken.md"),
            r#"---
name: broken
description: missing closing marker
tools: ["readFile"]
"#,
        )
        .expect("broken agent should be written");

        let loader = AgentProfileLoader::new().expect("loader should initialize");
        let error = loader
            .load_for_working_dir(Some(project.path()))
            .expect_err("unclosed frontmatter should fail");
        assert!(
            error.to_string().contains("missing YAML frontmatter"),
            "unexpected loader error: {error}"
        );
    }

    #[test]
    fn load_rejects_missing_frontmatter() {
        let _guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");
        let project_claude_dir = project.path().join(".claude").join("agents");
        std::fs::create_dir_all(&project_claude_dir)
            .expect("project claude agents dir should exist");
        std::fs::write(project_claude_dir.join("broken.md"), "just text")
            .expect("broken agent should be written");

        let loader = AgentProfileLoader::new().expect("loader should initialize");
        let error = loader
            .load_for_working_dir(Some(project.path()))
            .expect_err("missing frontmatter should fail");
        assert!(
            error.to_string().contains("missing YAML frontmatter"),
            "unexpected loader error: {error}"
        );
    }
}
