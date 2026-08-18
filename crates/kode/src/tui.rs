mod markdown;
mod theme;

mod commands;
mod draw;
mod events;
mod run;
mod state;

#[cfg(test)]
mod tests;

pub use run::run;

// These glob re-exports exist solely so `tests.rs`'s `use super::*;` can see
// every internal type/fn; nothing outside `tui` uses them (main.rs only
// calls `tui::run`), so they're gated to test builds to avoid unused-import
// warnings in the normal (non-test) compilation.
#[cfg(test)]
pub(crate) use commands::*;
#[cfg(test)]
pub(crate) use draw::*;
#[cfg(test)]
pub(crate) use events::*;
#[cfg(test)]
pub(crate) use run::*;
#[cfg(test)]
pub(crate) use state::*;
