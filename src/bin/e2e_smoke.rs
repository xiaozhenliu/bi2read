//! Headless end-to-end smoke driver (no GUI window shown to completion).
//!
//! Runs the real pipeline against a short public B站 video:
//! metadata -> download -> ffmpeg normalize -> FunASR transcribe ->
//! raw markdown -> final full.md. Then prints the produced artifacts.
//!
//! Usage:
//!   cargo run --release --bin e2e_smoke -- <URL-or-BVID> [--keep]
//!
//! Requires: external drive mounted, ffmpeg on PATH, Docker Desktop running,
//! and the funasr-docker image built. This is NOT a unit test; it performs real
//! network + docker work and takes minutes (first run downloads ~3GB of models).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use slint::{ComponentHandle, Weak};
use uuid::Uuid;

use bi2read::config::Config;
use bi2read::jobs::{
    CreatedFrom, Job, RetentionPolicy, RuntimeBackend, RuntimeSource, SourceLanguage, StageState,
    TranscriptionSelection,
};
use bi2read::pipeline;

const E2E_WORKING_ROOT: &str = "/tmp/bi2read/e2e-jobs";
const E2E_OUTPUT_ROOT: &str = "/tmp/bi2read/e2e-output";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Execute,
    #[cfg(test)]
    DryRun,
}

struct E2ePaths {
    working_root: PathBuf,
    output_root: PathBuf,
}

impl Default for E2ePaths {
    fn default() -> Self {
        Self {
            working_root: PathBuf::from(E2E_WORKING_ROOT),
            output_root: PathBuf::from(E2E_OUTPUT_ROOT),
        }
    }
}

fn main() -> ExitCode {
    env_logger::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    run_cli(&args, &E2ePaths::default(), RunMode::Execute)
}

/// Shared CLI entry point. Tests use `DryRun` to exercise the complete local
/// execution lifecycle while replacing only the network/Docker pipeline.
fn run_cli(args: &[String], paths: &E2ePaths, mode: RunMode) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage:");
        eprintln!("  e2e_smoke <URL-or-BVID> [--keep]            # new job, full run");
        eprintln!(
            "  e2e_smoke --resume <job-dir> [--keep]       # load crashed job, recover, re-run"
        );
        return ExitCode::from(2);
    }
    let keep = args.iter().any(|a| a == "--keep");

    // --resume <job-dir>: load the crashed job's state.json, recover, and re-run.
    if args[0] == "--resume" {
        return run_resume(args, keep, paths, mode);
    }

    let input = &args[0];

    // Parse the input to a BVID + page.
    let parsed = match bi2read::bilibili::parse_url(input) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("URL parse failed: {e}");
            return ExitCode::from(2);
        }
    };
    let page = parsed.page.unwrap_or(1);
    eprintln!("parsed: bvid={} page={}", parsed.bvid, page);

    // Build a Job and give this invocation its own output root. The work root
    // is also separate from the App's production jobs directory.
    let id = Uuid::new_v4();
    let cfg = e2e_config(id, paths);
    eprintln!(
        "config: working_dir={} runtime_project={} output_dir={}",
        cfg.working_dir.display(),
        cfg.runtime_project
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "未配置".into()),
        cfg.output_dir.display()
    );

    let selection = match cfg.job_transcription_selection(CreatedFrom::Cli, SourceLanguage::Auto) {
        Ok(selection) => selection,
        Err(error) if is_dry_run(mode) => {
            eprintln!("dry-run: using isolated fixture transcription selection: {error}");
            TranscriptionSelection::new(
                RuntimeSource::External,
                cfg.working_dir.join("fixture-runtime-project"),
                cfg.working_dir.join("fixture-runtime-data"),
                "contract-v2:e2e-smoke-dry-run".into(),
                RuntimeBackend::DockerCompose,
                Some("fixture model".into()),
                SourceLanguage::Auto,
                CreatedFrom::Cli,
            )
        }
        Err(error) => {
            eprintln!("cannot freeze verified Runtime selection: {error}");
            return ExitCode::from(1);
        }
    };
    let mut job = Job::new_v05_disabled_with_selection(
        id,
        parsed.bvid.clone(),
        page,
        selection,
        RetentionPolicy::Recommended,
    );
    job.source_url = Some(input.clone());
    for s in bi2read::jobs::pipeline_stages() {
        job.set_stage_state(s, StageState::Pending);
    }

    eprintln!("running pipeline...");
    let work_dir = cfg.working_dir.join(job.id.to_string());
    execute_job(&mut job, &cfg, work_dir, keep, mode)
}

/// Load a crashed job from its `state.json`, run recovery, and
/// re-run the pipeline. Verifies that completed stages are skipped and the
/// crashed (Running) stage is reset to Pending and re-executed.
fn run_resume(args: &[String], keep: bool, paths: &E2ePaths, mode: RunMode) -> ExitCode {
    let job_dir = match args.get(1) {
        Some(d) => match resolve_resume_job_dir(Path::new(d), &paths.working_root) {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("refusing to resume: {e}");
                return ExitCode::from(2);
            }
        },
        None => {
            eprintln!("--resume requires a job directory argument");
            return ExitCode::from(2);
        }
    };
    let state_json = job_dir.join("state.json");
    let mut job = match bi2read::jobs::load_job_state(&job_dir) {
        Some(j) => j,
        None => {
            eprintln!("no state.json at {}", state_json.display());
            return ExitCode::from(1);
        }
    };
    let expected_job_dir_name = job.id.to_string();
    if job_dir.file_name().and_then(|name| name.to_str()) != Some(expected_job_dir_name.as_str()) {
        eprintln!(
            "refusing to resume: job directory name must match state job id {}",
            job.id
        );
        return ExitCode::from(2);
    }
    // Trust the validated argument rather than a possibly stale serialized
    // path, so recovery revalidates artifacts in the directory being resumed.
    job.work_dir = Some(job_dir.clone());
    eprintln!(
        "loaded crashed job: stage={} status={} transcribe_state={:?}",
        job.stage.name(),
        job.status.label(),
        job.stage_state(bi2read::jobs::Stage::Transcribe)
    );

    // Run the queue recovery: resets Running -> Pending, revalidates Completed.
    let mut queue = bi2read::jobs::Queue {
        jobs: vec![job.clone()],
    };
    let reports = bi2read::jobs::recover(&mut queue);
    let r = &reports[0];
    eprintln!(
        "after recover: reset={:?} revalidated={:?}",
        r.reset_stages, r.revalidated_stages
    );
    job = queue.jobs[0].clone();
    eprintln!(
        "post-recover stage={} transcribe_state={:?}",
        job.stage.name(),
        job.stage_state(bi2read::jobs::Stage::Transcribe)
    );

    // The validated job directory is `<E2E_WORKING_ROOT>/<job-id>`, matching
    // the location `run_job` resolves from this isolated config.
    let cfg = e2e_config(Uuid::new_v4(), paths);

    eprintln!("re-running pipeline (completed stages should be skipped)...");
    execute_job(&mut job, &cfg, job_dir, keep, mode)
}

fn execute_job(
    job: &mut Job,
    cfg: &Config,
    work_dir: PathBuf,
    keep: bool,
    mode: RunMode,
) -> ExitCode {
    let start = std::time::Instant::now();
    begin_run(job, work_dir);
    let result = match mode {
        RunMode::Execute => {
            // Take a weak handle, then drop the strong handle so this headless
            // binary never shows a window. UI updates become no-ops.
            let app: bi2read::App = match bi2read::App::new() {
                Ok(app) => app,
                Err(e) => {
                    eprintln!("failed to init App (needed for Weak handle): {e}");
                    return ExitCode::from(1);
                }
            };
            let weak: Weak<bi2read::App> = app.as_weak();
            drop(app);
            let cancel_token = bi2read::cancel::CancellationToken::new();
            pipeline::run_job(job, cfg, &weak, &cancel_token)
        }
        #[cfg(test)]
        RunMode::DryRun => {
            eprintln!("dry-run: local lifecycle executed; external pipeline skipped");
            Ok(())
        }
    };
    report(&result, job, cfg, start, keep)
}

/// Start from built-in defaults instead of reading the production App config.
/// Dedicated paths plus a per-invocation output root keep smoke runs isolated
/// from both real user state and one another.
fn e2e_config(run_id: Uuid, paths: &E2ePaths) -> Config {
    Config {
        working_dir: paths.working_root.clone(),
        output_dir: e2e_output_dir(run_id, paths),
        ..Config::default()
    }
}

fn is_dry_run(mode: RunMode) -> bool {
    #[cfg(test)]
    {
        mode == RunMode::DryRun
    }
    #[cfg(not(test))]
    {
        let _ = mode;
        false
    }
}

fn e2e_output_dir(run_id: Uuid, paths: &E2ePaths) -> PathBuf {
    paths.output_root.join(run_id.to_string())
}

/// Resolve and validate a resume directory without ever opening production
/// job state. Canonicalization also prevents `..` and symlinks from escaping
/// the dedicated E2E root.
fn resolve_resume_job_dir(job_dir: &Path, working_root: &Path) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(working_root).map_err(|e| {
        format!(
            "cannot resolve E2E jobs root {}: {e}",
            working_root.display()
        )
    })?;
    let job_dir = std::fs::canonicalize(job_dir)
        .map_err(|e| format!("cannot resolve job directory {}: {e}", job_dir.display()))?;
    if !is_direct_job_dir(&root, &job_dir) {
        return Err(format!(
            "{} is not a direct child of the dedicated E2E jobs root {}",
            job_dir.display(),
            root.display()
        ));
    }
    Ok(job_dir)
}

fn is_direct_job_dir(root: &Path, job_dir: &Path) -> bool {
    job_dir.file_name().is_some() && job_dir.parent() == Some(root)
}

/// Set and persist the state normally supplied by the production scheduler.
fn begin_run(job: &mut Job, work_dir: PathBuf) {
    prepare_run_state(job, work_dir);
    if let Err(e) = bi2read::jobs::save_job_state(job) {
        eprintln!("warning: failed to persist E2E start state: {e}");
    }
}

fn prepare_run_state(job: &mut Job, work_dir: PathBuf) {
    job.work_dir = Some(work_dir);
    job.started_at = Some(chrono::Utc::now());
    job.finished_at = None;
}

/// Print the outcome (artifacts produced) for a completed or failed run.
fn report(
    result: &Result<(), bi2read::pipeline::PipelineError>,
    job: &mut Job,
    cfg: &Config,
    start: std::time::Instant,
    keep: bool,
) -> ExitCode {
    finish_run(job);
    if let Err(e) = bi2read::jobs::save_job_state(job) {
        eprintln!("warning: failed to persist final E2E state: {e}");
    }

    match result {
        Ok(()) => {
            eprintln!(
                "pipeline completed in {:.1}s",
                start.elapsed().as_secs_f64()
            );
            if let Some(dir) = &job.work_dir {
                eprintln!("job dir: {}", dir.display());
                for name in [
                    "metadata.json",
                    "transcript.raw.json",
                    "transcript.raw.md",
                    "transcript.readable.md",
                    "content-current.v1.json",
                ] {
                    let p = dir.join(name);
                    match std::fs::metadata(&p) {
                        Ok(m) => eprintln!("  - {} ({} bytes)", name, m.len()),
                        Err(_) => eprintln!("  - {} MISSING", name),
                    }
                }
                let out_dir = job
                    .final_output_dir
                    .as_deref()
                    .unwrap_or(cfg.output_dir.as_path());
                let out_full = out_dir.join("full.md");
                match std::fs::metadata(&out_full) {
                    Ok(m) => {
                        eprintln!("output full.md: {} ({} bytes)", out_full.display(), m.len())
                    }
                    Err(_) => eprintln!("output full.md: MISSING at {}", out_full.display()),
                }
                if serde_json::to_value(&*job)
                    .ok()
                    .and_then(|value| value.get("content_setup").cloned())
                    .is_some()
                    && dir.join("full.md").exists()
                {
                    eprintln!("warning: v0.5 work-dir full.md mirror unexpectedly exists");
                }
            }
            if !keep {
                if let Some(dir) = &job.work_dir {
                    eprintln!("cleaning up this job dir: {}", dir.display());
                    let _ = std::fs::remove_dir_all(dir);
                }
                eprintln!(
                    "cleaning up this run's output: {}",
                    cfg.output_dir.display()
                );
                let _ = std::fs::remove_dir_all(&cfg.output_dir);
            } else {
                eprintln!("--keep: retained job dir: {:?}", job.work_dir);
                let retained_output = job
                    .final_output_dir
                    .as_deref()
                    .unwrap_or(cfg.output_dir.as_path());
                eprintln!("--keep: retained output: {}", retained_output.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!(
                "pipeline FAILED after {:.1}s: {e}",
                start.elapsed().as_secs_f64()
            );
            eprintln!("job dir retained for inspection: {:?}", job.work_dir);
            eprintln!(
                "output root retained for inspection: {}",
                cfg.output_dir.display()
            );
            eprintln!("failed runs are always retained; --keep is not required");
            ExitCode::from(1)
        }
    }
}

fn finish_run(job: &mut Job) {
    job.finished_at = Some(chrono::Utc::now());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static HOME_LOCK: Mutex<()> = Mutex::new(());

    struct TestHome {
        previous: Option<OsString>,
        root: PathBuf,
    }

    impl TestHome {
        fn install(home: &Path, root: PathBuf) -> Self {
            let previous = std::env::var_os("HOME");
            std::env::set_var("HOME", home);
            Self { previous, root }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                std::env::set_var("HOME", previous);
            } else {
                std::env::remove_var("HOME");
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn resume_path_must_be_a_direct_child_of_e2e_root() {
        let root = Path::new(E2E_WORKING_ROOT);
        let job_id = Uuid::new_v4().to_string();

        assert!(is_direct_job_dir(root, &root.join(&job_id)));
        assert!(!is_direct_job_dir(root, root));
        assert!(!is_direct_job_dir(root, &root.join(&job_id).join("nested")));
        assert!(!is_direct_job_dir(
            root,
            Path::new("/tmp/bi2read/production-jobs")
                .join(&job_id)
                .as_path()
        ));
    }

    #[test]
    fn headless_run_records_scheduler_timestamps() {
        let id = Uuid::new_v4();
        let mut job = Job::new(id, "BV1test".into(), 1);
        let work_dir = PathBuf::from(E2E_WORKING_ROOT).join(id.to_string());

        prepare_run_state(&mut job, work_dir.clone());
        let started_at = job.started_at;
        finish_run(&mut job);

        assert_eq!(job.work_dir, Some(work_dir));
        assert!(started_at.is_some());
        assert!(job.finished_at >= started_at);
    }

    #[test]
    fn every_invocation_gets_an_independent_output_root() {
        let paths = E2ePaths::default();
        let first = e2e_output_dir(Uuid::new_v4(), &paths);
        let second = e2e_output_dir(Uuid::new_v4(), &paths);

        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(Path::new(E2E_OUTPUT_ROOT)));
        assert_eq!(second.parent(), Some(Path::new(E2E_OUTPUT_ROOT)));
    }

    #[test]
    fn cli_new_and_resume_execution_leave_production_queue_byte_identical() {
        let _home_lock = HOME_LOCK.lock().unwrap();
        let sandbox = std::env::temp_dir().join(format!("bi2read-e2e-smoke-{}", Uuid::new_v4()));
        let home = sandbox.join("home");
        let working_root = sandbox.join("e2e-jobs");
        let output_root = sandbox.join("e2e-output");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&working_root).unwrap();
        let _home = TestHome::install(&home, sandbox.clone());
        let paths = E2ePaths {
            working_root: working_root.clone(),
            output_root: output_root.clone(),
        };

        let production_queue = home
            .join("Library")
            .join("Application Support")
            .join("bi2read")
            .join("queue.json");
        std::fs::create_dir_all(production_queue.parent().unwrap()).unwrap();
        let sentinel = br#"{"production":"queue must stay byte-identical"}"#;
        std::fs::write(&production_queue, sentinel).unwrap();

        let cfg = e2e_config(Uuid::new_v4(), &paths);
        assert_eq!(cfg.working_dir, working_root);
        assert_eq!(cfg.output_dir.parent(), Some(output_root.as_path()));

        let before_new = std::fs::read(&production_queue).unwrap();
        let new_args = vec!["BV1xx411c7mD".to_string(), "--keep".to_string()];
        assert_eq!(
            run_cli(&new_args, &paths, RunMode::DryRun),
            ExitCode::SUCCESS
        );
        assert_eq!(std::fs::read(&production_queue).unwrap(), before_new);
        assert_eq!(
            std::fs::read_dir(&working_root).unwrap().count(),
            1,
            "dry execution must persist its isolated job state"
        );
        let smoke_job_dir = std::fs::read_dir(&working_root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let smoke_state = bi2read::jobs::load_job_state(&smoke_job_dir).unwrap();
        let smoke_json = serde_json::to_value(smoke_state).unwrap();
        assert!(smoke_json["transcription_selection"].is_object());
        assert!(smoke_json["content_setup"].is_object());

        let id = Uuid::new_v4();
        let job_dir = working_root.join(id.to_string());
        std::fs::create_dir_all(&job_dir).unwrap();
        let mut job = Job::new(id, "BV1xx411c7mD".into(), 1);
        job.work_dir = Some(job_dir.clone());
        job.status = bi2read::jobs::JobStatus::Running;
        job.stage = bi2read::jobs::Stage::Transcribe;
        job.set_stage_state(bi2read::jobs::Stage::Transcribe, StageState::Running);
        bi2read::jobs::save_job_state(&job).unwrap();

        let before_resume = std::fs::read(&production_queue).unwrap();
        let resume_args = vec![
            "--resume".to_string(),
            job_dir.display().to_string(),
            "--keep".to_string(),
        ];
        assert_eq!(
            run_cli(&resume_args, &paths, RunMode::DryRun),
            ExitCode::SUCCESS
        );
        assert_eq!(std::fs::read(&production_queue).unwrap(), before_resume);
        let resumed = bi2read::jobs::load_job_state(&job_dir).unwrap();
        assert!(resumed.started_at.is_some());
        assert!(resumed.finished_at.is_some());
    }
}
