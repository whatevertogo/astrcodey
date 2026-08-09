//! S5R 3.0 contract re-exports and extension hook name mappings.

pub use astrcode_extension_contract::protocol::{
    CAP_HANDLER_INVOKE, CAP_RUNTIME_PING, CancelMsg, CapabilityDescriptor, ErrorPayload,
    HandlerDescriptor, HandlerId, HandlerInvokeRequest, HandlerKind, InitializeMsg,
    InitializeOutput, InvokeMsg, PeerInfo, ResultKind, ResultMsg, S5R_STACK, S5R_VERSION,
    StreamMsg, WIRE_CODEC_JSON, WireMessage, encode_wire_message, parse_wire_message,
};

use crate::extension::{CompactEvent, HookMode, LifecycleEvent};

pub fn event_from_name(name: &str) -> Option<LifecycleEvent> {
    match name {
        "session_start" => Some(LifecycleEvent::SessionStart),
        "session_resume" => Some(LifecycleEvent::SessionResume),
        "session_shutdown" => Some(LifecycleEvent::SessionShutdown),
        "turn_start" => Some(LifecycleEvent::TurnStart),
        "turn_end" => Some(LifecycleEvent::TurnEnd),
        "turn_aborted" => Some(LifecycleEvent::TurnAborted),
        "step_start" => Some(LifecycleEvent::StepStart),
        "step_end" => Some(LifecycleEvent::StepEnd),
        "pre_tool_use" => Some(LifecycleEvent::PreToolUse),
        "post_tool_use" => Some(LifecycleEvent::PostToolUse),
        "before_provider_request" => Some(LifecycleEvent::BeforeProviderRequest),
        "after_provider_response" => Some(LifecycleEvent::AfterProviderResponse),
        "continue_after_stop" => Some(LifecycleEvent::ContinueAfterStop),
        "user_prompt_submit" => Some(LifecycleEvent::UserPromptSubmit),
        "user_message_envelope" => Some(LifecycleEvent::UserMessageEnvelope),
        "prompt_build" => Some(LifecycleEvent::PromptBuild),
        "post_recap" => Some(LifecycleEvent::PostRecap),
        _ => None,
    }
}

pub fn compact_event_from_name(name: &str) -> Option<CompactEvent> {
    match name {
        "pre_compact" => Some(CompactEvent::PreCompact),
        "post_compact" => Some(CompactEvent::PostCompact),
        _ => None,
    }
}

pub fn mode_from_name(name: &str) -> Option<HookMode> {
    match name {
        "blocking" => Some(HookMode::Blocking),
        "non_blocking" => Some(HookMode::NonBlocking),
        "advisory" => Some(HookMode::Advisory),
        _ => None,
    }
}

pub const fn mode_to_name(mode: HookMode) -> &'static str {
    match mode {
        HookMode::Blocking => "blocking",
        HookMode::NonBlocking => "non_blocking",
        HookMode::Advisory => "advisory",
    }
}

pub const fn event_to_name(event: &LifecycleEvent) -> &'static str {
    match event {
        LifecycleEvent::SessionStart => "session_start",
        LifecycleEvent::SessionResume => "session_resume",
        LifecycleEvent::SessionShutdown => "session_shutdown",
        LifecycleEvent::TurnStart => "turn_start",
        LifecycleEvent::TurnEnd => "turn_end",
        LifecycleEvent::TurnAborted => "turn_aborted",
        LifecycleEvent::StepStart => "step_start",
        LifecycleEvent::StepEnd => "step_end",
        LifecycleEvent::PreToolUse => "pre_tool_use",
        LifecycleEvent::PostToolUse => "post_tool_use",
        LifecycleEvent::BeforeProviderRequest => "before_provider_request",
        LifecycleEvent::AfterProviderResponse => "after_provider_response",
        LifecycleEvent::ContinueAfterStop => "continue_after_stop",
        LifecycleEvent::UserPromptSubmit => "user_prompt_submit",
        LifecycleEvent::UserMessageEnvelope => "user_message_envelope",
        LifecycleEvent::PromptBuild => "prompt_build",
        LifecycleEvent::PostRecap => "post_recap",
    }
}

pub const fn compact_event_to_name(event: CompactEvent) -> &'static str {
    match event {
        CompactEvent::PreCompact => "pre_compact",
        CompactEvent::PostCompact => "post_compact",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_ids_and_hook_names_have_stable_round_trips() {
        let handler = HandlerId::new("example", HandlerKind::Tool, "lookup");
        assert_eq!(
            handler.parts(),
            Some(("example", HandlerKind::Tool, "lookup"))
        );
        assert_eq!(
            event_from_name(event_to_name(&LifecycleEvent::ContinueAfterStop)),
            Some(LifecycleEvent::ContinueAfterStop)
        );
        assert_eq!(
            mode_from_name(mode_to_name(HookMode::Advisory)),
            Some(HookMode::Advisory)
        );
        assert_eq!(
            compact_event_from_name(compact_event_to_name(CompactEvent::PostCompact)),
            Some(CompactEvent::PostCompact)
        );
    }
}
