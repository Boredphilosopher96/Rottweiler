//! Authenticated client commands, event lifetimes, and shared wire values.

mod commands;
mod events;
mod shared;
pub use commands::*;
pub use events::*;
pub(crate) use shared::decimal_u64;
pub use shared::*;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod strict_tests;
