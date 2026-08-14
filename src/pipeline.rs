//! Pipeline execution and simulation.
//!
//! `stage_sequence()` is the ordered list of happy-path stages used by the UI
//! to render the per-job stages list. `simulate_progress` drives the UI with
//! fake progress so the interface can be evaluated end-to-end without real
//! downloads/transcription. `run_job` is the real pipeline entry point and is
//! a stub returning `NotImplemented` until later steps.

use std::thread;
use std::time::Duration;

use slint::{Model, Weak};
use uuid::Uuid;

use crate::cancel::CancellationToken;
use crate::jobs::{pipeline_stages, Stage};
use crate::App;

/// The ordered pipeline stages, as owned `Stage` values (for the UI).
pub fn stage_sequence() -> Vec<Stage> {
    pipeline_stages()
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("bilibili: {0}")]
    Bilibili(String),
    #[error("ffmpeg: {0}")]
    Ffmpeg(String),
    #[error("funasr: {0}")]
    Funasr(String),
    #[error("llm: {0}")]
    Llm(String),
    #[error("document: {0}")]
    Document(String),
    #[error("io: {0}")]
    Io(String),
    #[error("docker not running")]
    DockerNotRunning,
    #[error("external drive not mounted")]
    DriveNotMounted,
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

impl From<crate::bilibili::MetadataError> for PipelineError {
    fn from(e: crate::bilibili::MetadataError) -> Self {
        PipelineError::Bilibili(e.to_string())
    }
}
impl From<crate::funasr::FunasrError> for PipelineError {
    fn from(e: crate::funasr::FunasrError) -> Self {
        PipelineError::Funasr(e.to_string())
    }
}
impl From<std::io::Error> for PipelineError {
    fn from(e: std::io::Error) -> Self {
        PipelineError::Io(e.to_string())
    }
}

/// External drive mounted check: a `/Volumes/<drive>/...` path is "mounted" only
/// if its immediate `/Volumes/<drive>` root exists. If the drive is
/// absent, the whole queue waits rather than falling back to the system disk.
/// For paths not under `/Volumes`, the first existing ancestor must be reachable.
pub fn drive_mounted(working_dir: &std::path::Path) -> bool {
    use std::path::Component;

    // Collect the path components so we can inspect the /Volumes/<drive> prefix.
    let comps: Vec<Component> = working_dir.components().collect();
    // Detect a `/Volumes/<name>/...` layout.
    let mut it = comps.iter();
    if matches!(it.next(), Some(Component::RootDir))
        && matches!(it.next(), Some(Component::Normal(n)) if n.to_str() == Some("Volumes"))
    {
        // The next component is the drive name; its /Volumes/<name> must exist.
        if let Some(Component::Normal(drive)) = it.next() {
            let mount = std::path::Path::new("/Volumes").join(drive);
            return mount.exists();
        }
        // Just `/Volumes` itself: treat as mounted if it exists.
        return std::path::Path::new("/Volumes").exists();
    }

    // Non-/Volumes path: walk up to the first existing ancestor.
    let mut cur = std::path::PathBuf::from(working_dir);
    while !cur.exists() {
        if !cur.pop() {
            return false;
        }
    }
    true
}

/// Real pipeline entry point. Runs `job` through every stage,
/// skipping already-Completed stages whose artifacts validate. Updates the UI
/// row in place via the weak App handle. Persists `state.json` after each stage.
///
/// Returns `Ok(())` on completion, or `Err` on a fatal stage failure (the job
/// is marked Failed with the error message). Cancellation is cooperative: the
/// `cancel_token` is checked before and after each stage and after every long
/// blocking call. When cancelled, `Err(PipelineError::Cancelled)` is returned.
/// Cancellation returns a structured error and leaves recovery state intact.
pub fn run_job(
    job: &mut crate::jobs::Job,
    cfg: &crate::config::Config,
    app: &Weak<App>,
    cancel_token: &CancellationToken,
) -> Result<(), PipelineError> {
    // Check cancellation before starting.
    cancel_token.check()?;

    // Resolve the work directory for this job (on the external drive).
    // `ensure_work_dir` creates the dir and sets `job.work_dir` as a side effect.
    let _ = ensure_work_dir(job, cfg)?;

    let cleanup_was_complete =
        job.stage_state(Stage::Cleanup) == crate::jobs::StageState::Completed;
    let mut rebuilt = false;

    rebuilt |= run_stage(
        job,
        Stage::Metadata,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_metadata(job, app, cfg),
    )?;
    rebuilt |= run_stage(
        job,
        Stage::DownloadAudio,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_download_audio(job, app, cfg, cancel_token),
    )?;
    rebuilt |= run_stage(
        job,
        Stage::NormalizeAudio,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_normalize_audio(job, app, cfg, cancel_token),
    )?;
    rebuilt |= run_stage(
        job,
        Stage::Transcribe,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_transcribe(job, app, cfg, cancel_token),
    )?;
    rebuilt |= run_stage(
        job,
        Stage::RawDocument,
        app,
        cancel_token,
        cleanup_was_complete,
        stage_raw_document,
    )?;
    rebuilt |= run_stage(
        job,
        Stage::ReadableDocument,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_readable_document(job, app, cfg),
    )?;
    skip_stage(job, Stage::Screenshots, app, cancel_token)?;
    rebuilt |= run_stage(
        job,
        Stage::FinalDocument,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_final_document(job, app, cfg),
    )?;
    if rebuilt {
        job.set_stage_state(Stage::Cleanup, crate::jobs::StageState::Pending);
    }
    run_stage(job, Stage::Cleanup, app, cancel_token, false, |job, app| {
        stage_cleanup(job, app, cfg)
    })?;

    // Check cancellation before marking completed.
    if cancel_token.is_cancelled() {
        return Err(PipelineError::Cancelled);
    }

    // Mark completed.
    mark_job_completed(job);
    set_job_stage(
        app,
        job.id,
        Stage::Completed.name(),
        Stage::Completed.label(),
        false,
    );
    set_job_progress(app, job.id, 100);
    crate::jobs::save_job_state(job)?;
    Ok(())
}

fn mark_job_completed(job: &mut crate::jobs::Job) {
    job.stage = Stage::Completed;
    job.status = crate::jobs::JobStatus::Completed;
    job.stage_progress = 100;
    for stage in crate::jobs::pipeline_stages() {
        if stage == Stage::Screenshots {
            job.set_stage_state(stage, crate::jobs::StageState::Skipped);
        } else if job.stage_state(stage) != crate::jobs::StageState::Skipped {
            job.set_stage_state(stage, crate::jobs::StageState::Completed);
        }
    }
}

/// Wrap a stage: set Running, skip if Completed+valid, run, mark Completed,
/// persist state. On error, mark the stage Failed and the job Failed. Checks
/// the cancellation token before and after the stage body.
fn run_stage<F>(
    job: &mut crate::jobs::Job,
    stage: Stage,
    app: &Weak<App>,
    cancel_token: &CancellationToken,
    allow_retention_removal: bool,
    f: F,
) -> Result<bool, PipelineError>
where
    F: FnOnce(&mut crate::jobs::Job, &Weak<App>) -> Result<(), PipelineError>,
{
    // Check cancellation before starting the stage.
    cancel_token.check()?;

    // Skip if already completed and its artifact still validates.
    if job.stage_state(stage) == crate::jobs::StageState::Completed {
        if let Some(dir) = &job.work_dir {
            if crate::jobs::artifact_valid(stage, dir, job)
                || (allow_retention_removal && crate::jobs::artifact_expected_removed(job, stage))
            {
                return Ok(false);
            }
            // artifact invalid -> re-run
            job.set_stage_state(stage, crate::jobs::StageState::Pending);
        }
    }
    if job.stage_state(stage) == crate::jobs::StageState::Skipped {
        return Ok(false);
    }

    job.stage = stage;
    job.status = crate::jobs::JobStatus::Running;
    job.stage_progress = 0;
    job.set_stage_state(stage, crate::jobs::StageState::Running);
    set_job_stage(
        app,
        job.id,
        stage.name(),
        stage.label(),
        stage == Stage::Transcribe,
    );
    crate::jobs::save_job_state(job)?;

    match f(job, app) {
        Ok(()) => {
            // Check cancellation after the stage body.
            cancel_token.check()?;
            if job.stage_state(stage) == crate::jobs::StageState::Running {
                job.set_stage_state(stage, crate::jobs::StageState::Completed);
            }
            crate::jobs::save_job_state(job)?;
            Ok(true)
        }
        Err(e) => {
            job.set_stage_state(stage, crate::jobs::StageState::Failed);
            job.status = crate::jobs::JobStatus::Failed;
            job.error = Some(e.to_string());
            crate::jobs::save_job_state(job)?;
            // Reflect failure in the UI row.
            set_job_error(app, job.id, &e.to_string());
            Err(e)
        }
    }
}

fn skip_stage(
    job: &mut crate::jobs::Job,
    stage: Stage,
    app: &Weak<App>,
    cancel_token: &CancellationToken,
) -> Result<(), PipelineError> {
    cancel_token.check()?;
    job.stage = stage;
    job.stage_progress = 100;
    job.set_stage_state(stage, crate::jobs::StageState::Skipped);
    set_job_stage(app, job.id, stage.name(), stage.label(), false);
    set_job_progress(app, job.id, 100);
    crate::jobs::save_job_state(job)?;
    Ok(())
}

/// Create the job's work directory on the external drive and attach it to the job.
fn ensure_work_dir(
    job: &mut crate::jobs::Job,
    cfg: &crate::config::Config,
) -> Result<std::path::PathBuf, PipelineError> {
    let configured_or_existing = job.work_dir.as_ref().unwrap_or(&cfg.working_dir);
    if !drive_mounted(configured_or_existing) {
        job.stage = Stage::WaitingForDrive;
        job.status = crate::jobs::JobStatus::WaitingForDrive;
        return Err(PipelineError::DriveNotMounted);
    }

    let dir = match &job.work_dir {
        Some(existing) => existing.clone(),
        None => {
            let readable = cfg.working_dir.join(job_directory_name(job, false));
            if readable.exists() {
                cfg.working_dir.join(job_directory_name(job, true))
            } else {
                readable
            }
        }
    };
    std::fs::create_dir_all(&dir)?;
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs)?;
    job.work_dir = Some(dir.clone());
    Ok(dir)
}

fn job_directory_name(job: &crate::jobs::Job, full_uuid: bool) -> String {
    let created = job.created_at.unwrap_or_else(chrono::Utc::now);
    let mut source: String = job
        .bvid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    if source.is_empty() {
        source.push_str("job");
    }
    let uuid = job.id.simple().to_string();
    let suffix = if full_uuid { &uuid[..] } else { &uuid[..8] };

    format!(
        "{}_{}_P{}_{}",
        created.format("%Y%m%d-%H%M%SZ"),
        source,
        job.page,
        suffix
    )
}

// ---- Individual stages ----

fn stage_metadata(
    job: &mut crate::jobs::Job,
    app: &Weak<App>,
    _cfg: &crate::config::Config,
) -> Result<(), PipelineError> {
    // Resolve short links first.
    let bvid = if job.bvid.starts_with("b23:") {
        let code = job.bvid.trim_start_matches("b23:");
        let resolved = crate::bilibili::resolve_short_link(code)?;
        apply_resolved_short_link(job, &resolved)
    } else {
        job.bvid.clone()
    };
    job.bvid = bvid.clone();

    let meta = crate::bilibili::fetch_metadata(&bvid, Some(job.page))?;
    // The metadata API resolves legacy av ids to the canonical BVID.
    job.bvid = meta.bvid.clone();
    job.cid = Some(meta.cid);
    job.up_name = Some(meta.up_name.clone());
    job.duration_ms = Some(meta.duration_ms);
    job.title = meta.title.clone();

    // Persist metadata.json (atomic) with the job-id marker.
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let md_json = serde_json::json!({
        "bvid": meta.bvid,
        "cid": meta.cid,
        "title": meta.title,
        "up_name": meta.up_name,
        "duration_ms": meta.duration_ms,
        "page": job.page,
        "job_id": job.id.to_string(),
    });
    let data = serde_json::to_vec_pretty(&md_json).map_err(|e| PipelineError::Io(e.to_string()))?;
    crate::jobs::atomic_write(&dir.join("metadata.json"), &data)?;

    set_job_progress(app, job.id, 100);
    Ok(())
}

/// Apply the video identity carried by a b23 redirect to the pending job.
/// Redirect query parameters are authoritative because the short URL itself
/// commonly has no page information.
fn apply_resolved_short_link(job: &mut crate::jobs::Job, resolved: &str) -> String {
    match crate::bilibili::parse_url(resolved) {
        Ok(parsed) => {
            if let Some(page) = parsed.page {
                job.page = page;
            }
            parsed.bvid
        }
        Err(_) => resolved.to_string(),
    }
}

fn stage_download_audio(
    job: &mut crate::jobs::Job,
    app: &Weak<App>,
    _cfg: &crate::config::Config,
    cancel_token: &CancellationToken,
) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let bvid = job.bvid.clone();
    let cid = job
        .cid
        .ok_or_else(|| PipelineError::Bilibili("no cid".into()))?;

    // Fetch the audio stream URL (retry once if it 403s on download).
    let dest = dir.join("source.audio");
    let mut last_err: Option<PipelineError> = None;
    for _attempt in 0..2 {
        // Check cancellation before each download attempt.
        cancel_token.check()?;
        let url = match crate::bilibili::fetch_playurl_audio(&bvid, cid) {
            Ok(u) => u,
            Err(e) => {
                last_err = Some(e.into());
                continue;
            }
        };
        match crate::bilibili::download_audio(&url, &dest, None, |bytes, total| {
            let pct = if total > 0 {
                ((bytes as f64 / total as f64) * 100.0) as u8
            } else {
                0
            };
            set_job_progress(app, job.id, pct.min(99));
        }) {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e.into());
                // URL may have expired; loop re-fetches playurl.
            }
        }
    }
    // Check cancellation after the download.
    cancel_token.check()?;
    if let Some(e) = last_err {
        return Err(e);
    }
    set_job_progress(app, job.id, 100);
    Ok(())
}

fn stage_normalize_audio(
    job: &mut crate::jobs::Job,
    app: &Weak<App>,
    cfg: &crate::config::Config,
    cancel_token: &CancellationToken,
) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let src = dir.join("source.audio");
    let dest = dir.join("normalized.wav");
    let log = dir.join("logs").join("ffmpeg.log");

    // ffmpeg -y -i <src> -ac 1 -ar 16000 -c:a pcm_s16le <dest>
    let spec = crate::process::SubprocessSpec::new(vec![
        "ffmpeg".into(),
        "-y".into(),
        "-i".into(),
        src.to_string_lossy().into_owned(),
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        "16000".into(),
        "-c:a".into(),
        "pcm_s16le".into(),
        dest.to_string_lossy().into_owned(),
    ])
    .log(&log);
    // Spawn with a handle so we can cancel the process group.
    let mut handle =
        crate::process::spawn(spec).map_err(|e| PipelineError::Ffmpeg(e.to_string()))?;
    let code = loop {
        if cancel_token.is_cancelled() {
            let _ = handle.cancel();
            return Err(PipelineError::Cancelled);
        }
        match handle.is_running() {
            true => std::thread::sleep(std::time::Duration::from_millis(100)),
            false => break handle.wait(),
        }
    }
    .map_err(|e| PipelineError::Ffmpeg(e.to_string()))?;
    if code != 0 {
        return Err(PipelineError::Ffmpeg(format!("ffmpeg exited {}", code)));
    }
    // Verify output is readable & non-empty.
    let probe = crate::process::SubprocessSpec::new(vec![
        "ffprobe".into(),
        "-v".into(),
        "error".into(),
        "-show_entries".into(),
        "stream=sample_rate,channels".into(),
        "-of".into(),
        "default=noprint_wrappers=1".into(),
        dest.to_string_lossy().into_owned(),
    ]);
    let _ = crate::process::run(probe);
    let _ = cfg; // future: configurable sample rate
    set_job_progress(app, job.id, 100);
    Ok(())
}

fn stage_transcribe(
    job: &mut crate::jobs::Job,
    app: &Weak<App>,
    cfg: &crate::config::Config,
    cancel_token: &CancellationToken,
) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let normalized = dir.join("normalized.wav");
    let log = dir.join("logs").join("funasr.log");
    let project_dir = cfg
        .effective_runtime_project()
        .ok_or_else(|| PipelineError::Io("FunASR Runtime 未配置".into()))?;

    let utterances = match crate::funasr::run(crate::funasr::TranscribeRequest {
        job_dir: &dir,
        normalized_wav: &normalized,
        project_dir: &project_dir,
        runtime_data_dir: &cfg.runtime_data_dir,
        log_path: &log,
        job_id: &job.id,
        instance_nonce: &cfg.instance_nonce,
        cancel_token,
    }) {
        Ok(utterances) => utterances,
        Err(crate::funasr::FunasrError::DockerUnavailable) => {
            job.stage = Stage::NeedsUserAction;
            job.status = crate::jobs::JobStatus::NeedsUserAction;
            set_job_stage(
                app,
                job.id,
                Stage::NeedsUserAction.name(),
                Stage::NeedsUserAction.label(),
                false,
            );
            crate::jobs::save_job_state(job)?;
            return Err(PipelineError::DockerNotRunning);
        }
        Err(error) => return Err(error.into()),
    };
    // Store utterances count for display; the raw json is already written by funasr::run.
    let _ = utterances.len();
    set_job_progress(app, job.id, 100);
    Ok(())
}

fn stage_raw_document(job: &mut crate::jobs::Job, app: &Weak<App>) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let utts = load_utterances(&dir.join("transcript.raw.json"))?;
    let md = crate::document::render_raw_markdown(job, &utts);
    let path = dir.join("transcript.raw.md");
    crate::jobs::atomic_write(&path, md.as_bytes())?;
    set_job_progress(app, job.id, 100);
    Ok(())
}

fn stage_readable_document(
    job: &mut crate::jobs::Job,
    app: &Weak<App>,
    cfg: &crate::config::Config,
) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let utts = load_utterances(&dir.join("transcript.raw.json"))?;

    let readable: Option<Vec<String>> = if cfg.llm_enabled {
        if let Some(conn) = &cfg.llm_connection {
            match crate::llm::refine_utterances(conn, &utts) {
                Ok(refined) => {
                    // Clear any previous LLM fallback warning on success.
                    if matches!(
                        job.warning,
                        Some(crate::jobs::JobWarning::LlmFallback { .. })
                    ) {
                        job.warning = None;
                    }
                    Some(refined)
                }
                Err(e) => {
                    // Set a persistent non-fatal warning instead of just
                    // logging.
                    job.warning = Some(crate::jobs::JobWarning::LlmFallback {
                        message: e.to_string(),
                    });
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // If LLM produced text, write transcript.readable.md; otherwise mark Skipped
    // and write a rule-based fallback so full.md still has a body.
    let body = crate::document::render_readable_body(job, &utts, readable.as_deref());
    let path = dir.join("transcript.readable.md");
    crate::jobs::atomic_write(&path, body.as_bytes())?;
    if !cfg.llm_enabled {
        job.set_stage_state(Stage::ReadableDocument, crate::jobs::StageState::Skipped);
    }
    set_job_progress(app, job.id, 100);
    Ok(())
}

fn stage_final_document(
    job: &mut crate::jobs::Job,
    app: &Weak<App>,
    cfg: &crate::config::Config,
) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let utts = load_utterances(&dir.join("transcript.raw.json"))?;
    let raw_md = dir.join("transcript.raw.md");

    let readable = std::fs::read_to_string(dir.join("transcript.readable.md")).ok();
    let meta = crate::document::DocMeta {
        title: job.title.clone(),
        up_name: job.up_name.clone().unwrap_or_default(),
        duration_ms: job.duration_ms.unwrap_or(0),
        bvid: job.bvid.clone(),
        page: job.page,
        source_url: job.source_url.clone(),
    };
    let md = crate::document::render_full_markdown(job, &meta, &utts, readable.as_deref(), &raw_md);

    // Write into the configured output dir; fall back to the job dir.
    let out_dir = if cfg.output_dir.as_os_str().is_empty() {
        dir.clone()
    } else {
        let safe_title = sanitize(&job.title);
        let name = format!("{} [{}-P{}]", safe_title, job.bvid, job.page);
        cfg.output_dir.join(name)
    };
    std::fs::create_dir_all(&out_dir)?;
    let full_path = out_dir.join("full.md");
    crate::jobs::atomic_write(&full_path, md.as_bytes())?;

    // Store the final output dir so Speaker edits can rebuild it without
    // inferring from the current config.
    if out_dir != dir {
        job.final_output_dir = Some(out_dir.clone());
        // Also mirror full.md into the job dir for reveal-in-finder consistency.
        let _ = std::fs::copy(&full_path, dir.join("full.md"));
    } else {
        job.final_output_dir = Some(dir.clone());
    }
    set_job_progress(app, job.id, 100);
    Ok(())
}

fn stage_cleanup(
    job: &mut crate::jobs::Job,
    app: &Weak<App>,
    cfg: &crate::config::Config,
) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let _ = cfg; // cleanup is driven by job.retention, not global config
    match job.retention {
        crate::jobs::RetentionPolicy::KeepAll => {
            // Keep everything; only remove the funasr temp copies if any linger.
        }
        crate::jobs::RetentionPolicy::Recommended => {
            // Delete audio/video/temp, keep metadata/raw/readable/speaker-map/logs.
            for f in ["source.audio", "source.video", "normalized.wav"] {
                let _ = std::fs::remove_file(dir.join(f));
            }
        }
        crate::jobs::RetentionPolicy::DocumentsOnly => {
            // Delete all audio + raw data, keep only .md documents.
            for f in [
                "source.audio",
                "source.video",
                "normalized.wav",
                "transcript.raw.json",
                "transcript.readable.md",
                "metadata.json",
            ] {
                let _ = std::fs::remove_file(dir.join(f));
            }
        }
    }
    set_job_progress(app, job.id, 100);
    Ok(())
}

/// Load utterances from a previously-written transcript.raw.json.
pub fn load_utterances(
    path: &std::path::Path,
) -> Result<Vec<crate::funasr::Utterance>, PipelineError> {
    let data = std::fs::read(path)?;
    let utts: Vec<crate::funasr::Utterance> =
        serde_json::from_slice(&data).map_err(|e| PipelineError::Io(e.to_string()))?;
    Ok(utts)
}

/// Strip characters that are unsafe in a directory name.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | ':' | '\\' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Drive the UI with simulated progress for one job. The job is identified by
/// `job_id`; the matching `JobRow` in `App.jobs` is updated stage by stage.
///
/// This runs on a background thread and only touches the UI via
/// `Weak::upgrade_in_event_loop`, so it is `Send`-safe.
pub fn simulate_progress(job_id: Uuid, app: Weak<App>) {
    let stages = stage_sequence();
    // Skip Queued (index 0) since the row starts there; begin at Metadata.
    for (_i, stage) in stages.iter().enumerate().skip(1) {
        let is_transcribe = *stage == Stage::Transcribe;
        let stage_name = stage.name().to_string();
        let stage_label = stage.label().to_string();
        set_job_stage(&app, job_id, &stage_name, &stage_label, is_transcribe);

        if is_transcribe {
            thread::sleep(Duration::from_millis(4000));
            set_job_progress(&app, job_id, 100);
            continue;
        }

        let steps = 20u8;
        for s in 0..=steps {
            let pct = s * 100 / steps;
            set_job_progress(&app, job_id, pct);
            thread::sleep(Duration::from_millis(100));
        }
    }

    set_job_stage(
        &app,
        job_id,
        Stage::Completed.name(),
        Stage::Completed.label(),
        false,
    );
    set_job_progress(&app, job_id, 100);
}

/// Apply a full job snapshot to the UI: updates the JobRow and (if selected) the
/// detail panel + stages model in a single event loop callback.
pub fn apply_snapshot(app: &Weak<App>, snapshot: crate::jobs::JobViewSnapshot) {
    let app = app.clone();
    app.upgrade_in_event_loop(move |app| {
        apply_snapshot_inner(&app, &snapshot);
    })
    .ok();
}

fn apply_snapshot_inner(app: &crate::App, snap: &crate::jobs::JobViewSnapshot) {
    let id = snap.id.to_string();

    // Build the StageView list from the snapshot (reads persisted StageState,
    // not inferred from the stage's position in the sequence.
    let stage_views = stage_views_from_snapshot(snap);

    // Update the stages model.
    if let Some(vm) = app
        .get_stages()
        .as_any()
        .downcast_ref::<slint::VecModel<crate::StageView>>()
    {
        vm.set_vec(stage_views);
    }

    // Update the JobRow if present.
    let model = app.get_jobs();
    let mut found_selected = false;
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.id == id {
                row.title = snap.title.clone().into();
                row.stage_name = snap.stage.name().into();
                row.stage_label = snap.stage.label().into();
                row.stage_progress = snap.stage_progress as i32;
                row.total_progress = snap.total_progress as i32;
                row.indeterminate = snap.stage == Stage::Transcribe;
                row.elapsed_secs = snap.elapsed_secs as i32;
                row.elapsed = fmt_elapsed(snap.elapsed_secs).into();
                row.has_error = snap.error.is_some();
                row.error_text = snap.error.clone().unwrap_or_default().into();
                row.status_label = snap.status.label().into();
                model.set_row_data(i, row.clone());
                if row.selected {
                    found_selected = true;
                }
                break;
            }
        }
    }

    // If the updated row is selected, also refresh the detail panel.
    if found_selected {
        let caps = &snap.capabilities;
        let mut d = app.get_detail();
        d.has_job = true;
        d.title = snap.title.clone().into();
        d.bvid = snap.bvid.clone().into();
        d.page = snap.page as i32;
        d.status_label = snap.status.label().into();
        d.total_progress = snap.total_progress as i32;
        d.elapsed_secs = snap.elapsed_secs as i32;
        d.elapsed = fmt_elapsed(snap.elapsed_secs).into();
        d.error_text = snap.error.clone().unwrap_or_default().into();
        d.has_error = snap.error.is_some();
        d.warning_text = snap
            .warning
            .as_ref()
            .map(|warning| warning.label())
            .unwrap_or_default()
            .into();
        d.can_cancel = caps.can_cancel;
        d.can_retry = caps.can_retry;
        d.can_open = caps.can_open_document;
        d.can_reveal = caps.can_reveal;
        d.can_edit_speakers = caps.can_edit_speakers;
        d.retention_label = snap.retention_label.clone().into();
        app.set_detail(d);
    }
}

/// Convert persisted stage semantics into the one UI representation used by
/// initial selection and background snapshot updates.
pub fn stage_views_from_snapshot(snap: &crate::jobs::JobViewSnapshot) -> Vec<crate::StageView> {
    snap.stages
        .iter()
        .map(|s| {
            let is_current = s.name == snap.stage.name();
            crate::StageView {
                name: s.name.clone().into(),
                label: s.label.clone().into(),
                done: s.state == crate::jobs::StageState::Completed,
                skipped: s.state == crate::jobs::StageState::Skipped,
                active: is_current && snap.status == crate::jobs::JobStatus::Running,
                progress: if is_current {
                    snap.stage_progress as i32
                } else {
                    0
                },
                indeterminate: is_current
                    && snap.status == crate::jobs::JobStatus::Running
                    && snap.stage == Stage::Transcribe,
                error: is_current && snap.status == crate::jobs::JobStatus::Failed,
            }
        })
        .collect()
}

/// Update a job row's stage fields (called from background thread).
fn set_job_stage(app: &Weak<App>, job_id: Uuid, name: &str, label: &str, indeterminate: bool) {
    let id = job_id.to_string();
    let name = name.to_string();
    let label = label.to_string();
    let app = app.clone();
    app.upgrade_in_event_loop(move |app| {
        update_row(&app, &id, |row| {
            row.stage_name = name.clone().into();
            row.stage_label = label.clone().into();
            row.indeterminate = indeterminate;
            row.status_label = label.clone().into();
            row.stage_progress = 0;
            row.total_progress = recompute_total(row.stage_name.as_str(), row.stage_progress);
        });
    })
    .ok();
}

fn set_job_progress(app: &Weak<App>, job_id: Uuid, pct: u8) {
    let id = job_id.to_string();
    let app = app.clone();
    app.upgrade_in_event_loop(move |app| {
        update_row(&app, &id, |row| {
            row.stage_progress = pct as i32;
            row.total_progress = recompute_total(row.stage_name.as_str(), row.stage_progress);
        });
    })
    .ok();
}

/// Mark a job row as failed with an error message (called from the worker thread).
fn set_job_error(app: &Weak<App>, job_id: Uuid, err: &str) {
    let id = job_id.to_string();
    let err = err.to_string();
    let app = app.clone();
    app.upgrade_in_event_loop(move |app| {
        update_row(&app, &id, |row| {
            row.has_error = true;
            row.error_text = err.clone().into();
        });
    })
    .ok();
}

/// Mutate a single JobRow identified by its stringified id.
fn update_row<F: FnOnce(&mut crate::JobRow)>(app: &crate::App, id: &str, f: F) {
    let model = app.get_jobs();
    let len = model.row_count();
    for i in 0..len {
        if let Some(mut row) = model.row_data(i) {
            if row.id == id {
                f(&mut row);
                model.set_row_data(i, row.clone());
                break;
            }
        }
    }
}

pub fn fmt_elapsed(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

fn recompute_total(stage_name: &str, stage_progress: i32) -> i32 {
    Stage::from_name(stage_name)
        .map(|stage| crate::jobs::total_progress_for(stage, stage_progress.clamp(0, 100) as u8))
        .unwrap_or(0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_directory_name_is_readable_and_stable() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let mut job = crate::jobs::Job::new(id, "BV1GJ411x7h7".into(), 3);
        job.created_at = Some("2026-08-11T06:35:42Z".parse().unwrap());

        assert_eq!(
            job_directory_name(&job, false),
            "20260811-063542Z_BV1GJ411x7h7_P3_550e8400"
        );
    }

    #[test]
    fn job_directory_name_sanitizes_path_separators() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let mut job = crate::jobs::Job::new(id, "b23:../abc/def".into(), 1);
        job.created_at = Some("2026-08-11T06:35:42Z".parse().unwrap());

        let name = job_directory_name(&job, false);
        assert_eq!(name, "20260811-063542Z_b23----abc-def_P1_550e8400");
        assert!(!name.contains('/'));
    }

    #[test]
    fn duplicate_video_jobs_have_distinct_directory_names() {
        let mut first = crate::jobs::Job::new(
            Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            "BV1GJ411x7h7".into(),
            1,
        );
        let mut second = crate::jobs::Job::new(
            Uuid::parse_str("6ba7b810-9dad-41d1-80b4-00c04fd430c8").unwrap(),
            "BV1GJ411x7h7".into(),
            1,
        );
        let created = Some("2026-08-11T06:35:42Z".parse().unwrap());
        first.created_at = created;
        second.created_at = created;

        assert_ne!(
            job_directory_name(&first, false),
            job_directory_name(&second, false)
        );
    }

    #[test]
    fn ensure_work_dir_preserves_an_existing_job_path() {
        let root = std::env::temp_dir().join(format!("bimyscribe-readable-job-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let legacy = root.join("550e8400-e29b-41d4-a716-446655440000");
        let mut job = crate::jobs::Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        job.work_dir = Some(legacy.clone());
        let mut cfg = crate::config::Config::default();
        cfg.working_dir = root.join("new-location");

        assert_eq!(ensure_work_dir(&mut job, &cfg).unwrap(), legacy);
        assert!(legacy.join("logs").is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolved_short_link_applies_redirect_page() {
        let mut job = crate::jobs::Job::new(Uuid::new_v4(), "b23:abc123".into(), 1);
        let bvid =
            apply_resolved_short_link(&mut job, "https://www.bilibili.com/video/BV1GJ411x7h7?p=3");

        assert_eq!(bvid, "BV1GJ411x7h7");
        assert_eq!(job.page, 3);
    }

    #[test]
    fn resolved_short_link_without_page_keeps_requested_page() {
        let mut job = crate::jobs::Job::new(Uuid::new_v4(), "b23:abc123".into(), 2);
        let bvid =
            apply_resolved_short_link(&mut job, "https://www.bilibili.com/video/BV1GJ411x7h7");

        assert_eq!(bvid, "BV1GJ411x7h7");
        assert_eq!(job.page, 2);
    }

    #[test]
    fn drive_mounted_detects_unmounted() {
        // A clearly non-existent drive root is not mounted.
        assert!(!drive_mounted(std::path::Path::new(
            "/Volumes/definitely-not-mounted-xyz123/jobs"
        )));
    }

    #[test]
    fn drive_mounted_tmp_dir_is_mounted() {
        // A path on the system disk is considered mounted (exists).
        let tmp = std::env::temp_dir();
        assert!(drive_mounted(&tmp));
    }

    #[test]
    fn disabled_screenshot_stage_is_persisted_as_skipped() {
        let mut job = crate::jobs::Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        skip_stage(
            &mut job,
            Stage::Screenshots,
            &Weak::<App>::default(),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            job.stage_state(Stage::Screenshots),
            crate::jobs::StageState::Skipped
        );
    }

    #[test]
    fn run_stage_preserves_explicit_skipped_result() {
        let mut job = crate::jobs::Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        let ran = run_stage(
            &mut job,
            Stage::ReadableDocument,
            &Weak::<App>::default(),
            &CancellationToken::new(),
            false,
            |job, _| {
                job.set_stage_state(Stage::ReadableDocument, crate::jobs::StageState::Skipped);
                Ok(())
            },
        )
        .unwrap();
        assert!(ran);
        assert_eq!(
            job.stage_state(Stage::ReadableDocument),
            crate::jobs::StageState::Skipped
        );
    }

    #[test]
    fn live_progress_uses_the_jobs_progress_algorithm() {
        for stage in stage_sequence() {
            for progress in [0, 50, 99, 100] {
                assert_eq!(
                    recompute_total(stage.name(), progress),
                    crate::jobs::total_progress_for(stage, progress as u8) as i32
                );
            }
        }
        assert_eq!(recompute_total(Stage::Completed.name(), 0), 100);
    }

    #[test]
    fn completed_job_has_no_pending_structural_stage() {
        let mut job = crate::jobs::Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        job.set_stage_state(Stage::ReadableDocument, crate::jobs::StageState::Skipped);
        mark_job_completed(&mut job);
        assert_eq!(job.status, crate::jobs::JobStatus::Completed);
        assert_eq!(job.total_progress(), 100);
        assert!(crate::jobs::pipeline_stages().into_iter().all(|stage| {
            matches!(
                job.stage_state(stage),
                crate::jobs::StageState::Completed | crate::jobs::StageState::Skipped
            )
        }));
        assert_eq!(
            job.stage_state(Stage::Screenshots),
            crate::jobs::StageState::Skipped
        );
    }

    #[test]
    #[ignore] // requires a matching external volume to be mounted
    fn drive_mounted_external_drive() {
        assert!(drive_mounted(std::path::Path::new(
            "/Volumes/ExternalDisk/BiMyScribe/Jobs"
        )));
    }
}
