//! astrcode-extension-agent-tools — Subagent delegation + task management.
//!
//! Loaded via libloading. Entry point: `extension_factory(&api)`.

mod agent;
mod ffi_types;
mod task;

use std::sync::{LazyLock, Mutex};

use ffi_types::*;
use task::TaskStore;

static AGENTS: Mutex<Vec<agent::AgentConfig>> = Mutex::new(Vec::new());
static WORKING_DIR: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::from(".")));

// ─── Entry point ─────────────────────────────────────────────────────────

#[no_mangle]
/// # Safety
///
/// `api` must be a valid pointer to an `ExtensionApi` for the duration of this
/// call. The host owns the vtable and all pointers passed through it.
pub unsafe extern "C" fn extension_factory(api: *const ExtensionApi) {
    let Some(api) = api.as_ref() else {
        return;
    };

    unsafe {
        (api.on)(api, EVENT_SESSION_START, MODE_BLOCKING, on_session_start);
    }

    register_tool(
        api,
        "agent",
        "Spawn a subagent to handle a delegated task. Call agentList first when choosing \
         subagent_type. mode=single for one task, mode=chain for sequential steps with {previous} \
         placeholder.",
        r#"{"type":"object","properties":{"description":{"type":"string","description":"Short 3-5 word description"},"prompt":{"type":"string","description":"Task for the subagent"},"subagent_type":{"type":"string","description":"Agent name from agents/ directory"},"mode":{"type":"string","enum":["single","chain"],"default":"single"},"chain":{"type":"array","items":{"type":"object","properties":{"agent":{"type":"string"},"task":{"type":"string"}}}}},"required":["prompt","description"]}"#,
        execute_agent_tool,
    );
    register_tool(
        api,
        "agentList",
        "List available subagents with descriptions, declared tools, and model preferences.",
        r#"{"type":"object","properties":{}}"#,
        execute_agent_list_tool,
    );
    register_tool(
        api,
        "taskCreate",
        "Create a tracked task",
        r#"{"type":"object","properties":{"subject":{"type":"string"},"description":{"type":"string"},"blocks":{"type":"array","items":{"type":"string"}}},"required":["subject","description"]}"#,
        execute_task_create_tool,
    );
    register_tool(
        api,
        "taskList",
        "List all tracked tasks",
        r#"{"type":"object","properties":{}}"#,
        execute_task_list_tool,
    );
    register_tool(
        api,
        "taskUpdate",
        "Update task status",
        r#"{"type":"object","properties":{"id":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]},"subject":{"type":"string"},"description":{"type":"string"}},"required":["id"]}"#,
        execute_task_update_tool,
    );
}

fn register_tool(api: &ExtensionApi, name: &str, desc: &str, params: &str, callback: ToolCallback) {
    unsafe {
        (api.register_tool)(
            api,
            name.as_ptr(),
            name.len() as u32,
            desc.as_ptr(),
            desc.len() as u32,
            params.as_ptr(),
            params.len() as u32,
        );
        (api.register_tool_handler)(api, name.as_ptr(), name.len() as u32, callback);
    }
}

// ─── Event handlers ──────────────────────────────────────────────────────

unsafe extern "C" fn on_session_start(
    _event: u8,
    ctx: *const std::ffi::c_void,
    effect_out: *mut u8,
    _out_ptr: *mut *const u8,
    _out_len: *mut u32,
) {
    if !effect_out.is_null() {
        *effect_out = EFFECT_ALLOW;
    }
    if ctx.is_null() {
        return;
    }

    let ffi_ctx = read_ffi_ctx(ctx);
    let wd = read_ffi_str(ffi_ctx.working_dir_ptr, ffi_ctx.working_dir_len).to_string();
    let working_dir = if wd.is_empty() { String::from(".") } else { wd };

    if let Ok(mut current_dir) = WORKING_DIR.lock() {
        *current_dir = working_dir.clone();
    }
    if let Ok(mut agents) = AGENTS.lock() {
        *agents = agent::discover_agents(Some(&working_dir));
    }
}

// ─── Tool callbacks ──────────────────────────────────────────────────────

unsafe extern "C" fn execute_agent_tool(
    ctx: *const std::ffi::c_void,
    output_ptr: *mut *const u8,
    output_len: *mut u32,
    error_ptr: *mut *const u8,
    error_len: *mut u32,
) -> u8 {
    // Agent tool returns a declarative outcome JSON — use code 2.
    complete_tool_json_or_error(
        handle_agent(tool_input_json(ctx)),
        output_ptr,
        output_len,
        error_ptr,
        error_len,
    )
}

unsafe extern "C" fn execute_task_create_tool(
    ctx: *const std::ffi::c_void,
    output_ptr: *mut *const u8,
    output_len: *mut u32,
    error_ptr: *mut *const u8,
    error_len: *mut u32,
) -> u8 {
    complete_tool(
        handle_task_create(tool_input_json(ctx)),
        output_ptr,
        output_len,
        error_ptr,
        error_len,
    )
}

unsafe extern "C" fn execute_agent_list_tool(
    _ctx: *const std::ffi::c_void,
    output_ptr: *mut *const u8,
    output_len: *mut u32,
    error_ptr: *mut *const u8,
    error_len: *mut u32,
) -> u8 {
    complete_tool(
        handle_agent_list(),
        output_ptr,
        output_len,
        error_ptr,
        error_len,
    )
}

unsafe extern "C" fn execute_task_list_tool(
    _ctx: *const std::ffi::c_void,
    output_ptr: *mut *const u8,
    output_len: *mut u32,
    error_ptr: *mut *const u8,
    error_len: *mut u32,
) -> u8 {
    complete_tool(
        handle_task_list(),
        output_ptr,
        output_len,
        error_ptr,
        error_len,
    )
}

unsafe extern "C" fn execute_task_update_tool(
    ctx: *const std::ffi::c_void,
    output_ptr: *mut *const u8,
    output_len: *mut u32,
    error_ptr: *mut *const u8,
    error_len: *mut u32,
) -> u8 {
    complete_tool(
        handle_task_update(tool_input_json(ctx)),
        output_ptr,
        output_len,
        error_ptr,
        error_len,
    )
}

fn complete_tool_json_or_error(
    result: Result<String, String>,
    output_ptr: *mut *const u8,
    output_len: *mut u32,
    error_ptr: *mut *const u8,
    error_len: *mut u32,
) -> u8 {
    match result {
        Ok(json) => {
            write_ffi_string(output_ptr, output_len, json);
            TOOL_STATUS_OUTCOME_JSON
        },
        Err(error) => {
            write_ffi_string(error_ptr, error_len, error);
            TOOL_STATUS_ERROR
        },
    }
}

fn complete_tool(
    result: Result<String, String>,
    output_ptr: *mut *const u8,
    output_len: *mut u32,
    error_ptr: *mut *const u8,
    error_len: *mut u32,
) -> u8 {
    match result {
        Ok(output) => {
            write_ffi_string(output_ptr, output_len, output);
            TOOL_STATUS_OK
        },
        Err(error) => {
            write_ffi_string(error_ptr, error_len, error);
            TOOL_STATUS_ERROR
        },
    }
}

unsafe fn tool_input_json(ctx: *const std::ffi::c_void) -> &'static str {
    if ctx.is_null() {
        return "{}";
    }
    let ffi_ctx = read_ffi_ctx(ctx);
    let input = read_ffi_str(ffi_ctx.tool_input_ptr, ffi_ctx.tool_input_len);
    if input.is_empty() { "{}" } else { input }
}

fn write_ffi_string(ptr_out: *mut *const u8, len_out: *mut u32, text: String) {
    let boxed = text.into_boxed_str();
    let ptr = boxed.as_ptr();
    let len = boxed.len() as u32;
    let _leaked = Box::leak(boxed);
    unsafe {
        if !ptr_out.is_null() {
            *ptr_out = ptr;
        }
        if !len_out.is_null() {
            *len_out = len;
        }
    }
}

// ─── Tool implementations ────────────────────────────────────────────────

fn handle_agent(input_json: &str) -> Result<String, String> {
    let input: serde_json::Value =
        serde_json::from_str(input_json).map_err(|e| format!("parse: {e}"))?;
    let prompt = input["prompt"].as_str().ok_or("prompt required")?;
    let agent_name = input["subagent_type"].as_str().unwrap_or("");
    let mode = input["mode"].as_str().unwrap_or("single");
    let agents = AGENTS.lock().map_err(|e| e.to_string())?;

    match mode {
        "chain" => {
            // v1: chain mode not yet supported via outcome pattern.
            // The host would need sequential outcome support.
            Err(
                "chain mode is not yet supported — use single mode or list each agent step \
                 manually"
                    .into(),
            )
        },
        _ => {
            let agent = if agent_name.is_empty() {
                agents.first().ok_or("no agents configured")?
            } else {
                agents
                    .iter()
                    .find(|a| a.name == agent_name)
                    .ok_or_else(|| {
                        format!(
                            "agent '{agent_name}' not found.\n\n{}",
                            format_agents_for_model(&agents)
                        )
                    })?
            };

            // Return declarative RunSession outcome as JSON.
            // The host runner will interpret and spawn the child session.
            let outcome = serde_json::json!({
                "kind": "run_session",
                "name": agent.name,
                "system_prompt": agent.body,
                "user_prompt": prompt,
                "allowed_tools": agent.tools,
                "model_preference": agent.model,
            });
            Ok(outcome.to_string())
        },
    }
}

fn handle_agent_list() -> Result<String, String> {
    let agents = AGENTS.lock().map_err(|e| e.to_string())?;
    Ok(format_agents_for_model(&agents))
}

fn format_agents_for_model(agents: &[agent::AgentConfig]) -> String {
    if agents.is_empty() {
        return String::from("No agents configured.");
    }

    let mut lines = Vec::with_capacity(agents.len() + 1);
    lines.push(String::from("Available agents:"));
    for agent in agents {
        let tools = if agent.tools.is_empty() {
            String::from("inherit/default")
        } else {
            agent.tools.join(", ")
        };
        let model = agent.model.as_deref().unwrap_or("inherit/default");
        lines.push(format!(
            "- {}: {} (tools: {}; model: {})",
            agent.name, agent.description, tools, model
        ));
    }
    lines.join("\n")
}

fn handle_task_create(input_json: &str) -> Result<String, String> {
    let input: serde_json::Value =
        serde_json::from_str(input_json).map_err(|e| format!("parse: {e}"))?;
    let subject = input["subject"].as_str().ok_or("subject required")?;
    let desc = input["description"].as_str().unwrap_or("");
    let blocks: Vec<String> = input["blocks"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let store = TaskStore::new();
    let task = store.create(subject, desc, &blocks);
    Ok(format!("Created task {}: {}", task.id, task.subject))
}

fn handle_task_list() -> Result<String, String> {
    let store = TaskStore::new();
    let tasks = store.list();
    if tasks.is_empty() {
        return Ok("No tasks.".into());
    }
    let lines: Vec<String> = tasks
        .iter()
        .map(|task| {
            format!(
                "[{}] {} - {}",
                task.id,
                status_icon(&task.status),
                task.subject
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

fn handle_task_update(input_json: &str) -> Result<String, String> {
    let input: serde_json::Value =
        serde_json::from_str(input_json).map_err(|e| format!("parse: {e}"))?;
    let id = input["id"].as_str().ok_or("id required")?;
    let status = match input["status"].as_str() {
        Some("in_progress") => Some(task::TaskStatus::InProgress),
        Some("completed") => Some(task::TaskStatus::Completed),
        Some("pending") => Some(task::TaskStatus::Pending),
        _ => None,
    };
    let store = TaskStore::new();
    let task = store
        .update(
            id,
            status,
            input["subject"].as_str(),
            input["description"].as_str(),
        )
        .ok_or_else(|| format!("task '{id}' not found"))?;
    Ok(format!(
        "Updated task {}: {} - {}",
        task.id,
        task.subject,
        status_icon(&task.status)
    ))
}

fn status_icon(status: &task::TaskStatus) -> &str {
    match status {
        task::TaskStatus::Pending => "pending",
        task::TaskStatus::InProgress => "in_progress",
        task::TaskStatus::Completed => "completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_agent_metadata_for_model_selection() {
        let agents = vec![agent::AgentConfig {
            id: String::from("code-reviewer"),
            name: String::from("code-reviewer"),
            description: String::from("Use for behavior-focused code review"),
            tools: vec![String::from("Read"), String::from("Grep")],
            model: Some(String::from("opus")),
            body: String::from("Review carefully."),
        }];

        let output = format_agents_for_model(&agents);

        assert!(output.contains("code-reviewer"));
        assert!(output.contains("Use for behavior-focused code review"));
        assert!(output.contains("tools: Read, Grep"));
        assert!(output.contains("model: opus"));
    }

    #[test]
    fn formats_empty_agent_list() {
        assert_eq!(format_agents_for_model(&[]), "No agents configured.");
    }
}
