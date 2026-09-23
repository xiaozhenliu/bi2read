//! Durable transcription pipeline.
//!
//! [`run_job`] is the external interface. It owns stage ordering, artifact
//! validation, recovery semantics, persistence, cancellation, and optional
//! enhancement fallback. Slint mapping lives in `ui_bridge`.

use slint::Weak;

use crate::cancel::CancellationToken;
use crate::content_results::{ContentSnapshotV1, ContentViewV1, Intent};
use crate::jobs::{pipeline_stages, Stage};
use crate::ui_bridge::{set_job_error, set_job_progress, set_job_stage};
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
    #[error("{0}: task must be rebuilt with explicit transcription settings")]
    TranscriptionSelectionRequired(&'static str),
    #[error("runtime-identity-changed: 任务冻结的 Runtime 已改变，请用当前 Runtime 重建任务")]
    RuntimeIdentityChanged,
    #[error("external drive not mounted")]
    DriveNotMounted,
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

fn content_pipeline_error(error: crate::content_results::ContentError) -> PipelineError {
    match error {
        crate::content_results::ContentError::Cancelled => PipelineError::Cancelled,
        other => PipelineError::Document(other.to_string()),
    }
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

/// All external side effects of the pipeline (metadata fetch, audio download,
/// transcription). Production uses [`ProdDeps`]; integration tests provide
/// fixture implementations.
pub trait PipelineDeps {
    fn fetch_metadata(
        &self,
        bvid: &str,
        page: Option<u32>,
    ) -> Result<crate::bilibili::Metadata, PipelineError>;
    fn fetch_audio(
        &self,
        bvid: &str,
        cid: u64,
        dest: &std::path::Path,
        progress: &dyn Fn(u64, u64),
        cancel_token: &CancellationToken,
    ) -> Result<(), PipelineError>;
    fn transcribe(
        &self,
        request: crate::funasr::TranscribeRequest<'_>,
    ) -> Result<crate::funasr::TranscribeOutcome, crate::funasr::FunasrError>;
}

/// Production implementation talking to the real bilibili API and funasr runtime.
pub struct ProdDeps;

impl PipelineDeps for ProdDeps {
    fn fetch_metadata(
        &self,
        bvid: &str,
        page: Option<u32>,
    ) -> Result<crate::bilibili::Metadata, PipelineError> {
        Ok(crate::bilibili::fetch_metadata(bvid, page)?)
    }

    fn fetch_audio(
        &self,
        bvid: &str,
        cid: u64,
        dest: &std::path::Path,
        progress: &dyn Fn(u64, u64),
        cancel_token: &CancellationToken,
    ) -> Result<(), PipelineError> {
        // Fetch the audio stream URL (retry once if it 403s on download).
        let mut last_err: Option<PipelineError> = None;
        for _attempt in 0..2 {
            // Check cancellation before each download attempt.
            cancel_token.check()?;
            let url = match crate::bilibili::fetch_playurl_audio(bvid, cid) {
                Ok(u) => u,
                Err(e) => {
                    let terminal = matches!(&e, &crate::bilibili::MetadataError::NotAccessible);
                    last_err = Some(e.into());
                    if terminal {
                        break;
                    }
                    continue;
                }
            };
            match crate::bilibili::download_audio(&url, dest, None, progress) {
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
        Ok(())
    }

    fn transcribe(
        &self,
        request: crate::funasr::TranscribeRequest<'_>,
    ) -> Result<crate::funasr::TranscribeOutcome, crate::funasr::FunasrError> {
        crate::funasr::run(request)
    }
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
    run_job_with_deps(job, cfg, app, cancel_token, &ProdDeps)
}

/// Dependency-injected entry point. `run_job` forwards here with [`ProdDeps`];
/// integration tests inject fixture dependencies instead. No extra wrapper is
/// provided: this is the only seam for full-pipeline regression tests.
pub fn run_job_with_deps(
    job: &mut crate::jobs::Job,
    cfg: &crate::config::Config,
    app: &Weak<App>,
    cancel_token: &CancellationToken,
    deps: &dyn PipelineDeps,
) -> Result<(), PipelineError> {
    // Check cancellation before starting.
    cancel_token.check()?;

    // A pre-v0.4 Job has no trustworthy Runtime or language identity. Reject
    // it before resolving the current Config or creating any work directory;
    // recovery and retry must require an explicit new Job selection.
    if job.requires_transcription_rebuild() {
        job.status = crate::jobs::JobStatus::NeedsUserAction;
        job.stage = Stage::NeedsUserAction;
        job.stage_progress = 0;
        job.error = Some(format!(
            "{}: task must be rebuilt with explicit transcription settings",
            crate::jobs::LEGACY_UNRECORDED
        ));
        crate::jobs::save_job_state(job)?;
        return Err(PipelineError::TranscriptionSelectionRequired(
            crate::jobs::LEGACY_UNRECORDED,
        ));
    }

    // Resolve the work directory for this job (on the external drive).
    // `ensure_work_dir` creates the dir and sets `job.work_dir` as a side effect.
    let _ = ensure_work_dir(job, cfg)?;

    let cleanup_was_complete =
        job.stage_state(Stage::Cleanup) == crate::jobs::StageState::Completed;
    let mut rebuilt = false;

    let metadata_rebuilt = run_stage(
        job,
        Stage::Metadata,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_metadata(job, app, cfg, deps),
    )?;
    rebuilt |= metadata_rebuilt;
    rebuilt |= run_stage(
        job,
        Stage::DownloadAudio,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_download_audio(job, app, cfg, cancel_token, deps),
    )?;
    rebuilt |= run_stage(
        job,
        Stage::NormalizeAudio,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_normalize_audio(job, app, cfg, cancel_token),
    )?;
    let transcribe_rebuilt = run_stage(
        job,
        Stage::Transcribe,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_transcribe(job, app, cfg, cancel_token, deps),
    )?;
    rebuilt |= transcribe_rebuilt;
    let raw_document_rebuilt = run_stage(
        job,
        Stage::RawDocument,
        app,
        cancel_token,
        cleanup_was_complete,
        stage_raw_document,
    )?;
    rebuilt |= raw_document_rebuilt;
    if job.content_setup.is_some() && (metadata_rebuilt || transcribe_rebuilt) {
        // Metadata changes alter source context and transcription changes
        // Evidence. Force Initial before Final so an old completed readable
        // stage cannot let Final consume a mismatched current snapshot.
        job.set_stage_state(Stage::ReadableDocument, crate::jobs::StageState::Pending);
    }
    let readable_rebuilt = run_stage(
        job,
        Stage::ReadableDocument,
        app,
        cancel_token,
        cleanup_was_complete,
        |job, app| stage_readable_document(job, app, cfg, cancel_token),
    )?;
    rebuilt |= readable_rebuilt;
    if job.content_setup.is_some() && (metadata_rebuilt || raw_document_rebuilt || readable_rebuilt)
    {
        // A new current or Evidence-derived readable output invalidates any
        // previous final Presentation. Keep the final stage explicit so a
        // retry cannot accidentally open an older full.md.
        job.set_stage_state(Stage::FinalDocument, crate::jobs::StageState::Pending);
    }
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
        Err(PipelineError::Cancelled) => {
            // Cancellation is not a failed stage. Leave the in-memory stage
            // as Running for Scheduler's Cancelled convergence; importantly,
            // do not persist Failed/error or surface a false failure banner.
            Err(PipelineError::Cancelled)
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
    deps: &dyn PipelineDeps,
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

    let meta = deps.fetch_metadata(&bvid, Some(job.page))?;
    // The metadata API resolves legacy av ids to the canonical BVID.
    job.bvid = meta.bvid.clone();
    job.cid = Some(meta.cid);
    job.up_name = Some(meta.up_name.clone());
    job.part_title = meta.part_title.clone();
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
        "part_title": meta.part_title,
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
    deps: &dyn PipelineDeps,
) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let bvid = job.bvid.clone();
    let cid = job
        .cid
        .ok_or_else(|| PipelineError::Bilibili("no cid".into()))?;

    let dest = dir.join("source.audio");
    deps.fetch_audio(
        &bvid,
        cid,
        &dest,
        &|bytes, total| {
            let pct = if total > 0 {
                ((bytes as f64 / total as f64) * 100.0) as u8
            } else {
                0
            };
            set_job_progress(app, job.id, pct.min(99));
        },
        cancel_token,
    )?;
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
    let output_size = std::fs::metadata(&dest)
        .map_err(|error| PipelineError::Ffmpeg(format!("normalized output: {error}")))?
        .len();
    if output_size == 0 {
        return Err(PipelineError::Ffmpeg(
            "ffmpeg produced an empty normalized output".into(),
        ));
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
    ])
    .log(&log);
    let probe_code =
        crate::process::run(probe).map_err(|error| PipelineError::Ffmpeg(error.to_string()))?;
    if probe_code != 0 {
        return Err(PipelineError::Ffmpeg(format!(
            "ffprobe rejected normalized output (exit {})",
            probe_code
        )));
    }
    let _ = cfg; // future: configurable sample rate
    set_job_progress(app, job.id, 100);
    Ok(())
}

fn stage_transcribe(
    job: &mut crate::jobs::Job,
    app: &Weak<App>,
    cfg: &crate::config::Config,
    cancel_token: &CancellationToken,
    deps: &dyn PipelineDeps,
) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let normalized = dir.join("normalized.wav");
    let log = dir.join("logs").join("funasr.log");
    let selection = job.transcription_selection.as_ref().ok_or(
        PipelineError::TranscriptionSelectionRequired(crate::jobs::LEGACY_UNRECORDED),
    )?;
    job.transcription_result = None;

    let outcome = match deps.transcribe(crate::funasr::TranscribeRequest {
        job_dir: &dir,
        normalized_wav: &normalized,
        selection,
        duration_ms: job.duration_ms,
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
        Err(crate::funasr::FunasrError::IdentityChanged) => {
            // 冻结的 Runtime 身份已变：重试永远失配，只能用当前 Runtime 重建。
            // 进入等待用户操作并持久化错误码，让 JobCapabilities 开放“重建”。
            let error = PipelineError::RuntimeIdentityChanged;
            job.stage = Stage::NeedsUserAction;
            job.status = crate::jobs::JobStatus::NeedsUserAction;
            job.error = Some(error.to_string());
            set_job_stage(
                app,
                job.id,
                Stage::NeedsUserAction.name(),
                Stage::NeedsUserAction.label(),
                false,
            );
            set_job_error(app, job.id, &error.to_string());
            crate::jobs::save_job_state(job)?;
            return Err(error);
        }
        Err(error) => return Err(error.into()),
    };
    job.transcription_result = Some(outcome.result);
    // Store utterances count for display; the raw json is already written by funasr::run.
    let _ = outcome.utterances.len();
    set_job_progress(app, job.id, 100);
    Ok(())
}

fn stage_raw_document(job: &mut crate::jobs::Job, app: &Weak<App>) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    if job.content_setup.is_some() {
        let evidence = match crate::content_results::current(job)
            .map_err(|error| PipelineError::Document(error.to_string()))?
        {
            ContentViewV1::Current(snapshot) => snapshot.evidence.utterances,
            ContentViewV1::RawOnly { evidence, .. } => evidence.utterances,
            ContentViewV1::Legacy => {
                return Err(PipelineError::Document(
                    "v0.5 Job unexpectedly resolved as legacy".into(),
                ))
            }
        };
        let md = crate::document::render_raw_markdown(job, &evidence);
        let path = dir.join("transcript.raw.md");
        crate::jobs::atomic_write(&path, md.as_bytes())?;
        set_job_progress(app, job.id, 100);
        return Ok(());
    }
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
    cancel_token: &CancellationToken,
) -> Result<(), PipelineError> {
    let dir = job
        .work_dir
        .clone()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    if job.content_setup.is_some() {
        let _snapshot = crate::content_results::execute(job, Intent::Initial, cancel_token)
            .map_err(content_pipeline_error)?;
        set_job_progress(app, job.id, 100);
        return Ok(());
    }
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
    if job.content_setup.is_some() {
        let out_dir = final_output_dir(job, cfg, &dir);
        std::fs::create_dir_all(&out_dir)?;
        job.final_output_dir = Some(out_dir);
        let snapshot = current_snapshot(job)?;
        crate::document::rebuild_presentation(job, &snapshot)
            .map_err(|error| PipelineError::Document(error.to_string()))?;
        set_job_progress(app, job.id, 100);
        return Ok(());
    }
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
    let out_dir = final_output_dir(job, cfg, &dir);
    std::fs::create_dir_all(&out_dir)?;
    let full_path = out_dir.join("full.md");
    crate::jobs::atomic_write(&full_path, md.as_bytes())?;

    // Store the final output dir so Speaker edits can rebuild it without
    // inferring from the current config.
    job.final_output_dir = Some(out_dir.clone());
    if out_dir != dir {
        // Legacy jobs retain the historical work-dir mirror for compatibility.
        let _ = std::fs::copy(&full_path, dir.join("full.md"));
    }
    set_job_progress(app, job.id, 100);
    Ok(())
}

pub(crate) fn ensure_final_output_dir(
    job: &mut crate::jobs::Job,
    cfg: &crate::config::Config,
) -> Result<std::path::PathBuf, PipelineError> {
    if job.content_setup.is_none() {
        return Err(PipelineError::Document(
            "legacy Job 使用旧的最终文档路径".into(),
        ));
    }
    let work_dir = job
        .work_dir
        .as_deref()
        .ok_or_else(|| PipelineError::Io("no work dir".into()))?;
    let output_dir = final_output_dir(job, cfg, work_dir);
    std::fs::create_dir_all(&output_dir)?;
    job.final_output_dir = Some(output_dir.clone());
    Ok(output_dir)
}

fn final_output_dir(
    job: &crate::jobs::Job,
    cfg: &crate::config::Config,
    work_dir: &std::path::Path,
) -> std::path::PathBuf {
    if let Some(existing) = &job.final_output_dir {
        return existing.clone();
    }
    if cfg.output_dir.as_os_str().is_empty() {
        return work_dir.to_path_buf();
    }
    let safe_title = sanitize(&job.title);
    let name = format!("{} [{}-P{}]", safe_title, job.bvid, job.page);
    cfg.output_dir.join(name)
}

fn current_snapshot(job: &crate::jobs::Job) -> Result<ContentSnapshotV1, PipelineError> {
    match crate::content_results::current(job)
        .map_err(|error| PipelineError::Document(error.to_string()))?
    {
        ContentViewV1::Current(snapshot) => Ok(snapshot),
        ContentViewV1::RawOnly { .. } => Err(PipelineError::Document(
            "可信文字结果尚未生成，请修复文字结果".into(),
        )),
        ContentViewV1::Legacy => Err(PipelineError::Document(
            "v0.5 Job unexpectedly resolved as legacy".into(),
        )),
    }
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
            // v0.5 content jobs retain machine-readable Evidence, current and
            // metadata so the result page can reopen, map sources and
            // regenerate an enhancement without rerunning ASR. Legacy jobs
            // keep the historical documents-only behavior.
            let files = if job.content_setup.is_some() {
                ["source.audio", "source.video", "normalized.wav"].as_slice()
            } else {
                [
                    "source.audio",
                    "source.video",
                    "normalized.wav",
                    "transcript.raw.json",
                    "transcript.readable.md",
                    "metadata.json",
                ]
                .as_slice()
            };
            for f in files {
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

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

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
        let root = std::env::temp_dir().join(format!("bi2read-readable-job-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let legacy = root.join("550e8400-e29b-41d4-a716-446655440000");
        let mut job = crate::jobs::Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        job.work_dir = Some(legacy.clone());
        let cfg = crate::config::Config {
            working_dir: root.join("new-location"),
            ..Default::default()
        };

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
    fn run_job_rejects_legacy_selection_before_using_config() {
        let mut job = crate::jobs::Job::new(Uuid::new_v4(), "BV1legacy".into(), 1);
        let result = run_job(
            &mut job,
            &crate::config::Config::default(),
            &Weak::<App>::default(),
            &CancellationToken::new(),
        );

        assert!(matches!(
            result,
            Err(PipelineError::TranscriptionSelectionRequired(
                crate::jobs::LEGACY_UNRECORDED
            ))
        ));
        assert_eq!(job.status, crate::jobs::JobStatus::NeedsUserAction);
        assert_eq!(job.stage, Stage::NeedsUserAction);
        assert!(job.work_dir.is_none());
    }

    #[test]
    fn content_cancellation_is_a_pipeline_cancellation() {
        assert!(matches!(
            content_pipeline_error(crate::content_results::ContentError::Cancelled),
            PipelineError::Cancelled
        ));
    }

    #[test]
    fn readable_content_cancellation_does_not_publish_or_mark_failed() {
        let root = std::env::temp_dir().join(format!("bi2read-readable-cancel-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut job = crate::jobs::Job::new(Uuid::new_v4(), "BV1cancel".into(), 1);
        job.content_setup = Some(crate::content_results::ContentSetupV1::disabled());
        job.work_dir = Some(root.clone());
        job.cid = Some(7);
        job.duration_ms = Some(1000);
        job.status = crate::jobs::JobStatus::Running;
        job.stage = Stage::ReadableDocument;
        std::fs::write(
            root.join("transcript.raw.json"),
            br#"[{"id":"u1","text":"raw","start_ms":0,"end_ms":1,"speaker_id":0}]"#,
        )
        .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = stage_readable_document(
            &mut job,
            &Weak::<App>::default(),
            &crate::config::Config::default(),
            &cancel,
        );
        assert!(matches!(result, Err(PipelineError::Cancelled)));
        assert!(job.error.is_none());
        assert_ne!(job.status, crate::jobs::JobStatus::Failed);
        assert!(!root.join("content-current.v1.json").exists());
        job.set_stage_state(Stage::ReadableDocument, crate::jobs::StageState::Pending);
        let stage_cancel = CancellationToken::new();
        let stage_result = run_stage(
            &mut job,
            Stage::ReadableDocument,
            &Weak::<App>::default(),
            &stage_cancel,
            false,
            |_job, _app| Err(PipelineError::Cancelled),
        );
        assert!(matches!(stage_result, Err(PipelineError::Cancelled)));
        assert_eq!(job.status, crate::jobs::JobStatus::Running);
        assert_eq!(
            job.stage_state(Stage::ReadableDocument),
            crate::jobs::StageState::Running
        );
        assert!(job.error.is_none());
        std::fs::remove_dir_all(root).ok();
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
            "/Volumes/ExternalDisk/bi2read/Jobs"
        )));
    }
}
