//! astrcode-support: Host environment utilities.
//!
//! Path resolution, shell detection, and tool result persistence.
//! Things that need the host OS but don't belong in core.

pub mod hostpaths;
pub mod shell;
pub mod tool_results;
