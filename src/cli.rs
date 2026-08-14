//! Non-interactive command-line interface for the production pipeline.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use slint::{ComponentHandle, Weak};
use uuid::Uuid;

use crate::cancel::CancellationToken;
use crate::config::Config;
use crate::jobs::{Job, JobStatus, Queue, RetentionPolicy, Stage, StageState};
use crate::paths::{AppPaths, InstanceLock};
use crate::App;

const AFTER_HELP: &str = "Examples:\n  bimyscribe transcribe 'https://www.bilibili.com/video/BV...'\n  bimyscribe transcribe BV... --output-dir /Volumes/Data/Markdown --json\n  bimyscribe runtime status\n  bimyscribe runtime install --runtime-data-dir /Volumes/Data/BiMyScribe-Runtime\n\nRun `bimyscribe <COMMAND> --help` for command-specific options.\nWith no command, BiMyScribe opens the desktop interface.";

#[derive(Debug, Parser)]
#[command(
    name = "bimyscribe",
    version,
    about = "Turn Bilibili videos into local Markdown transcripts",
    after_help = AFTER_HELP,
    disable_help_subcommand = true
)]
struct Cli {
    /// Isolate all state under a fresh absolute directory and require the bundled Runtime.
    #[arg(long, global = true, value_name = "PATH")]
    release_check_root: Option<PathBuf>,

    /// Per-run hexadecimal token required to reuse one isolated validation root.
    #[arg(
        long,
        global = true,
        requires = "release_check_root",
        value_name = "HEX"
    )]
    release_check_token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Transcribe one Bilibili URL, BV ID, or AV ID and wait for the result.
    #[command(alias = "run")]
    Transcribe(TranscribeArgs),
    /// Inspect or prepare the configured FunASR Runtime.
    Runtime(RuntimeArgs),
    /// Inspect the effective persisted application configuration.
    Config(ConfigArgs),
}

#[derive(Debug, Args)]
struct TranscribeArgs {
    /// Bilibili URL, BV ID, AV ID, or b23.tv short link.
    input: String,

    #[command(flatten)]
    paths: PathOverrides,

    /// Disable optional LLM refinement for this run.
    #[arg(long)]
    no_llm: bool,

    /// Override the configured retention policy for this run.
    #[arg(long, value_enum)]
    retention: Option<RetentionArg>,

    /// Print a machine-readable JSON result to stdout.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RuntimeArgs {
    #[command(subcommand)]
    command: RuntimeCommand,
}

#[derive(Debug, Subcommand)]
enum RuntimeCommand {
    /// Report the effective Runtime project, data directory, and readiness.
    Status(RuntimePathArgs),
    /// Install or revalidate the effective Runtime and its model environment.
    Install(RuntimePathArgs),
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the persisted configuration after platform defaults are applied.
    Show {
        /// Print JSON instead of TOML.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args, Default)]
struct RuntimePathArgs {
    /// Use this Runtime project instead of the saved or bundled Runtime.
    #[arg(long, value_name = "PATH")]
    runtime_project: Option<PathBuf>,

    /// Use this Runtime data directory instead of the saved directory.
    #[arg(long, value_name = "PATH")]
    runtime_data_dir: Option<PathBuf>,
}

#[derive(Debug, Args, Default)]
struct PathOverrides {
    /// Store task intermediates in this directory for this run.
    #[arg(long, value_name = "PATH")]
    work_dir: Option<PathBuf>,

    /// Write the generated document beneath this directory for this run.
    #[arg(long, value_name = "PATH")]
    output_dir: Option<PathBuf>,

    #[command(flatten)]
    runtime: RuntimePathArgs,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RetentionArg {
    Recommended,
    KeepAll,
    DocumentsOnly,
}

impl From<RetentionArg> for RetentionPolicy {
    fn from(value: RetentionArg) -> Self {
        match value {
            RetentionArg::Recommended => Self::Recommended,
            RetentionArg::KeepAll => Self::KeepAll,
            RetentionArg::DocumentsOnly => Self::DocumentsOnly,
        }
    }
}

/// Parse and run a requested CLI command. Returns `None` when the GUI should
/// launch, otherwise the process exit status.
pub fn dispatch_requested() -> anyhow::Result<Option<ExitCode>> {
    if std::env::args_os().len() == 1 {
        return Ok(None);
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    if let Some(root) = cli.release_check_root {
        let token = cli.release_check_token.ok_or_else(|| {
            anyhow::anyhow!("--release-check-root requires --release-check-token")
        })?;
        crate::paths::set_release_check_root(
            require_absolute(&root, "--release-check-root")?,
            &token,
        )?;
    }
    execute(cli.command).map(Some)
}

fn execute(command: Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::Transcribe(args) => transcribe(args),
        Command::Runtime(args) => runtime(args.command),
        Command::Config(args) => config(args.command),
    }
}

struct ProductionContext {
    _lock: InstanceLock,
    config: Config,
}

fn production_context() -> anyhow::Result<ProductionContext> {
    let paths = AppPaths::discover()?;
    let lock = InstanceLock::acquire(&paths).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::anyhow!(
                "BiMyScribe is already running; quit the app or wait for the other CLI command"
            )
        } else {
            error.into()
        }
    })?;
    crate::paths::initialize_or_migrate(&paths)?;
    Ok(ProductionContext {
        _lock: lock,
        config: Config::load()?,
    })
}

fn transcribe(args: TranscribeArgs) -> anyhow::Result<ExitCode> {
    let parsed = crate::bilibili::parse_url(&args.input)?;
    let mut context = production_context()?;
    apply_path_overrides(&mut context.config, &args.paths)?;
    if args.no_llm {
        context.config.llm_enabled = false;
    }

    let runtime_project = require_runtime_project(&context.config)?;
    if !crate::funasr::runtime_is_ready(&runtime_project, &context.config.runtime_data_dir) {
        anyhow::bail!(
            "Runtime is not ready: {}\nRun `bimyscribe runtime install` first.",
            crate::funasr::runtime_status(&runtime_project, &context.config.runtime_data_dir)
        );
    }

    let mut job = Job::new(Uuid::new_v4(), parsed.bvid, parsed.page.unwrap_or(1));
    job.source_url = Some(args.input);
    job.retention = args
        .retention
        .map(Into::into)
        .unwrap_or(context.config.default_retention);
    for stage in crate::jobs::pipeline_stages() {
        job.set_stage_state(stage, StageState::Pending);
    }

    // Slint supplies the weak progress target expected by the production
    // pipeline. Dropping the strong handle guarantees that no window appears.
    let app = App::new()?;
    let weak: Weak<App> = app.as_weak();
    drop(app);

    let mut queue = Queue::load()?;
    queue.jobs.push(job.clone());
    queue.save()?;

    job.status = JobStatus::Running;
    job.started_at = Some(chrono::Utc::now());
    replace_queued_job(&mut queue, &job)?;
    eprintln!("BiMyScribe: transcribing {} page {}", job.bvid, job.page);

    let token = CancellationToken::new();
    let result = crate::pipeline::run_job(&mut job, &context.config, &weak, &token);

    job.finished_at = Some(chrono::Utc::now());
    match result {
        Ok(()) => {
            job.status = JobStatus::Completed;
            job.stage = Stage::Completed;
            job.stage_progress = 100;
            crate::jobs::save_job_state(&job)?;
            replace_queued_job(&mut queue, &job)?;
            let document = final_document(&job)?;
            print_transcription_result(&job, &document, args.json)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            job.status = JobStatus::Failed;
            job.error = Some(error.to_string());
            crate::jobs::save_job_state(&job)?;
            replace_queued_job(&mut queue, &job)?;
            Err(error.into())
        }
    }
}

fn runtime(command: RuntimeCommand) -> anyhow::Result<ExitCode> {
    let mut context = production_context()?;
    let args = match &command {
        RuntimeCommand::Status(args) | RuntimeCommand::Install(args) => args,
    };
    apply_runtime_overrides(&mut context.config, args)?;
    let project = require_runtime_project(&context.config)?;

    match command {
        RuntimeCommand::Status(_) => {
            let manifest = crate::funasr::load_runtime(&project)?;
            println!(
                "source: {}",
                if crate::funasr::runtime_is_bundled(context.config.runtime_project.as_deref()) {
                    "bundled"
                } else {
                    "external"
                }
            );
            println!("backend: {}", manifest.backend.label());
            println!("project: {}", project.display());
            println!("data: {}", context.config.runtime_data_dir.display());
            println!(
                "status: {}",
                crate::funasr::runtime_status(&project, &context.config.runtime_data_dir)
            );
            Ok(
                if crate::funasr::runtime_is_ready(&project, &context.config.runtime_data_dir) {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                },
            )
        }
        RuntimeCommand::Install(_) => {
            eprintln!(
                "BiMyScribe: installing Runtime in {}",
                context.config.runtime_data_dir.display()
            );
            let ready = crate::funasr::install_runtime(&project, &context.config.runtime_data_dir)?;
            println!("Runtime is ready: {}", ready.device);
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn config(command: ConfigCommand) -> anyhow::Result<ExitCode> {
    let context = production_context()?;
    match command {
        ConfigCommand::Show { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&context.config)?);
            } else {
                println!("{}", toml::to_string_pretty(&context.config)?);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn apply_path_overrides(config: &mut Config, overrides: &PathOverrides) -> anyhow::Result<()> {
    if let Some(path) = &overrides.work_dir {
        config.working_dir = require_absolute(path, "--work-dir")?;
    }
    if let Some(path) = &overrides.output_dir {
        config.output_dir = require_absolute(path, "--output-dir")?;
    }
    apply_runtime_overrides(config, &overrides.runtime)
}

fn apply_runtime_overrides(config: &mut Config, overrides: &RuntimePathArgs) -> anyhow::Result<()> {
    if crate::paths::is_release_check() && overrides.runtime_project.is_some() {
        anyhow::bail!(
            "--release-check-root requires the bundled Runtime; --runtime-project is forbidden"
        );
    }
    if let Some(path) = &overrides.runtime_project {
        let path = require_absolute(path, "--runtime-project")?;
        if !path.is_dir() {
            anyhow::bail!("--runtime-project is not a directory: {}", path.display());
        }
        config.runtime_project = Some(path);
    }
    if let Some(path) = &overrides.runtime_data_dir {
        config.runtime_data_dir = require_absolute(path, "--runtime-data-dir")?;
    }
    Ok(())
}

fn require_absolute(path: &Path, option: &str) -> anyhow::Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("{option} requires an absolute path: {}", path.display());
    }
    Ok(path.to_path_buf())
}

fn require_runtime_project(config: &Config) -> anyhow::Result<PathBuf> {
    config.effective_runtime_project().ok_or_else(|| {
        anyhow::anyhow!(
            "no Runtime is configured or bundled; pass --runtime-project or configure it in the app"
        )
    })
}

fn replace_queued_job(queue: &mut Queue, job: &Job) -> anyhow::Result<()> {
    let stored = queue
        .get_mut(job.id)
        .ok_or_else(|| anyhow::anyhow!("CLI job disappeared from the queue"))?;
    *stored = job.clone();
    queue.save()?;
    Ok(())
}

fn final_document(job: &Job) -> anyhow::Result<PathBuf> {
    let directory = job
        .final_output_dir
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("pipeline completed without an output directory"))?;
    let document = directory.join("full.md");
    if !document.is_file() {
        anyhow::bail!(
            "pipeline completed but the final document is missing: {}",
            document.display()
        );
    }
    Ok(document)
}

#[derive(Serialize)]
struct TranscriptionOutput<'a> {
    status: &'static str,
    job_id: Uuid,
    bvid: &'a str,
    page: u32,
    document: &'a Path,
    work_dir: Option<&'a Path>,
}

fn print_transcription_result(job: &Job, document: &Path, json: bool) -> anyhow::Result<()> {
    if json {
        let output = TranscriptionOutput {
            status: "completed",
            job_id: job.id,
            bvid: &job.bvid,
            page: job.page,
            document,
            work_dir: job.work_dir.as_deref(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("{}", document.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transcribe_with_machine_readable_output_and_path_overrides() {
        let cli = Cli::try_parse_from([
            "bimyscribe",
            "transcribe",
            "BV1example",
            "--work-dir",
            "/Volumes/Data/Jobs",
            "--json",
        ])
        .unwrap();

        let Command::Transcribe(args) = cli.command else {
            panic!("expected transcribe command");
        };
        assert_eq!(args.input, "BV1example");
        assert_eq!(
            args.paths.work_dir,
            Some(PathBuf::from("/Volumes/Data/Jobs"))
        );
        assert!(args.json);
    }

    #[test]
    fn transcribe_requires_an_input() {
        let error = Cli::try_parse_from(["bimyscribe", "transcribe"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn path_overrides_require_absolute_paths() {
        let error = require_absolute(Path::new("relative/jobs"), "--work-dir").unwrap_err();
        assert!(error.to_string().contains("requires an absolute path"));
    }
}
