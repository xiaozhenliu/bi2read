//! Non-interactive command-line interface for the production pipeline.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Weak};
use uuid::Uuid;

use crate::cancel::CancellationToken;
use crate::config::Config;
use crate::jobs::{
    CreatedFrom, Job, JobCreationInput, JobStatus, Queue, RetentionPolicy, SourceLanguage, Stage,
};
use crate::paths::{AppPaths, InstanceLock};
use crate::App;

const AFTER_HELP: &str = "Examples:\n  bimyscribe transcribe 'https://www.bilibili.com/video/BV...' --language zh\n  bimyscribe transcribe BV... --language en --output-dir /Volumes/Data/Markdown --json\n  bimyscribe runtime status --json\n  bimyscribe runtime install --runtime-data-dir /Volumes/Data/BiMyScribe-Runtime\n\nRun bimyscribe <COMMAND> --help for command-specific options.\nWith no command, BiMyScribe opens the desktop interface.";
const CLI_SCHEMA_VERSION: u32 = 1;
const EXIT_INVALID_ARGUMENTS: u8 = 2;
const EXIT_RUNTIME: u8 = 3;
const EXIT_PIPELINE: u8 = 4;
const EXIT_ARTIFACT: u8 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliError {
    pub code: String,
    pub message: String,
    pub action: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliEnvelope<T> {
    pub schema_version: u32,
    pub ok: bool,
    pub data: Option<T>,
    pub error: Option<CliError>,
}

fn success_envelope<T>(data: T) -> CliEnvelope<T> {
    CliEnvelope {
        schema_version: CLI_SCHEMA_VERSION,
        ok: true,
        data: Some(data),
        error: None,
    }
}

fn error_envelope(error: CliError) -> CliEnvelope<()> {
    CliEnvelope {
        schema_version: CLI_SCHEMA_VERSION,
        ok: false,
        data: None,
        error: Some(error),
    }
}

fn print_json<T: Serialize>(value: &CliEnvelope<T>) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn cli_error(code: &str, message: impl Into<String>, action: Option<&str>) -> CliError {
    CliError {
        code: code.into(),
        message: message.into(),
        action: action.map(str::to_owned),
    }
}

fn cli_error_from_anyhow(error: &anyhow::Error) -> CliError {
    if let Some(error) = error.downcast_ref::<crate::bilibili::ParseError>() {
        return cli_error(
            "invalid-input",
            error.to_string(),
            Some("提供 Bilibili URL、BV 或 AV 标识"),
        );
    }
    if let Some(error) = error.downcast_ref::<crate::funasr::FunasrError>() {
        return cli_error_from_funasr(error);
    }
    if let Some(error) = error.downcast_ref::<crate::pipeline::PipelineError>() {
        return match error {
            crate::pipeline::PipelineError::DockerNotRunning => cli_error(
                "runtime-not-ready",
                error.to_string(),
                Some("启动 Docker Desktop 后重试"),
            ),
            crate::pipeline::PipelineError::DriveNotMounted => cli_error(
                "external-drive-not-mounted",
                error.to_string(),
                Some("连接保存任务的外置盘后重试"),
            ),
            crate::pipeline::PipelineError::TranscriptionSelectionRequired(_) => cli_error(
                "legacy-job-rebuild-required",
                error.to_string(),
                Some("使用新 Runtime 与语言设置重建任务"),
            ),
            crate::pipeline::PipelineError::Funasr(message) => cli_error_from_message(message),
            crate::pipeline::PipelineError::Document(message) => cli_error(
                "artifact-failed",
                message.clone(),
                Some("检查输出目录后重试"),
            ),
            crate::pipeline::PipelineError::Cancelled => {
                cli_error("cancelled", error.to_string(), None)
            }
            _ => cli_error(
                "pipeline-failed",
                error.to_string(),
                Some("检查 error.message 后重试"),
            ),
        };
    }

    cli_error_from_message(&error.to_string())
}

fn cli_error_from_funasr(error: &crate::funasr::FunasrError) -> CliError {
    match error {
        crate::funasr::FunasrError::ContractUpgradeRequired(_) => cli_error(
            "runtime-contract-upgrade-required",
            error.to_string(),
            Some("升级 Runtime 到 contract v2"),
        ),
        crate::funasr::FunasrError::NotReady => cli_error(
            "runtime-not-ready",
            error.to_string(),
            Some("运行 runtime install 或检查 Runtime 数据目录"),
        ),
        crate::funasr::FunasrError::IdentityChanged => cli_error(
            "runtime-identity-changed",
            error.to_string(),
            Some("重新查看 Runtime 状态并重建任务"),
        ),
        crate::funasr::FunasrError::DockerUnavailable => cli_error(
            "runtime-not-ready",
            error.to_string(),
            Some("启动 Docker Desktop 后重试"),
        ),
        crate::funasr::FunasrError::Validation(message) => cli_error_from_message(message),
        crate::funasr::FunasrError::Parse(_) => cli_error(
            "invalid-evidence",
            error.to_string(),
            Some("检查 Runtime normalized 输出"),
        ),
        _ => cli_error(
            "runtime-failed",
            error.to_string(),
            Some("检查 Runtime 日志后重试"),
        ),
    }
}

fn cli_error_from_message(message: &str) -> CliError {
    if message.contains("runtime-contract-upgrade-required") {
        return cli_error(
            "runtime-contract-upgrade-required",
            message,
            Some("升级 Runtime 到 contract v2"),
        );
    }
    if message.contains("runtime-identity-changed") {
        return cli_error(
            "runtime-identity-changed",
            message,
            Some("重新查看 Runtime 状态并重建任务"),
        );
    }
    if message.contains("runtime-not-ready")
        || message.contains("Runtime 尚未就绪")
        || message.contains("Runtime is not ready")
    {
        return cli_error(
            "runtime-not-ready",
            message,
            Some("运行 runtime install 或检查 Runtime 数据目录"),
        );
    }
    if message.contains("no Runtime is configured")
        || message.contains("未配置 FunASR Runtime")
        || message.contains("no Runtime")
    {
        return cli_error(
            "runtime-not-configured",
            message,
            Some("传入 --runtime-project 或先在设置中配置 Runtime"),
        );
    }
    if message.contains("requires an absolute path")
        || message.contains("不是有效目录")
        || message.contains("路径")
    {
        return cli_error("invalid-path", message, Some("使用存在且为绝对路径的目录"));
    }
    if message.contains("final document is missing")
        || message.contains("without an output directory")
    {
        return cli_error("artifact-failed", message, Some("检查输出目录后重试"));
    }
    cli_error("internal-error", message, None)
}

fn exit_code_for(error: &CliError) -> ExitCode {
    let value = match error.code.as_str() {
        "invalid-arguments" | "invalid-input" | "invalid-path" => EXIT_INVALID_ARGUMENTS,
        "runtime-not-configured"
        | "runtime-not-ready"
        | "runtime-contract-upgrade-required"
        | "runtime-identity-changed"
        | "runtime-failed"
        | "external-drive-not-mounted"
        | "legacy-job-rebuild-required" => EXIT_RUNTIME,
        "artifact-failed" | "invalid-evidence" => EXIT_ARTIFACT,
        "pipeline-failed" | "cancelled" => EXIT_PIPELINE,
        _ => 1,
    };
    ExitCode::from(value)
}

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

    /// Source language requested from the Runtime. Omitted means auto.
    #[arg(long, value_name = "LANG", default_value = "auto")]
    language: SourceLanguage,

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
    Status(RuntimeStatusArgs),
    /// Install or revalidate the effective Runtime and its model environment.
    Install(RuntimePathArgs),
}

#[derive(Debug, Args)]
struct RuntimeStatusArgs {
    #[command(flatten)]
    paths: RuntimePathArgs,

    /// Print one versioned JSON envelope.
    #[arg(long)]
    json: bool,
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
    let json_mode = args_request_json();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if json_mode => {
            print_json(&error_envelope(CliError {
                code: "invalid-arguments".into(),
                message: error.to_string(),
                action: Some("检查 --help 中的参数格式".into()),
            }))?;
            return Ok(Some(ExitCode::from(EXIT_INVALID_ARGUMENTS)));
        }
        Err(error) => error.exit(),
    };
    let result = (|| {
        if let Some(root) = cli.release_check_root {
            let token = cli.release_check_token.ok_or_else(|| {
                anyhow::anyhow!("--release-check-root requires --release-check-token")
            })?;
            crate::paths::set_release_check_root(
                require_absolute(&root, "--release-check-root")?,
                &token,
            )?;
        }
        execute(cli.command)
    })();
    match result {
        Ok(status) => Ok(Some(status)),
        Err(error) if json_mode => {
            let cli_error = cli_error_from_anyhow(&error);
            let status = exit_code_for(&cli_error);
            print_json(&error_envelope(cli_error))?;
            Ok(Some(status))
        }
        Err(error) => Err(error),
    }
}

fn args_request_json() -> bool {
    std::env::args_os().any(|argument| argument.as_os_str() == OsStr::new("--json"))
}

fn execute(command: Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::Transcribe(args) => transcribe(args),
        Command::Runtime(args) => runtime(args.command),
        Command::Config(args) => config(args.command),
    }
}

/// Convert the opaque ExitCode returned by the CLI boundary into the process
/// code used by the process entry point. Keeping this mapping here makes all
/// JSON and human paths share the same stable values.
pub fn exit_code_value(status: ExitCode) -> i32 {
    for value in 0..=u8::MAX {
        if status == ExitCode::from(value) {
            return i32::from(value);
        }
    }
    1
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

    let selection = context
        .config
        .job_transcription_selection(CreatedFrom::Cli, args.language)
        .map_err(anyhow::Error::msg)?;
    let mut job = Job::from_creation(JobCreationInput::new(
        Uuid::new_v4(),
        parsed.bvid,
        parsed.page.unwrap_or(1),
        selection,
        args.retention
            .map(Into::into)
            .unwrap_or(context.config.default_retention),
        context.config.content_setup(),
    ));
    job.source_url = Some(args.input);

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

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeStatusData {
    runtime: crate::funasr::RuntimeDescription,
}

fn runtime(command: RuntimeCommand) -> anyhow::Result<ExitCode> {
    match command {
        RuntimeCommand::Status(args) => runtime_status(args),
        RuntimeCommand::Install(args) => runtime_install(args),
    }
}

fn runtime_status(args: RuntimeStatusArgs) -> anyhow::Result<ExitCode> {
    let mut context = production_context()?;
    apply_runtime_overrides(&mut context.config, &args.paths)?;
    let project = require_runtime_project(&context.config)?;
    let source = if crate::funasr::runtime_is_bundled(context.config.runtime_project.as_deref()) {
        crate::jobs::RuntimeSource::Bundled
    } else {
        crate::jobs::RuntimeSource::External
    };
    let description =
        crate::funasr::describe_runtime(&project, &context.config.runtime_data_dir, source)?;
    let ready = description.ready;
    if args.json {
        print_json(&success_envelope(RuntimeStatusData {
            runtime: description,
        }))?;
    } else {
        println!(
            "source: {}",
            if source == crate::jobs::RuntimeSource::Bundled {
                "bundled"
            } else {
                "external"
            }
        );
        println!("backend: {}", description.backend);
        println!("contract: v{}", description.contract_version);
        println!("project: {}", description.project.display());
        println!("data: {}", description.data_dir.display());
        println!("identity: {}", description.identity);
        println!("status: {}", if ready { "ready" } else { "not-ready" });
    }
    Ok(if ready {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_RUNTIME)
    })
}

fn runtime_install(args: RuntimePathArgs) -> anyhow::Result<ExitCode> {
    let mut context = production_context()?;
    apply_runtime_overrides(&mut context.config, &args)?;
    let project = require_runtime_project(&context.config)?;
    eprintln!(
        "BiMyScribe: installing Runtime in {}",
        context.config.runtime_data_dir.display()
    );
    let ready = crate::funasr::install_runtime(&project, &context.config.runtime_data_dir)?;
    println!("Runtime is ready: {}", ready.device);
    Ok(ExitCode::SUCCESS)
}

fn config(command: ConfigCommand) -> anyhow::Result<ExitCode> {
    let context = production_context()?;
    match command {
        ConfigCommand::Show { json } => {
            if json {
                print_json(&success_envelope(&context.config))?;
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
    runtime_identity: &'a str,
    runtime_source: crate::jobs::RuntimeSource,
    runtime_backend: crate::jobs::RuntimeBackend,
    model_description: Option<&'a str>,
    requested_language: SourceLanguage,
    reported_language: Option<SourceLanguage>,
    reported_model: Option<&'a str>,
    reported_runtime_identity: Option<&'a str>,
}

fn print_transcription_result(job: &Job, document: &Path, json: bool) -> anyhow::Result<()> {
    if json {
        let selection = job
            .transcription_selection
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("completed CLI Job has no transcription selection"))?;
        let result = job.transcription_result.as_ref();
        let output = TranscriptionOutput {
            status: "completed",
            job_id: job.id,
            bvid: &job.bvid,
            page: job.page,
            document,
            work_dir: job.work_dir.as_deref(),
            runtime_identity: &selection.runtime_identity,
            runtime_source: selection.runtime_source,
            runtime_backend: selection.runtime_backend,
            model_description: selection.model_description.as_deref(),
            requested_language: selection.requested_language,
            reported_language: result.and_then(|result| result.reported_language),
            reported_model: result.and_then(|result| result.reported_model.as_deref()),
            reported_runtime_identity: result
                .and_then(|result| result.reported_runtime_identity.as_deref()),
        };
        print_json(&success_envelope(output))?;
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
        assert_eq!(args.language, SourceLanguage::Auto);
        assert!(args.json);
    }

    #[test]
    fn parses_all_source_languages_and_rejects_unknown_values() {
        for (value, expected) in [
            ("auto", SourceLanguage::Auto),
            ("zh", SourceLanguage::Zh),
            ("en", SourceLanguage::En),
        ] {
            let cli = Cli::try_parse_from([
                "bimyscribe",
                "transcribe",
                "BV1example",
                "--language",
                value,
            ])
            .unwrap();
            let Command::Transcribe(args) = cli.command else {
                panic!("expected transcribe command");
            };
            assert_eq!(args.language, expected);
        }
        let error =
            Cli::try_parse_from(["bimyscribe", "transcribe", "BV1example", "--language", "fr"])
                .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn parses_runtime_status_json_and_explicit_auto_matches_default() {
        let implicit = Cli::try_parse_from(["bimyscribe", "transcribe", "BV1example"]).unwrap();
        let explicit = Cli::try_parse_from([
            "bimyscribe",
            "transcribe",
            "BV1example",
            "--language",
            "auto",
        ])
        .unwrap();
        let Command::Transcribe(implicit) = implicit.command else {
            panic!("expected transcribe command");
        };
        let Command::Transcribe(explicit) = explicit.command else {
            panic!("expected transcribe command");
        };
        assert_eq!(implicit.language, explicit.language);

        let cli = Cli::try_parse_from(["bimyscribe", "runtime", "status", "--json"]).unwrap();
        let Command::Runtime(RuntimeArgs {
            command: RuntimeCommand::Status(args),
        }) = cli.command
        else {
            panic!("expected runtime status command");
        };
        assert!(args.json);
    }

    #[test]
    fn envelope_has_one_versioned_success_or_failure_shape() {
        let success = success_envelope(serde_json::json!({
            "requested_language": "en"
        }));
        let success_json = serde_json::to_value(&success).unwrap();
        assert_eq!(success_json["schema_version"], 1);
        assert_eq!(success_json["ok"], true);
        assert!(success_json["data"].is_object());
        assert!(success_json["error"].is_null());

        let failure = error_envelope(cli_error("runtime-not-ready", "not ready", Some("install")));
        let failure_json = serde_json::to_value(&failure).unwrap();
        assert_eq!(failure_json["schema_version"], 1);
        assert_eq!(failure_json["ok"], false);
        assert!(failure_json["data"].is_null());
        assert_eq!(failure_json["error"]["code"], "runtime-not-ready");
    }

    #[test]
    fn errors_map_to_stable_codes_and_exit_values() {
        let error = cli_error_from_message("runtime-contract-upgrade-required: v1");
        assert_eq!(error.code, "runtime-contract-upgrade-required");
        assert_eq!(exit_code_for(&error), ExitCode::from(EXIT_RUNTIME));
        let error = cli_error_from_message("final document is missing");
        assert_eq!(error.code, "artifact-failed");
        assert_eq!(exit_code_for(&error), ExitCode::from(EXIT_ARTIFACT));
        assert_eq!(exit_code_value(ExitCode::from(EXIT_PIPELINE)), 4);
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
