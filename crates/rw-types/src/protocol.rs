//! Authenticated client commands, event lifetimes, and shared wire values.

mod actions;
mod commands;
mod provider_setup;
pub use provider_setup::*;
mod events;
mod shared;
pub use actions::*;
pub use commands::*;
pub use events::*;
pub(crate) use shared::decimal_u64;
pub use shared::*;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod strict_tests;

#[cfg(test)]
mod nullable_tests;
