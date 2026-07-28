//! Extension hook types: contexts, handlers, results, commands, and registration structs.

mod commands;
mod contexts;
mod handlers;
mod results;
mod types;

// Re-export everything from sub-modules — the public API is unchanged.
pub use commands::*;
pub use contexts::*;
pub use handlers::*;
pub use results::*;
pub use types::*;
