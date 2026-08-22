//! BiMyScribe application modules and Slint-generated view types.

// Slint generates `App` and the view structs from ui/app.slint.
slint::include_modules!();

pub mod bilibili;
pub mod cancel;
pub mod cli;
pub mod config;
pub mod desktop;
pub mod document;
pub mod funasr;
pub mod jobs;
pub mod llm;
pub mod package_check;
pub mod paths;
pub mod pipeline;
pub mod process;
pub mod scheduler;
mod ui_bridge;
