// This file is generated. Do not edit.

export const PHASES = ["idle","thinking","streaming","calling_tool","compacting","error"] as const
export const TOOL_OUTPUT_STREAMS = ["stdout","stderr"] as const
export const APPROVAL_DECISIONS = ["allow_once","deny_once","allow_always","deny_always"] as const
export const PROVIDER_WIRE_FORMATS = ["openai_chat_completions","openai_responses","anthropic_messages","google_genai"] as const
export const PROVIDER_AUTH_SCHEMES = ["none","bearer","x_api_key","x_goog_api_key"] as const
export const THINKING_LEVELS = ["low","medium","high"] as const
export const AGENT_SESSION_STATUSES = ["running","completed","failed"] as const
export const EXTENSION_CAPABILITIES = ["session_control","session_inspect","public_http","public_http_dispatch","main_model","small_model","session_history","emit_events","consume_events","workspace_read","workspace_write","process_spawn","network_client","provider_request","input_delivery","tool_intercept","turn_continuation_control","live_conversation"] as const
export const TOOL_ORIGINS = ["builtin","bundled","extension","sdk"] as const
export const EXECUTION_MODES = ["sequential","parallel"] as const
export const BLOCK_STATUSES = ["streaming","complete","error"] as const
