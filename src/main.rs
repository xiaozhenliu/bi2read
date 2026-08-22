//! Process entry point.
//!
//! Argument routing stays here; each execution mode hides its lifecycle behind
//! one module interface.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--package-self-check")) {
        return bimyscribe::package_check::run();
    }

    env_logger::init();
    if let Some(status) = bimyscribe::cli::dispatch_requested()? {
        if status == std::process::ExitCode::SUCCESS {
            return Ok(());
        }
        std::process::exit(bimyscribe::cli::exit_code_value(status));
    }

    bimyscribe::desktop::run()
}
