//! CLI entry point.
//!
//! First version only adds tasks to the App's inbox. Real implementation will
//! write a request file to `inbox/` with a unique `request_id` and launch/activate
//! the running App (single-instance). This stub just parses args and prints.

use std::env;
use thiserror::Error;

#[derive(Debug)]
pub enum Command {
    Add {
        url: String,
    },
    /// Default: launch GUI (no args, or unrecognized -> GUI).
    Gui,
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("no command given")]
    NoCommand,
    #[error("missing URL argument for `add`")]
    MissingUrl,
}

pub fn parse() -> Result<Command, CliError> {
    let mut args = env::args().skip(1);
    let Some(cmd) = args.next() else {
        return Ok(Command::Gui);
    };
    match cmd.as_str() {
        "add" => {
            let url = args.next().ok_or(CliError::MissingUrl)?;
            Ok(Command::Add { url })
        }
        _ => Ok(Command::Gui),
    }
}

/// Entry point when invoked as a CLI subcommand. STUB: real implementation
/// writes `{request_id}.json` into the App inbox dir and launches the App.
pub fn run(cmd: Command) -> Result<(), CliError> {
    match cmd {
        Command::Add { url } => {
            // TODO(step 2): write to inbox/ + launch/activate App.
            println!(
                "add: {} (inbox enqueue is a stub; start the App GUI to process)",
                url
            );
            Ok(())
        }
        Command::Gui => {
            // Handled by main.rs running the Slint UI.
            Ok(())
        }
    }
}
