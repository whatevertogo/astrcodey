//! Session authoring API and S5R contract re-exports.

pub use crate::{
    extension::SessionToolSelection,
    wire::session::{
        HostCreateSessionOutput, HostCreateSessionRequest, HostRecycleSessionRequest,
        HostRootSubmitTurnRequest, HostSessionEvent, HostSessionEventsPageOutput,
        HostSessionEventsPageRequest, HostSessionReactivateOutput, HostSessionStateOutput,
        HostSessionTargetRequest, HostSubmitTurnOutput, HostSubmitTurnRequest,
        SessionLifecycleStateDto, SessionMessageOriginDto, SessionPhaseDto,
        SessionToolSelectionDto,
    },
};

/// Maps the bundled authoring selection into the stable extension boundary contract.
pub fn tool_selection_to_dto(selection: SessionToolSelection) -> SessionToolSelectionDto {
    match selection {
        SessionToolSelection::All { except } => SessionToolSelectionDto::All { except },
        SessionToolSelection::Only { names } => SessionToolSelectionDto::Only { names },
    }
}
