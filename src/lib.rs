// Some public APIs are kept for the next steps (CLI inbox, screenshots, full
// subprocess cancellation) and are intentionally unused this round.
#![allow(dead_code)]

// Slint generates the `App` component and the structs from ui/app.slint.
slint::include_modules!();

pub mod bilibili;
pub mod cancel;
pub mod cli;
pub mod config;
pub mod document;
pub mod funasr;
pub mod jobs;
pub mod llm;
pub mod paths;
pub mod pipeline;
pub mod process;
pub mod scheduler;
