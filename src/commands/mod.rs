//! Command module
//!
//! This module provides typed command identifiers and command execution utilities.

mod executor;
mod types;

pub use executor::CommandContext;
pub use types::PaletteCommandId;
