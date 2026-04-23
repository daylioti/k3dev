//! Command module
//!
//! This module provides typed command identifiers and command execution utilities.

mod executor;
mod info_exec;
mod types;
mod visibility;

pub use executor::CommandContext;
pub use info_exec::{capture_exec, strip_ansi, trim_output};
pub use types::PaletteCommandId;
pub use visibility::check_visible;
