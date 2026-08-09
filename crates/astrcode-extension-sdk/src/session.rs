//! Session authoring API and S5R contract re-exports.

pub use astrcode_extension_contract::session::*;

pub use crate::{
    extension::SessionToolSelection,
    tool::{
        CreateSessionRequest, SessionAccess, SessionAccessPair, SessionApiError, SessionHandle,
        SessionLifecycleState, SessionOperations, SessionReactivation, SessionState, SessionStatus,
        SubmitTurnRequest, SubmitTurnResult,
    },
};

/// Maps the bundled authoring selection into the stable extension boundary contract.
pub fn tool_selection_to_dto(selection: SessionToolSelection) -> SessionToolSelectionDto {
    match selection {
        SessionToolSelection::All { except } => SessionToolSelectionDto::All { except },
        SessionToolSelection::Only { names } => SessionToolSelectionDto::Only { names },
    }
}
