//! Pure session state derived from durable core events.
//!
//! This crate owns read models and reducers. It intentionally contains no
//! storage I/O or session orchestration.

mod model;
mod reducer;

pub use model::*;
pub use reducer::{
    ProjectionError, SessionReadModelProjection, reduce, replay, validate_next_event,
};
