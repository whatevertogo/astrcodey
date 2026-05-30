//! Claude-style skill discovery and the bundled `Skill` tool.
//!
//! Skills stay outside the core agent loop. This extension contributes a small
//! prompt index during `PromptBuild`, then lets the model load the full
//! `SKILL.md` content only when a matching task appears.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use astrcode_extension_sdk::{
    extension::{
        CommandContext, CommandDiscoveryHandler, CommandHandler, Extension, ExtensionCapability,
        ExtensionCommandResult, ExtensionError, PromptBuildContext, PromptBuildHandler,
        PromptContributions, Registrar, ToolHandler,
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult, tool_metadata},
};
use astrcode_support::{frontmatter, hostpaths};
use serde::Deserialize;
use serde_json::{Value, json};

pub const SKILL_TOOL_NAME: &str = "Skill";
const SKILL_FILE_NAME: &str = "SKILL.md";
const MAX_INDEX_CHARS: usize = 8_000;
const MAX_DESCRIPTION_CHARS: usize = 250;
const SKILL_NAME_TAG: &str = "skill-name";
const SKILL_ARGS_TAG: &str = "skill-args";

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(SkillExtension)
}

struct SkillExtension;

#[async_trait::async_trait]
impl Extension for SkillExtension {
    fn id(&self) -> &str {
        "astrcode-skill"
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        &[ExtensionCapability::WorkspaceRead]
    }

    fn register(&self, reg: &mut Registrar) {
        let shared = Arc::new(SkillShared::new());
        reg.tool(
            skill_tool_definition(),
            Arc::new(SkillToolHandler {
                shared: shared.clone(),
            }),
        );
        reg.tool_metadata(skill_tool_metadata());
        reg.command_discovery(Arc::new(SkillCommandDiscovery {
            shared: shared.clone(),
        }));
        reg.on_prompt_build(
            0,
            Arc::new(SkillPromptBuildHandler {
                shared: shared.clone(),
            }),
        );
    }
}

// ─── Shared Cache ───────────────────────────────────────────────────────

/// Skill 发现结果缓存，按 working_dir 缓存。
struct SkillShared {
    cache: Mutex<HashMap<String, Vec<SkillDefinition>>>,
}

impl SkillShared {
    fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn get_or_discover(&self, working_dir: &str) -> Vec<SkillDefinition> {
        if let Some(skills) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(working_dir)
        {
            return skills.clone();
        }
        let skills = discover_skills(working_dir);
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(working_dir.to_string())
            .or_insert_with(|| skills.clone());
        skills
    }
}

struct SkillToolHandler {
    shared: Arc<SkillShared>,
}

#[async_trait::async_trait]
impl ToolHandler for SkillToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        working_dir: &str,
        ctx: &astrcode_extension_sdk::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != SKILL_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }

        Ok(handle_skill_tool(
            arguments,
            working_dir,
            ctx.session_id.as_str(),
            &self.shared,
        ))
    }
}

struct SkillPromptBuildHandler {
    shared: Arc<SkillShared>,
}

#[async_trait::async_trait]
impl PromptBuildHandler for SkillPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let has_skill_tool = ctx.tools.iter().any(|t| t.name == SKILL_TOOL_NAME);
        if !has_skill_tool {
            return Ok(PromptContributions::default());
        }

        let skills = self.shared.get_or_discover(&ctx.working_dir);
        Ok(PromptContributions {
            skills: vec![format_skills_for_model(&skills)],
            ..Default::default()
        })
    }
}

struct SkillCommandDiscovery {
    shared: Arc<SkillShared>,
}

#[async_trait::async_trait]
impl CommandDiscoveryHandler for SkillCommandDiscovery {
    async fn discover(
        &self,
        working_dir: &str,
    ) -> Vec<(
        astrcode_extension_sdk::extension::SlashCommand,
        Arc<dyn CommandHandler>,
    )> {
        self.shared
            .get_or_discover(working_dir)
            .into_iter()
            .map(|skill| {
                let description =
                    truncate_for_index(&skill.index_description(), MAX_DESCRIPTION_CHARS);
                let cmd = astrcode_extension_sdk::extension::SlashCommand {
                    name: skill.id.clone(),
                    description,
                    args_schema: None,
                };
                (
                    cmd,
                    Arc::new(SkillCommandHandler {
                        skill_id: skill.id,
                        shared: self.shared.clone(),
                    }) as Arc<dyn CommandHandler>,
                )
            })
            .collect()
    }
}

struct SkillCommandHandler {
    skill_id: String,
    shared: Arc<SkillShared>,
}

#[async_trait::async_trait]
impl CommandHandler for SkillCommandHandler {
    async fn execute(
        &self,
        _command_name: &str,
        arguments: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let skills = self.shared.get_or_discover(working_dir);
        let Some(skill) = skills
            .iter()
            .find(|skill| skill.matches_requested_name(&self.skill_id))
        else {
            return Err(ExtensionError::NotFound(self.skill_id.clone()));
        };

        Ok(ExtensionCommandResult::start_turn(render_skill_content(
            skill,
            Some(arguments),
            &ctx.session_id,
        )))
    }
}

fn skill_tool_metadata()
-> std::collections::HashMap<String, astrcode_extension_sdk::tool::ToolPromptMetadata> {
    let mut map = std::collections::HashMap::new();
    map.insert(
        SKILL_TOOL_NAME.to_string(),
        astrcode_extension_sdk::tool::ToolPromptMetadata::new(String::new())
            .caveat("Users may also refer to skills as slash commands, e.g. `/commit`.")
            .caveat(
                "If the skill was not found, pick from the listed [Skills] names — do not retry \
                 with a guessed name.",
            )
            .example("Task matches `/commit` in [Skills] → Skill(\"commit\"), not ad-hoc prose.")
            .prompt_tag(astrcode_extension_sdk::tool::ToolPromptTag::Discovery),
    );
    map
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillDefinition {
    id: String,
    display_name: Option<String>,
    description: String,
    when_to_use: Option<String>,
    guide: String,
    skill_root: PathBuf,
    asset_files: Vec<String>,
    source: SkillSource,
}

impl SkillDefinition {
    fn matches_requested_name(&self, requested: &str) -> bool {
        normalize_skill_request(requested) == self.id
    }

    fn index_description(&self) -> String {
        match self
            .when_to_use
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            Some(when_to_use) => format!("{} - {}", self.description, when_to_use.trim()),
            None => self.description.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillSource {
    UserClaude,
    UserAstrcode,
    ProjectClaude,
    ProjectAstrcode,
}

impl SkillSource {
    fn label(self) -> &'static str {
        match self {
            Self::UserClaude => "user:.claude",
            Self::UserAstrcode => "user:.astrcode",
            Self::ProjectClaude => "project:.claude",
            Self::ProjectAstrcode => "project:.astrcode",
        }
    }
}

#[derive(Debug)]
struct SkillRoot {
    dir: PathBuf,
    source: SkillSource,
}

#[derive(Debug, Default, Deserialize)]
struct RawSkillFrontmatter {
    name: Option<serde_yaml::Value>,
    description: Option<serde_yaml::Value>,
    when_to_use: Option<serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillToolArgs {
    skill: String,
    #[serde(default)]
    args: Option<String>,
}

fn skill_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: SKILL_TOOL_NAME.into(),
        description: "Load a named skill's instructions into the conversation. The skill's rules \
                      govern your subsequent behavior until the skill completes.\n\nWhen NOT to \
                      use:\n- No [Skills] entry matches the task\n- Simple one-shot work with no \
                      skill-specific workflow\n\nTips:\n- Task matches a [Skills] description or \
                      when_to_use\n- User invokes a slash command (e.g. `/commit`)\n\nUse the \
                      exact skill name from [Skills]. Optional `args` are forwarded to the skill."
            .into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Skill name, e.g. \"commit\" or \"/commit\". Must match an entry from [Skills]."
                },
                "args": {
                    "type": "string",
                    "description": "Optional free-form arguments forwarded to the skill."
                }
            },
            "required": ["skill"]
        }),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    }
}

fn handle_skill_tool(
    arguments: Value,
    working_dir: &str,
    session_id: &str,
    shared: &SkillShared,
) -> ToolResult {
    let args = match serde_json::from_value::<SkillToolArgs>(arguments) {
        Ok(args) => args,
        Err(error) => {
            let msg = format!("invalid Skill input: {error}");
            return ToolResult {
                call_id: String::new(),
                // content 必须非空,LLM 只读 content,不读 error 字段。
                content: msg.clone(),
                is_error: true,
                error: Some(msg),
                metadata: BTreeMap::new(),
                duration_ms: None,
            };
        },
    };

    let skills = shared.get_or_discover(working_dir);
    let Some(skill) = skills
        .iter()
        .find(|skill| skill.matches_requested_name(&args.skill))
    else {
        let available = skills
            .iter()
            .map(|skill| skill.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let msg = format!(
            "unknown skill '{}'. Available skills: {}",
            normalize_skill_request(&args.skill),
            available
        );
        return ToolResult {
            call_id: String::new(),
            content: msg.clone(),
            is_error: true,
            error: Some(msg),
            metadata: tool_metadata([("availableSkills", json!(available))]),
            duration_ms: None,
        };
    };

    ToolResult::text(
        render_skill_content(skill, args.args.as_deref(), session_id),
        false,
        tool_metadata([
            ("skill", json!(skill.id)),
            ("source", json!(skill.source.label())),
        ]),
    )
}

fn discover_skills(working_dir: &str) -> Vec<SkillDefinition> {
    let home_dir = hostpaths::resolve_home_dir();
    discover_skills_with_home(Path::new(working_dir), Some(&home_dir))
}

fn discover_skills_with_home(working_dir: &Path, home_dir: Option<&Path>) -> Vec<SkillDefinition> {
    let roots = skill_roots(working_dir, home_dir);
    let mut skills = Vec::new();
    for root in roots {
        merge_skill_layer(&mut skills, load_skills_from_root(&root));
    }
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    skills
}

fn skill_roots(working_dir: &Path, home_dir: Option<&Path>) -> Vec<SkillRoot> {
    let mut roots = Vec::new();
    let user_dirs = home_dir.map(user_skill_dirs).unwrap_or_default();

    if let Some([claude, astrcode]) = user_dirs.get(0..2) {
        roots.push(SkillRoot {
            dir: claude.clone(),
            source: SkillSource::UserClaude,
        });
        roots.push(SkillRoot {
            dir: astrcode.clone(),
            source: SkillSource::UserAstrcode,
        });
    }

    let mut ancestors = working_dir.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        for (dir, source) in [
            (
                ancestor.join(".claude").join("skills"),
                SkillSource::ProjectClaude,
            ),
            (
                ancestor.join(".astrcode").join("skills"),
                SkillSource::ProjectAstrcode,
            ),
        ] {
            if user_dirs.iter().any(|user_dir| user_dir == &dir) {
                continue;
            }
            roots.push(SkillRoot { dir, source });
        }
    }

    roots
}

fn user_skill_dirs(home_dir: &Path) -> Vec<PathBuf> {
    vec![
        home_dir.join(".claude").join("skills"),
        home_dir.join(".astrcode").join("skills"),
    ]
}

fn load_skills_from_root(root: &SkillRoot) -> Vec<SkillDefinition> {
    let entries = match fs::read_dir(&root.dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    entries
        .into_iter()
        .filter_map(|entry| load_skill_dir(entry.path(), root.source))
        .collect()
}

fn load_skill_dir(skill_dir: PathBuf, source: SkillSource) -> Option<SkillDefinition> {
    if !skill_dir.is_dir() {
        return None;
    }

    let skill_path = skill_dir.join(SKILL_FILE_NAME);
    if !skill_path.is_file() {
        return None;
    }

    let id = skill_dir.file_name()?.to_string_lossy().to_string();
    if !is_valid_skill_id(&id) {
        return None;
    }

    let content = fs::read_to_string(skill_path).ok()?;
    parse_skill_md(&content, &id, skill_dir, source)
}

fn parse_skill_md(
    content: &str,
    id: &str,
    skill_root: PathBuf,
    source: SkillSource,
) -> Option<SkillDefinition> {
    let normalized = normalize_skill_content(content);
    let (frontmatter, body) = frontmatter::split_frontmatter(&normalized)?;
    let raw = serde_yaml::from_str::<RawSkillFrontmatter>(frontmatter).ok()?;
    let guide = body.trim().to_string();
    if guide.is_empty() {
        return None;
    }

    let description = frontmatter::yaml_string_value(raw.description.as_ref())
        .filter(|text| !text.trim().is_empty())
        .or_else(|| extract_description_from_markdown(&guide))?;

    Some(SkillDefinition {
        id: id.to_string(),
        display_name: frontmatter::yaml_string_value(raw.name.as_ref()).filter(|name| name != id),
        description,
        when_to_use: frontmatter::yaml_string_value(raw.when_to_use.as_ref()),
        guide,
        asset_files: collect_asset_files(&skill_root),
        skill_root,
        source,
    })
}

fn normalize_skill_content(content: &str) -> String {
    content
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn extract_description_from_markdown(markdown: &str) -> Option<String> {
    markdown
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches('#').trim())
        .filter(|line| !line.is_empty())
        .map(|line| truncate_for_index(line, MAX_DESCRIPTION_CHARS))
}

fn collect_asset_files(skill_dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_asset_files_recursive(skill_dir, skill_dir, &mut files);
    files.retain(|path| path != SKILL_FILE_NAME);
    files.sort();
    files
}

fn collect_asset_files_recursive(root: &Path, base_dir: &Path, files: &mut Vec<String>) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_asset_files_recursive(&path, base_dir, files);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if let Ok(relative) = path.strip_prefix(base_dir) {
            files.push(normalize_path(relative));
        }
    }
}

fn merge_skill_layer(base: &mut Vec<SkillDefinition>, overrides: Vec<SkillDefinition>) {
    for skill in overrides {
        if let Some(existing) = base.iter_mut().find(|candidate| candidate.id == skill.id) {
            *existing = skill;
        } else {
            base.push(skill);
        }
    }
}

fn format_skills_for_model(skills: &[SkillDefinition]) -> String {
    if skills.is_empty() {
        return "No skills are configured.".to_string();
    }

    let mut output = String::from(
        "When a task matches one of these skills, calling the Skill tool with the exact skill \
         name is required before continuing. Users may also refer to skills as slash commands, \
         such as /commit.\n",
    );
    for skill in skills {
        let display = skill
            .display_name
            .as_deref()
            .filter(|name| *name != skill.id)
            .map(|name| format!(" ({name})"))
            .unwrap_or_default();
        let description = truncate_for_index(&skill.index_description(), MAX_DESCRIPTION_CHARS);
        let line = format!("- {}{}: {}\n", skill.id, display, description);
        if output.len() + line.len() > MAX_INDEX_CHARS {
            output.push_str("- ... additional skills omitted from the index\n");
            break;
        }
        output.push_str(&line);
    }
    output.trim_end().to_string()
}

fn render_skill_content(skill: &SkillDefinition, args: Option<&str>, session_id: &str) -> String {
    let mut sections = Vec::new();
    sections.push(format!("<{SKILL_NAME_TAG}>{}</{SKILL_NAME_TAG}>", skill.id));
    if let Some(args) = args.filter(|args| !args.trim().is_empty()) {
        sections.push(format!(
            "<{SKILL_ARGS_TAG}>{}</{SKILL_ARGS_TAG}>",
            args.trim()
        ));
    }

    sections.push(format!("Skill: {}", skill.id));
    sections.push(format!("Description: {}", skill.description.trim()));

    if let Some(args) = args.filter(|args| !args.trim().is_empty()) {
        sections.push(format!("Invocation arguments: {}", args.trim()));
    }

    let skill_root = normalize_path(&skill.skill_root);
    sections.push(format!("Base directory for this skill: {skill_root}"));

    let mut guide = skill.guide.clone();
    guide = substitute_skill_variables(&guide, &skill_root, session_id);
    sections.push(guide.trim().to_string());

    if !skill.asset_files.is_empty() {
        let files = skill
            .asset_files
            .iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("Available skill files:\n{files}"));
    }

    sections.join("\n\n")
}

fn substitute_skill_variables(guide: &str, skill_root: &str, session_id: &str) -> String {
    let mut content = guide.replace("${SKILL_DIR}", skill_root);
    content = content.replace("${SESSION_ID}", session_id);
    content = content.replace("${CLAUDE_SKILL_DIR}", skill_root);
    content = content.replace("${CLAUDE_SESSION_ID}", session_id);
    content
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn normalize_skill_request(raw: &str) -> String {
    raw.trim().trim_start_matches('/').to_ascii_lowercase()
}

fn is_valid_skill_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && !bytes.starts_with(b"-")
        && !bytes.ends_with(b"-")
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == &b'-')
}

fn truncate_for_index(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    format!("{}...", text.chars().take(keep).collect::<String>())
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::{
        config::ModelSelection,
        extension::CommandContext,
        tool::{ToolCapabilities, ToolExecutionContext},
    };

    use super::*;

    fn write_skill(root: &Path, name: &str, skill_md: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("skill dir");
        fs::write(dir.join(SKILL_FILE_NAME), skill_md).expect("skill md");
        dir
    }

    fn sample_md(description: &str, body: &str) -> String {
        format!("---\ndescription: {description}\n---\n{body}")
    }

    #[test]
    fn parses_claude_style_skill_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let skill_dir = write_skill(
            temp.path(),
            "repo-search",
            "---\nname: Repo Search\ndescription: Search the repository.\nwhen_to_use: When the \
             task mentions files.\nextra: ignored\n---\nUse ${CLAUDE_SKILL_DIR}.",
        );

        let skill = load_skill_dir(skill_dir, SkillSource::UserClaude).expect("skill");

        assert_eq!(skill.id, "repo-search");
        assert_eq!(skill.display_name.as_deref(), Some("Repo Search"));
        assert_eq!(skill.description, "Search the repository.");
        assert_eq!(
            skill.when_to_use.as_deref(),
            Some("When the task mentions files.")
        );
    }

    #[test]
    fn discovers_user_and_project_astrcode_and_claude_roots() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let workspace = temp.path().join("workspace");
        let nested = workspace.join("packages").join("app");
        fs::create_dir_all(&nested).expect("nested");

        write_skill(
            &home.join(".claude").join("skills"),
            "shared",
            &sample_md("user claude", "User Claude"),
        );
        write_skill(
            &home.join(".astrcode").join("skills"),
            "shared",
            &sample_md("user astrcode", "User Astrcode"),
        );
        write_skill(
            &workspace.join(".claude").join("skills"),
            "shared",
            &sample_md("project claude", "Project Claude"),
        );
        write_skill(
            &nested.join(".astrcode").join("skills"),
            "nested-only",
            &sample_md("nested astrcode", "Nested Astrcode"),
        );

        let skills = discover_skills_with_home(&nested, Some(&home));

        assert_eq!(
            skills
                .iter()
                .find(|skill| skill.id == "shared")
                .map(|skill| skill.description.as_str()),
            Some("project claude")
        );
        assert!(skills.iter().any(|skill| skill.id == "nested-only"));
    }

    #[test]
    fn same_project_level_astrcode_overrides_claude() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");

        write_skill(
            &workspace.join(".claude").join("skills"),
            "review",
            &sample_md("claude review", "Claude"),
        );
        write_skill(
            &workspace.join(".astrcode").join("skills"),
            "review",
            &sample_md("astrcode review", "Astrcode"),
        );

        let skills = discover_skills_with_home(&workspace, None);

        assert_eq!(
            skills
                .iter()
                .find(|skill| skill.id == "review")
                .map(|skill| skill.description.as_str()),
            Some("astrcode review")
        );
    }

    #[test]
    fn skill_tool_renders_content_with_paths_assets_and_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let skill_dir = write_skill(
            &workspace.join(".claude").join("skills"),
            "review",
            "---\ndescription: Review code.\n---\nRead ${SKILL_DIR} for ${SESSION_ID}.",
        );
        fs::create_dir_all(skill_dir.join("references")).expect("asset dir");
        fs::write(skill_dir.join("references").join("rules.md"), "rules").expect("asset");

        let shared = SkillShared::new();
        let result = handle_skill_tool(
            json!({ "skill": "/review", "args": "src/lib.rs" }),
            &workspace.to_string_lossy(),
            "session-123",
            &shared,
        );

        assert!(!result.is_error);
        assert!(result.content.contains("<skill-name>review</skill-name>"));
        assert!(
            result
                .content
                .contains("<skill-args>src/lib.rs</skill-args>")
        );
        assert!(result.content.contains("Skill: review"));
        assert!(result.content.contains("Invocation arguments: src/lib.rs"));
        assert!(result.content.contains("session-123"));
        assert!(result.content.contains("- references/rules.md"));
    }

    #[test]
    fn skill_variable_substitution_accepts_neutral_and_claude_aliases() {
        let output = substitute_skill_variables(
            "${SKILL_DIR} ${SESSION_ID} ${CLAUDE_SKILL_DIR} ${CLAUDE_SESSION_ID}",
            "/tmp/skill",
            "session-1",
        );

        assert_eq!(output, "/tmp/skill session-1 /tmp/skill session-1");
    }

    #[test]
    fn formats_index_with_blocking_instruction() {
        let temp = tempfile::tempdir().expect("tempdir");
        let skill_dir = write_skill(
            temp.path(),
            "commit",
            &sample_md("Commit changes.", "Commit guide"),
        );
        let skill = load_skill_dir(skill_dir, SkillSource::UserAstrcode).expect("skill");

        let index = format_skills_for_model(&[skill]);

        assert!(index.contains("calling the Skill tool"));
        assert!(index.contains("/commit"));
        assert!(index.contains("- commit: Commit changes."));
    }


    fn tool_ctx(working_dir: &Path) -> ToolExecutionContext {
        ToolExecutionContext {
            session_id: "session".into(),
            working_dir: working_dir.to_string_lossy().into_owned(),
            tool_call_id: None,
            event_tx: None,
            capabilities: ToolCapabilities::default(),
        }
    }

    #[tokio::test]
    async fn prompt_build_contributes_skill_index_when_tool_is_available() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        write_skill(
            &workspace.join(".astrcode").join("skills"),
            "commit",
            &sample_md("Commit changes.", "Commit guide"),
        );

        let handler = SkillPromptBuildHandler {
            shared: Arc::new(SkillShared::new()),
        };
        let ctx = PromptBuildContext {
            session_id: "test".into(),
            working_dir: workspace.to_string_lossy().into_owned(),
            model: astrcode_extension_sdk::config::ModelSelection::simple("mock"),
            tools: vec![skill_tool_definition()],
        };
        let contributions = handler.handle(ctx).await.expect("prompt build");

        assert!(contributions.skills[0].contains("- commit: Commit changes."));
    }

    #[tokio::test]
    async fn skills_are_registered_as_slash_commands_for_working_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        write_skill(
            &workspace.join(".astrcode").join("skills"),
            "reviewnow",
            &sample_md("Review current code.", "Review guide"),
        );

        let discovery = SkillCommandDiscovery {
            shared: Arc::new(SkillShared::new()),
        };
        let commands = discovery.discover(&workspace.to_string_lossy()).await;

        assert!(commands.iter().any(|(cmd, _)| {
            cmd.name == "reviewnow" && cmd.description == "Review current code."
        }));
    }

    #[tokio::test]
    async fn skill_slash_command_starts_turn_with_rendered_instructions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        write_skill(
            &workspace.join(".astrcode").join("skills"),
            "commit",
            &sample_md("Commit changes.", "Use ${SKILL_DIR} for ${SESSION_ID}."),
        );

        let handler = SkillCommandHandler {
            skill_id: "commit".into(),
            shared: Arc::new(SkillShared::new()),
        };
        let ctx = CommandContext {
            session_id: "session".into(),
            working_dir: workspace.to_string_lossy().into_owned(),
            model: ModelSelection::simple("mock"),
            session_store_dir: None,
        };
        let result = handler
            .execute("commit", "staged files", &workspace.to_string_lossy(), &ctx)
            .await
            .expect("skill command");

        let astrcode_extension_sdk::extension::ExtensionCommandResult::StartTurn { instructions } =
            result
        else {
            panic!("skill command should start a turn");
        };
        assert!(instructions.contains("<skill-name>commit</skill-name>"));
        assert!(instructions.contains("<skill-args>staged files</skill-args>"));
    }

    #[tokio::test]
    async fn extension_tool_uses_bound_working_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        write_skill(
            &workspace.join(".astrcode").join("skills"),
            "commit",
            &sample_md("Commit changes.", "Commit guide"),
        );

        let handler = SkillToolHandler {
            shared: Arc::new(SkillShared::new()),
        };
        let result = handler
            .execute(
                SKILL_TOOL_NAME,
                json!({ "skill": "commit" }),
                &workspace.to_string_lossy(),
                &tool_ctx(&workspace),
            )
            .await
            .expect("skill tool");

        assert!(!result.is_error);
        assert!(result.content.contains("Skill: commit"));
    }
}
