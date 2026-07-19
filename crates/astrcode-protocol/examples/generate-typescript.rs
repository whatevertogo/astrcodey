use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

use astrcode_protocol::{agent_session_link::AgentSessionLinkDto, http::*, wire::*};
use serde::Serialize;
use ts_rs::{Config, TS};

const GENERATED_RELATIVE_DIR: &str = "frontend/src/services/generated";
const STAGING_RELATIVE_DIR: &str = "target/protocol-types";

fn main() -> Result<(), Box<dyn Error>> {
    let check = parse_check_arg()?;
    let workspace_root = workspace_root()?;
    let generated_dir = workspace_root.join(GENERATED_RELATIVE_DIR);
    let staging_dir = workspace_root.join(STAGING_RELATIVE_DIR);

    recreate_dir(&staging_dir)?;
    export_types(&staging_dir)?;
    write_wire_values(&staging_dir)?;
    write_index(&staging_dir)?;
    normalize_generated_files(&staging_dir)?;

    if check {
        check_generated_files(&staging_dir, &generated_dir)?;
        println!("TypeScript protocol bindings are up to date");
    } else {
        replace_generated_files(&staging_dir, &generated_dir)?;
        println!("Generated TypeScript protocol bindings in {GENERATED_RELATIVE_DIR}");
    }
    Ok(())
}

fn parse_check_arg() -> Result<bool, io::Error> {
    let mut args = env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (None, None) => Ok(false),
        (Some("--check"), None) => Ok(true),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: generate-typescript [--check]",
        )),
    }
}

fn workspace_root() -> Result<PathBuf, io::Error> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("protocol crate is not inside the workspace"))
}

fn recreate_dir(path: &Path) -> Result<(), io::Error> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)
}

fn export_types(output_dir: &Path) -> Result<(), Box<dyn Error>> {
    // HTTP/SSE JSON numbers arrive in JavaScript as `number`; emitting `bigint`
    // would describe a runtime value that JSON.parse never produces.
    let config = Config::new()
        .with_out_dir(output_dir)
        .with_large_int("number");

    macro_rules! export {
        ($($ty:ty),+ $(,)?) => {
            $(<$ty as TS>::export_all(&config)?;)+
        };
    }

    export!(
        CreateSessionRequest,
        CreateSessionResponseDto,
        PromptAttachmentDto,
        PromptRequest,
        ToolApprovalRequest,
        ToolUiRespondRequest,
        ToolUiRespondResponse,
        PromptSubmitResponse,
        CompactSessionRequest,
        CompactSessionResponse,
        CommandInvokeRequest,
        CommandInvokeResponse,
        CommandCompletionRequest,
        CommandCompletionResponse,
        CommandCompletionItemDto,
        SlashCommandListResponseDto,
        KeybindingDto,
        StatusItemDto,
        SlashCommandInfoDto,
        ShadowedSlashCommandDto,
        ForkSessionRequest,
        SessionListItemDto,
        SessionListResponseDto,
        ConversationCursorDto,
        ConversationSnapshotResponseDto,
        ConversationControlStateDto,
        ConversationBlockDto,
        ConversationBlockStatusDto,
        ConversationStreamEnvelopeDto,
        ConversationDeltaDto,
        ConversationErrorEnvelopeDto,
        DeleteProjectResponseDto,
        ConfigViewResponseDto,
        ExtensionStateDto,
        ExtensionSlashCommandDto,
        ExtensionEventDeclDto,
        ExtensionDeclarationDto,
        ToolDefinitionDto,
        ExtensionHttpRouteDto,
        ExtensionDiagnosticsDto,
        ExtensionStageDiagnosticsDto,
        ExtensionListResponseDto,
        ExtensionReloadResponseDto,
        SetExtensionEnabledRequest,
        SetExtensionEnabledResponseDto,
        ProfileDto,
        ProviderCatalogResponseDto,
        ProviderSpecDto,
        ProviderEndpointPresetDto,
        ProviderSpecCapabilitiesDto,
        ApplyProviderPresetRequest,
        ApplyProviderPresetResponseDto,
        RemoveProviderPresetRequest,
        RemoveProviderPresetResponseDto,
        ModelOptionsDto,
        ModelDto,
        UpdateActiveSelectionRequest,
        UpdateActiveSelectionResponseDto,
        ConfigReloadResponseDto,
        CurrentModelResponseDto,
        AvailableModelDto,
        ModelListResponseDto,
        ModelTestResponseDto,
        AgentSessionLinkDto,
        PhaseDto,
        ToolOutputStreamDto,
        ApprovalDecisionDto,
        ProviderWireFormatDto,
        ProviderAuthSchemeDto,
        ThinkingLevelDto,
        AgentSessionStatusDto,
        ExtensionCapabilityDto,
        ToolOriginDto,
        ExecutionModeDto,
    );
    Ok(())
}

fn write_wire_values(output_dir: &Path) -> Result<(), Box<dyn Error>> {
    let mut output = String::from("// This file is generated. Do not edit.\n\n");
    push_wire_values(&mut output, "PHASES", PhaseDto::ALL)?;
    push_wire_values(&mut output, "TOOL_OUTPUT_STREAMS", ToolOutputStreamDto::ALL)?;
    push_wire_values(&mut output, "APPROVAL_DECISIONS", ApprovalDecisionDto::ALL)?;
    push_wire_values(
        &mut output,
        "PROVIDER_WIRE_FORMATS",
        ProviderWireFormatDto::ALL,
    )?;
    push_wire_values(
        &mut output,
        "PROVIDER_AUTH_SCHEMES",
        ProviderAuthSchemeDto::ALL,
    )?;
    push_wire_values(&mut output, "THINKING_LEVELS", ThinkingLevelDto::ALL)?;
    push_wire_values(
        &mut output,
        "AGENT_SESSION_STATUSES",
        AgentSessionStatusDto::ALL,
    )?;
    push_wire_values(
        &mut output,
        "EXTENSION_CAPABILITIES",
        ExtensionCapabilityDto::ALL,
    )?;
    push_wire_values(&mut output, "TOOL_ORIGINS", ToolOriginDto::ALL)?;
    push_wire_values(&mut output, "EXECUTION_MODES", ExecutionModeDto::ALL)?;
    push_wire_values(
        &mut output,
        "BLOCK_STATUSES",
        ConversationBlockStatusDto::ALL,
    )?;
    fs::write(output_dir.join("wire-values.ts"), output)?;
    Ok(())
}

fn push_wire_values<T: Serialize>(
    output: &mut String,
    name: &str,
    values: &[T],
) -> Result<(), Box<dyn Error>> {
    let values = serde_json::to_string(values)?;
    writeln!(output, "export const {name} = {values} as const")?;
    Ok(())
}

fn write_index(output_dir: &Path) -> Result<(), io::Error> {
    let files = directory_files(output_dir)?;
    let mut output = String::from("// This file is generated. Do not edit.\n\n");
    for name in files.keys() {
        if name.contains('/') {
            continue;
        }
        let Some(type_name) = name.strip_suffix(".ts") else {
            continue;
        };
        if type_name != "wire-values" {
            writeln!(output, "export type {{ {type_name} }} from './{type_name}'")
                .map_err(io::Error::other)?;
        }
    }
    output.push_str("export * from './wire-values'\n");
    fs::write(output_dir.join("index.ts"), output)
}

fn normalize_generated_files(output_dir: &Path) -> Result<(), io::Error> {
    for (name, bytes) in directory_files(output_dir)? {
        let source = String::from_utf8(bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("generated TypeScript is not UTF-8: {name}: {error}"),
            )
        })?;
        let mut normalized = source
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        if source.ends_with('\n') {
            normalized.push('\n');
        }
        fs::write(output_dir.join(name), normalized)?;
    }
    Ok(())
}

fn check_generated_files(staging: &Path, committed: &Path) -> Result<(), io::Error> {
    let actual = directory_files(staging)?;
    let expected = directory_files(committed)?;
    if actual == expected {
        return Ok(());
    }

    let names: BTreeSet<_> = actual.keys().chain(expected.keys()).collect();
    let changed = names
        .into_iter()
        .filter(|name| actual.get(*name) != expected.get(*name))
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    Err(io::Error::other(format!(
        "generated TypeScript protocol bindings are stale: {changed}; run `npm run \
         generate:protocol`"
    )))
}

fn replace_generated_files(staging: &Path, committed: &Path) -> Result<(), io::Error> {
    if committed.exists() {
        fs::remove_dir_all(committed)?;
    }
    let parent = committed
        .parent()
        .ok_or_else(|| io::Error::other("generated directory has no parent"))?;
    fs::create_dir_all(parent)?;
    fs::rename(staging, committed)
}

fn directory_files(directory: &Path) -> Result<BTreeMap<String, Vec<u8>>, io::Error> {
    if !directory.is_dir() {
        return Ok(BTreeMap::new());
    }

    let mut files = BTreeMap::new();
    collect_directory_files(directory, directory, &mut files)?;
    Ok(files)
}

fn collect_directory_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), io::Error> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_directory_files(root, &path, files)?;
            continue;
        }
        if !path.is_file() {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("generated file is outside its output directory"))?;
        let name = relative
            .to_str()
            .ok_or_else(|| io::Error::other("generated filename is not UTF-8"))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        files.insert(name, fs::read(path)?);
    }
    Ok(())
}
