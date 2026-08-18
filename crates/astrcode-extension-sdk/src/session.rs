//! Session authoring helpers.
//!
//! The wire DTOs themselves live in [`crate::wire::session`]; this module only
//! carries the mapping between the bundled authoring selection and the wire
//! contract, so wire types keep exactly one home.

use crate::{extension::SessionToolSelection, wire::session::SessionToolSelectionDto};

/// Maps the bundled authoring selection into the stable extension boundary contract.
pub fn tool_selection_to_dto(selection: SessionToolSelection) -> SessionToolSelectionDto {
    match selection {
        SessionToolSelection::All { except } => SessionToolSelectionDto::All { except },
        SessionToolSelection::Only { names } => SessionToolSelectionDto::Only { names },
    }
}
