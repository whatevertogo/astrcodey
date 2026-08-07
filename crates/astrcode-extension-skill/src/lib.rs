//! Claude-style skill discovery and the bundled `Skill` tool.
//!
//! Skills stay outside the core agent loop. This extension contributes a small
//! prompt index during `PromptBuild`, then lets the model load the full
//! `SKILL.md` content only when a matching task appears.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_extension_sdk::{
    builder::{ExtensionToolDefinition, manifest},
    discovery::DiscoveryCache,
    extension::{
        CommandContext, CommandDiscovery, CommandDiscoveryContext, CommandDiscoveryHandler,
        CommandHandler, DiscoveredCommand, Extension, ExtensionCapability, ExtensionCommandResult,
        ExtensionError, ExtensionManifest, PromptBuildContext, PromptBuildHandler,
        PromptContributions, Registrar, ToolContext, ToolHandler,
    },
    frontmatter, hostpaths,
    tool::{
        ExecutionMode, ToolDefinition, ToolOrigin, ToolPromptMetadata, ToolPromptTag, ToolResult,
        tool_metadata,
    },
};
use noyalib::compat::serde_yaml as yaml;
use serde::Deserialize;
use serde_json::{Value, json};

const SKILL_TOOL_NAME: &str = "Skill";
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
    fn manifest(&self) -> ExtensionManifest {
        manifest("astrcode-skill")
            .version(env!("CARGO_PKG_VERSION"))
            .description(env!("CARGO_PKG_DESCRIPTION"))
            .capability(ExtensionCapability::WorkspaceRead)
            .build()
    }

    fn register(&self, reg: &mut Registrar) {
        let shared = Arc::new(SkillShared::new());
        reg.tool(
            ExtensionToolDefinition::from_definition(skill_tool_definition())
                .with_prompt(skill_tool_prompt()),
            Arc::new(SkillToolHandler {
                shared: shared.clone(),
            }),
        );
        reg.command_discovery(Arc::new(SkillCommandDiscovery {
            shared: shared.clone(),
        }));
        reg.on_prompt_build(0, Arc::new(SkillPromptBuildHandler { shared }));
    }
}

// ─── Shared Cache ───────────────────────────────────────────────────────

/// Skill 发现结果缓存，按 working_dir 缓存。
struct SkillShared {
    cache: DiscoveryCache<Vec<SkillDefinition>>,
}

impl SkillShared {
    fn new() -> Self {
        Self {
            cache: DiscoveryCache::new(),
        }
    }

    fn get_or_discover(&self, working_dir: &str) -> Vec<SkillDefinition> {
        self.cache
            .get_or_discover(working_dir, || discover_skills(working_dir))
    }
}

struct SkillToolHandler {
    shared: Arc<SkillShared>,
}

#[async_trait::async_trait]
impl ToolHandler for SkillToolHandler {
    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let tool_name = ctx.tool_name();
        if tool_name != SKILL_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }
        let working_dir = ctx
            .call()
            .require_working_dir()?
            .to_string_lossy()
            .into_owned();
        let session_id = ctx.call().require_session_id()?;

        Ok(handle_skill_tool(
            ctx.raw_arguments().clone(),
            &working_dir,
            session_id.as_str(),
            &self.shared,
        )
        .into())
    }
}

struct SkillPromptBuildHandler {
    shared: Arc<SkillShared>,
}

#[async_trait::async_trait]
impl PromptBuildHandler for SkillPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let has_skill_tool = ctx.tools().iter().any(|t| t.name == SKILL_TOOL_NAME);
        if !has_skill_tool {
            return Ok(PromptContributions::default());
        }

        let working_dir = ctx.call().require_working_dir()?;
        let working_dir = working_dir.to_string_lossy();
        let skills = self.shared.get_or_discover(&working_dir);
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
        ctx: CommandDiscoveryContext,
    ) -> Result<CommandDiscovery, ExtensionError> {
        let working_dir = ctx.call().require_working_dir()?;
        let working_dir = working_dir.to_string_lossy();
        let commands = self
            .shared
            .get_or_discover(&working_dir)
            .into_iter()
            .map(|skill| {
                let description =
                    truncate_for_index(&skill.index_description(), MAX_DESCRIPTION_CHARS);
                let cmd = astrcode_extension_sdk::extension::SlashCommand {
                    name: skill.id.clone(),
                    description,
                    args_schema: None,
                    requires_idle: false,
                    argument_completions: false,
                    priority: 0,
                };
                DiscoveredCommand::new(
                    cmd,
                    Arc::new(SkillCommandHandler {
                        skill_id: skill.id,
                        shared: self.shared.clone(),
                    }) as Arc<dyn CommandHandler>,
                )
            })
            .collect();
        Ok(CommandDiscovery::new(commands))
    }
}

struct SkillCommandHandler {
    skill_id: String,
    shared: Arc<SkillShared>,
}

#[async_trait::async_trait]
impl CommandHandler for SkillCommandHandler {
    async fn execute(&self, ctx: CommandContext) -> Result<ExtensionCommandResult, ExtensionError> {
        let working_dir = ctx.call().require_working_dir()?;
        let session_id = ctx.call().require_session_id()?;
        let working_dir = working_dir.to_string_lossy();
        let skills = self.shared.get_or_discover(&working_dir);
        let Some(skill) = skills
            .iter()
            .find(|skill| skill.matches_requested_name(&self.skill_id))
        else {
            return Err(ExtensionError::NotFound(self.skill_id.clone()));
        };

        Ok(ExtensionCommandResult::start_turn(render_skill_content(
            skill,
            Some(ctx.argument()),
            session_id.as_str(),
        )))
    }
}

fn skill_tool_prompt() -> ToolPromptMetadata {
    ToolPromptMetadata::new(String::new())
        .caveat("Users may also refer to skills as slash commands, e.g. `/commit`.")
        .caveat(
            "If the skill was not found, pick from the listed [Skills] names — do not retry with \
             a guessed name.",
        )
        .example("Task matches `/commit` in [Skills] → Skill(\"commit\"), not ad-hoc prose.")
        .prompt_tag(ToolPromptTag::Discovery)
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
    name: Option<String>,
    description: Option<String>,
    when_to_use: Option<String>,
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
        strict: true,
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
    let home_dir = hostpaths::user_home_dir();
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
    let raw = yaml::from_str::<RawSkillFrontmatter>(frontmatter).ok()?;
    let guide = body.trim().to_string();
    if guide.is_empty() {
        return None;
    }

    let description =
        trimmed_nonempty(raw.description).or_else(|| extract_description_from_markdown(&guide))?;

    Some(SkillDefinition {
        id: id.to_string(),
        display_name: trimmed_nonempty(raw.name).filter(|name| name != id),
        description,
        when_to_use: trimmed_nonempty(raw.when_to_use),
        guide,
        asset_files: collect_asset_files(&skill_root),
        skill_root,
        source,
    })
}

fn trimmed_nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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
        "These skills provide workflows for matching tasks. When one seems relevant, consider \
         calling the Skill tool with the exact skill name. Users may also refer to skills as \
         slash commands, such as /commit.\n",
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
        extension::{RuntimeHookCallContext, RuntimePromptBuildContext},
        testing::{CommandContextBuilder, ToolContextBuilder},
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
        let call = ToolContextBuilder::new("astrcode-skill", "fixture")
            .session("test", &workspace, None)
            .build()
            .call()
            .clone();
        let input = RuntimePromptBuildContext::new(
            RuntimeHookCallContext::new("test", &workspace, ModelSelection::simple("mock"), None),
            vec![skill_tool_definition()],
        );
        let ctx = PromptBuildContext::from_runtime(call, &input);
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
        let call = ToolContextBuilder::new("astrcode-skill", "fixture")
            .workspace(&workspace)
            .build()
            .call()
            .clone();
        let commands = discovery
            .discover(CommandDiscoveryContext::from_runtime(call, 1))
            .await
            .unwrap();

        assert!(commands.commands().iter().any(|command| {
            command.command().name == "reviewnow"
                && command.command().description == "Review current code."
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
        let ctx = CommandContextBuilder::new("astrcode-skill", "commit")
            .session("session", &workspace, None)
            .model(ModelSelection::simple("mock"))
            .argument("staged files")
            .build();
        let result = handler.execute(ctx).await.expect("skill command");

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
        let ctx = ToolContextBuilder::new("astrcode-skill", SKILL_TOOL_NAME)
            .session("session", &workspace, None)
            .arguments(json!({ "skill": "commit" }))
            .build();
        let result = handler.execute(ctx).await.expect("skill tool");

        assert!(!result.is_error);
        assert!(result.content.contains("Skill: commit"));
    }
}
