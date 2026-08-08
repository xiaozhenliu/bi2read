//! Task state, stages and the in-memory + persisted queue.
//!
//! Implements the stage enum, per-stage completion state
//! (`StageState`), and atomic file-backed persistence (`queue.json` on the
//! system disk + `state.json` per job on the external drive). State writes use
//! `.tmp` + atomic rename so a crash never leaves a half-written
//! JSON file. Startup recovery resets in-flight stages and re-validates each
//! stage's required artifacts against the documented minimum-artifact table.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// All pipeline stages plus the non-linear waiting and terminal states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Queued,
    Metadata,
    DownloadAudio,
    NormalizeAudio,
    Transcribe,
    RawDocument,
    ReadableDocument,
    Screenshots,
    FinalDocument,
    Cleanup,
    Completed,
    // Non-linear statuses (not part of the happy path):
    Paused,
    NeedsUserAction,
    WaitingForDrive,
    Failed,
    Cancelled,
}

impl Stage {
    pub fn name(&self) -> &'static str {
        match self {
            Stage::Queued => "queued",
            Stage::Metadata => "metadata",
            Stage::DownloadAudio => "download_audio",
            Stage::NormalizeAudio => "normalize_audio",
            Stage::Transcribe => "transcribe",
            Stage::RawDocument => "raw_document",
            Stage::ReadableDocument => "readable_document",
            Stage::Screenshots => "screenshots",
            Stage::FinalDocument => "final_document",
            Stage::Cleanup => "cleanup",
            Stage::Completed => "completed",
            Stage::Paused => "paused",
            Stage::NeedsUserAction => "needs_user_action",
            Stage::WaitingForDrive => "waiting_for_drive",
            Stage::Failed => "failed",
            Stage::Cancelled => "cancelled",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Stage::Queued => "排队",
            Stage::Metadata => "获取元数据",
            Stage::DownloadAudio => "下载音频",
            Stage::NormalizeAudio => "归一化音频",
            Stage::Transcribe => "转写",
            Stage::RawDocument => "原始文稿",
            Stage::ReadableDocument => "可读文稿",
            Stage::Screenshots => "截图",
            Stage::FinalDocument => "最终文档",
            Stage::Cleanup => "清理",
            Stage::Completed => "已完成",
            Stage::Paused => "已暂停",
            Stage::NeedsUserAction => "等待用户操作",
            Stage::WaitingForDrive => "等待外置盘",
            Stage::Failed => "失败",
            Stage::Cancelled => "已取消",
        }
    }

    /// Parse a stage name back to the enum; used by `state.json` recovery.
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Stage::Queued,
            "metadata" => Stage::Metadata,
            "download_audio" => Stage::DownloadAudio,
            "normalize_audio" => Stage::NormalizeAudio,
            "transcribe" => Stage::Transcribe,
            "raw_document" => Stage::RawDocument,
            "readable_document" => Stage::ReadableDocument,
            "screenshots" => Stage::Screenshots,
            "final_document" => Stage::FinalDocument,
            "cleanup" => Stage::Cleanup,
            "completed" => Stage::Completed,
            "paused" => Stage::Paused,
            "needs_user_action" => Stage::NeedsUserAction,
            "waiting_for_drive" => Stage::WaitingForDrive,
            "failed" => Stage::Failed,
            "cancelled" => Stage::Cancelled,
            _ => return None,
        })
    }
}

/// High-level job status (separate from the fine-grained stage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Paused,
    NeedsUserAction,
    WaitingForDrive,
    Failed,
    Cancelling,
    Cancelled,
    Completed,
}

impl JobStatus {
    pub fn label(&self) -> &'static str {
        match self {
            JobStatus::Queued => "排队中",
            JobStatus::Running => "处理中",
            JobStatus::Paused => "已暂停",
            JobStatus::NeedsUserAction => "等待用户操作",
            JobStatus::WaitingForDrive => "等待外置盘",
            JobStatus::Failed => "失败",
            JobStatus::Cancelling => "取消中",
            JobStatus::Cancelled => "已取消",
            JobStatus::Completed => "已完成",
        }
    }
}

/// Per-stage completion state, persisted in `state.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageState {
    /// Not yet started.
    Pending,
    /// In progress at crash time; recovery resets this to Pending.
    Running,
    /// Finished and its artifact validated.
    Completed,
    /// Explicitly skipped (e.g. LLM off, screenshots off).
    Skipped,
    /// Failed; can be retried.
    Failed,
}

impl StageState {
    pub fn label(&self) -> &'static str {
        match self {
            StageState::Pending => "待处理",
            StageState::Running => "处理中",
            StageState::Completed => "已完成",
            StageState::Skipped => "已跳过",
            StageState::Failed => "失败",
        }
    }
}

/// Retention policy applied to job artifacts after the final document is
/// produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionPolicy {
    /// Delete audio/video/temp, keep metadata/raw/readable/speaker-map/logs.
    #[default]
    Recommended,
    /// Keep everything.
    KeepAll,
    /// Delete all audio + raw data, keep only .md documents.
    DocumentsOnly,
}

impl RetentionPolicy {
    pub fn label(&self) -> &'static str {
        match self {
            RetentionPolicy::Recommended => "推荐",
            RetentionPolicy::KeepAll => "保留全部",
            RetentionPolicy::DocumentsOnly => "仅保留文档",
        }
    }

    /// Parse a Chinese label back to the enum (used by `Config::apply_view`).
    pub fn from_label(s: &str) -> Self {
        match s {
            "保留全部" => RetentionPolicy::KeepAll,
            "仅保留文档" => RetentionPolicy::DocumentsOnly,
            _ => RetentionPolicy::Recommended,
        }
    }
}

/// Non-fatal warning that does not block job completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobWarning {
    /// LLM refine failed; the document uses raw fallback text.
    LlmFallback { message: String },
    /// An optional stage was skipped (e.g. screenshots disabled).
    OptionalStageSkipped { stage: String, reason: String },
}

impl JobWarning {
    /// A human-readable label for the UI warning banner.
    pub fn label(&self) -> String {
        match self {
            JobWarning::LlmFallback { message } => {
                format!(
                    "文档已生成，但 LLM 可读化失败，当前使用原始转写。({})",
                    message
                )
            }
            JobWarning::OptionalStageSkipped { stage, reason } => {
                format!("可选阶段 {} 已跳过：{}", stage, reason)
            }
        }
    }
}

/// What actions are available for a job in its current state.
#[derive(Debug, Clone)]
pub struct JobCapabilities {
    pub can_cancel: bool,
    pub can_retry: bool,
    pub can_open_document: bool,
    pub can_reveal: bool,
    pub can_edit_speakers: bool,
}

impl JobCapabilities {
    pub fn from_job(job: &Job) -> Self {
        let is_terminal = matches!(
            job.status,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
        );
        let is_done = job.status == JobStatus::Completed;
        let has_transcript = job
            .work_dir
            .as_ref()
            .map(|d| d.join("transcript.raw.json").exists())
            .unwrap_or(false);
        Self {
            can_cancel: !is_terminal,
            can_retry: job.status == JobStatus::Failed || job.status == JobStatus::NeedsUserAction,
            can_open_document: is_done,
            can_reveal: job.work_dir.is_some(),
            can_edit_speakers: has_transcript,
        }
    }
}

/// One stage's snapshot for the UI, reading the persisted `StageState` directly
/// instead of inferring from position.
#[derive(Debug, Clone)]
pub struct StageSnapshot {
    pub name: String,
    pub label: String,
    pub state: StageState,
}

/// A complete snapshot of a job for UI rendering. `Job` is the single source of
/// truth; the pipeline builds this snapshot and applies it in one UI event.
#[derive(Debug, Clone)]
pub struct JobViewSnapshot {
    pub id: Uuid,
    pub title: String,
    pub bvid: String,
    pub page: u32,
    pub status: JobStatus,
    pub stage: Stage,
    pub stage_progress: u8,
    pub total_progress: u8,
    pub stages: Vec<StageSnapshot>,
    pub elapsed_secs: u64,
    pub error: Option<String>,
    pub warning: Option<JobWarning>,
    pub capabilities: JobCapabilities,
    pub retention_label: String,
}

impl JobViewSnapshot {
    /// Build a snapshot from the job's current state and the wall-clock `now`.
    /// Elapsed is computed from `started_at`/`finished_at`.
    pub fn from_job(job: &Job, now: chrono::DateTime<chrono::Utc>) -> Self {
        let elapsed_secs = match (job.started_at, job.finished_at) {
            (Some(start), Some(end)) => (end - start).num_seconds().max(0) as u64,
            (Some(start), None) => (now - start).num_seconds().max(0) as u64,
            _ => 0,
        };
        let stages: Vec<StageSnapshot> = pipeline_stages()
            .into_iter()
            .map(|s| StageSnapshot {
                name: s.name().to_string(),
                label: s.label().to_string(),
                state: job.stage_state(s),
            })
            .collect();
        Self {
            id: job.id,
            title: job.title.clone(),
            bvid: job.bvid.clone(),
            page: job.page,
            status: job.status,
            stage: job.stage,
            stage_progress: job.stage_progress,
            total_progress: job.total_progress(),
            stages,
            elapsed_secs,
            error: job.error.clone(),
            warning: job.warning.clone(),
            capabilities: JobCapabilities::from_job(job),
            retention_label: job.retention.label().to_string(),
        }
    }
}

/// A single job in the queue. Serialized into `queue.json` (summary) and the
/// per-job `state.json` on the external drive (full state).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub bvid: String,
    pub page: u32,
    pub title: String,
    pub stage: Stage,
    pub status: JobStatus,
    /// 0..100 progress within the current stage.
    pub stage_progress: u8,
    /// Last error message, if the stage failed.
    pub error: Option<String>,
    /// Working directory on the external drive (`<working_dir>/<job-id>`).
    pub work_dir: Option<PathBuf>,
    /// CID resolved during the `metadata` stage.
    pub cid: Option<u64>,
    /// UP主 name, filled at metadata time.
    pub up_name: Option<String>,
    /// Duration in ms, filled at metadata time.
    pub duration_ms: Option<u64>,
    /// Per-stage completion state. Absent entry == Pending.
    #[serde(default)]
    pub stages: BTreeMap<String, StageState>,
    /// speaker_id -> display name. Absent entry uses the raw "Speaker N" label.
    #[serde(default)]
    pub speaker_map: BTreeMap<u32, String>,
    /// Original URL the user pasted (kept for the final document link).
    #[serde(default)]
    pub source_url: Option<String>,
    /// When the job was created.
    #[serde(default)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the job started running.
    #[serde(default)]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the job reached a terminal state.
    #[serde(default)]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Non-fatal warning (e.g. LLM fallback); does not block completion.
    #[serde(default)]
    pub warning: Option<JobWarning>,
    /// Per-job retention policy, copied from config at creation.
    #[serde(default)]
    pub retention: RetentionPolicy,
    /// Final output directory where `full.md` was written.
    #[serde(default)]
    pub final_output_dir: Option<PathBuf>,
}

impl Job {
    pub fn new(id: Uuid, bvid: String, page: u32) -> Self {
        let title = bvid.clone();
        Self {
            id,
            bvid,
            page,
            title,
            stage: Stage::Queued,
            status: JobStatus::Queued,
            stage_progress: 0,
            error: None,
            work_dir: None,
            cid: None,
            up_name: None,
            duration_ms: None,
            stages: BTreeMap::new(),
            speaker_map: BTreeMap::new(),
            source_url: None,
            created_at: Some(chrono::Utc::now()),
            started_at: None,
            finished_at: None,
            warning: None,
            retention: RetentionPolicy::default(),
            final_output_dir: None,
        }
    }

    /// Overall progress across all pipeline stages (0..100).
    pub fn total_progress(&self) -> u8 {
        if self.status == JobStatus::Completed {
            return 100;
        }
        total_progress_for(self.stage, self.stage_progress)
    }

    /// Get the stage state, defaulting to Pending.
    pub fn stage_state(&self, stage: Stage) -> StageState {
        self.stages
            .get(stage.name())
            .copied()
            .unwrap_or(StageState::Pending)
    }

    /// Set a stage's state (writes through to `stages` map).
    pub fn set_stage_state(&mut self, stage: Stage, state: StageState) {
        self.stages.insert(stage.name().to_string(), state);
    }

    /// Resolve a speaker display name, falling back to the raw label.
    pub fn speaker_name(&self, speaker_id: u32) -> String {
        self.speaker_map
            .get(&speaker_id)
            .cloned()
            .unwrap_or_else(|| format!("Speaker {}", speaker_id))
    }

    /// The `state.json` path under this job's work directory.
    pub fn state_path(&self) -> Option<PathBuf> {
        self.work_dir.as_ref().map(|d| d.join("state.json"))
    }
}

/// Calculate overall progress from one accumulated numerator so integer
/// truncation happens only once. This is the single progress algorithm used by
/// persisted jobs and live UI updates.
pub fn total_progress_for(stage: Stage, stage_progress: u8) -> u8 {
    if stage == Stage::Completed {
        return 100;
    }
    let seq = pipeline_stages();
    let total = seq.len().max(1) as u32;
    let idx = seq.iter().position(|s| *s == stage).unwrap_or(0) as u32;
    ((idx * 100 + u32::from(stage_progress.min(100))) / total).min(100) as u8
}

/// The happy-path stages in order.
pub fn pipeline_stages() -> Vec<Stage> {
    vec![
        Stage::Queued,
        Stage::Metadata,
        Stage::DownloadAudio,
        Stage::NormalizeAudio,
        Stage::Transcribe,
        Stage::RawDocument,
        Stage::ReadableDocument,
        Stage::Screenshots,
        Stage::FinalDocument,
        Stage::Cleanup,
        Stage::Completed,
    ]
}

/// In-memory queue. Persisted atomically to `queue.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Queue {
    pub jobs: Vec<Job>,
}

impl Queue {
    pub fn load() -> std::io::Result<Self> {
        let path = queue_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(path)?;
        serde_json::from_str(&data).map_err(io_err)
    }

    /// Atomically persist the queue: write `queue.tmp` then rename.
    pub fn save(&self) -> std::io::Result<()> {
        let path = queue_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(self).map_err(io_err)?;
        atomic_write(&path, &data)
    }

    pub fn snapshot(&self) -> &[Job] {
        &self.jobs
    }

    /// Find a job by id.
    pub fn get(&self, id: Uuid) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    pub fn get_mut(&mut self, id: Uuid) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }
}

/// Atomically write `bytes` to `path`: write `path.tmp` then rename.
/// The rename is atomic on the same filesystem; callers must keep `.tmp` on the
/// same volume as the target (this holds for both queue.json on the system disk
/// and state.json on the external drive).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    // If a stale .tmp lingers from a prior crash, remove it first.
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn io_err(e: serde_json::Error) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Persist a job's full state to its `state.json` (atomic). No-op if the job
/// has no work directory yet (e.g. before the drive is mounted).
pub fn save_job_state(job: &Job) -> std::io::Result<()> {
    let Some(path) = job.state_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(job).map_err(io_err)?;
    atomic_write(&path, &data)
}

/// Load a single job's `state.json` from its work directory. Returns None if the
/// file does not exist (fresh job).
pub fn load_job_state(work_dir: &Path) -> Option<Job> {
    let path = work_dir.join("state.json");
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Queue file in the platform application-state directory.
pub fn queue_path() -> std::io::Result<PathBuf> {
    Ok(crate::paths::AppPaths::discover()?.queue_file())
}

/// Platform application-state directory.
pub fn config_dir() -> std::io::Result<PathBuf> {
    Ok(crate::paths::AppPaths::discover()?.application_support())
}

// ---------------------------------------------------------------------------
// Startup recovery
// ---------------------------------------------------------------------------

/// Outcome of recovering one job.
#[derive(Debug, Clone)]
pub struct RecoveryReport {
    pub job_id: Uuid,
    /// Stages that were reset from Running -> Pending (will be re-run).
    pub reset_stages: Vec<Stage>,
    /// Stages whose artifact was missing/invalid and so were reset to Pending.
    pub revalidated_stages: Vec<Stage>,
}

/// Recover the queue after a crash.
///
/// For each job:
/// 1. Delete leftover `*.tmp` artifacts in its work directory.
/// 2. Reset any `Running` stage to `Pending` and zero its progress.
/// 3. Re-validate each `Completed` stage's artifact; if missing/invalid,
///    reset it to `Pending` so it re-runs.
/// 4. A fully `Completed` job is left alone (only final doc + retained
///    artifacts checked); re-processing requires explicit user action.
pub fn recover(queue: &mut Queue) -> Vec<RecoveryReport> {
    let mut reports = Vec::new();
    for job in &mut queue.jobs {
        let mut report = RecoveryReport {
            job_id: job.id,
            reset_stages: Vec::new(),
            revalidated_stages: Vec::new(),
        };

        // Step 1: remove leftover .tmp files in the work dir.
        if let Some(dir) = &job.work_dir {
            cleanup_tmp_files(dir);
        }

        // Screenshots have never had a backend implementation. Older state
        // files incorrectly recorded the no-op as Completed; normalize that
        // durable lie to the neutral Skipped state.
        if job.stage_state(Stage::Screenshots) == StageState::Completed {
            job.set_stage_state(Stage::Screenshots, StageState::Skipped);
        }

        // Step 2: reset Running -> Pending, zero progress.
        let running: Vec<Stage> = pipeline_stages()
            .into_iter()
            .filter(|s| job.stage_state(*s) == StageState::Running)
            .collect();
        for s in &running {
            job.set_stage_state(*s, StageState::Pending);
            report.reset_stages.push(*s);
        }

        // If the job was Running overall, demote to Queued.
        if job.status == JobStatus::Running {
            job.status = JobStatus::Queued;
            job.stage = next_pending_stage(job).unwrap_or(Stage::Queued);
            job.stage_progress = 0;
        }

        // Completed jobs have already applied their retention policy. Validate
        // every artifact that policy promises to retain, while accepting files
        // that cleanup intentionally removed.
        if job.status == JobStatus::Completed {
            let invalid = invalid_retained_stages(job);
            if !invalid.is_empty() {
                let first = earliest_rebuild_stage(job, &invalid);
                reset_pipeline_from(job, first, &mut report.revalidated_stages);
                job.status = JobStatus::Queued;
                job.stage = first;
                job.stage_progress = 0;
                job.finished_at = None;
                job.error = None;
            } else {
                job.stage = Stage::Completed;
                job.stage_progress = 100;
                // Repair state files produced by the old recovery bug. Once
                // every retained artifact is trustworthy, no stage may remain
                // Pending/Running/Failed under an overall Completed status.
                // Preserve intentional Skipped states (readable, screenshots).
                for stage in pipeline_stages() {
                    if stage == Stage::Screenshots {
                        job.set_stage_state(stage, StageState::Skipped);
                    } else if stage != Stage::ReadableDocument
                        || job.stage_state(stage) != StageState::Skipped
                    {
                        job.set_stage_state(stage, StageState::Completed);
                    }
                }
            }
        } else {
            // Non-terminal jobs have not completed cleanup, so their completed
            // stage artifacts must still be present and structurally valid.
            let completed: Vec<Stage> = pipeline_stages()
                .into_iter()
                .filter(|s| job.stage_state(*s) == StageState::Completed)
                .collect();
            for s in &completed {
                if let Some(dir) = &job.work_dir {
                    if !artifact_valid(*s, dir, job) {
                        job.set_stage_state(*s, StageState::Pending);
                        report.revalidated_stages.push(*s);
                    }
                }
            }
        }

        reports.push(report);
    }
    reports
}

/// Whether a missing stage artifact is an expected consequence of a cleanup
/// policy that has already run. Callers still need to reset a stage explicitly
/// when that artifact is required to rebuild a later output.
pub fn artifact_expected_removed(job: &Job, stage: Stage) -> bool {
    if job.status != JobStatus::Completed
        && job.stage_state(Stage::Cleanup) != StageState::Completed
    {
        return false;
    }
    match job.retention {
        RetentionPolicy::KeepAll => false,
        RetentionPolicy::Recommended => {
            matches!(stage, Stage::DownloadAudio | Stage::NormalizeAudio)
        }
        RetentionPolicy::DocumentsOnly => matches!(
            stage,
            Stage::Metadata
                | Stage::DownloadAudio
                | Stage::NormalizeAudio
                | Stage::Transcribe
                | Stage::ReadableDocument
        ),
    }
}

fn final_document_valid(job: &Job) -> bool {
    let output = job.final_output_dir.as_ref().map(|dir| dir.join("full.md"));
    let work = job.work_dir.as_ref().map(|dir| dir.join("full.md"));
    output
        .into_iter()
        .chain(work)
        .any(|path| md_has_job_id(&path, job))
}

fn invalid_retained_stages(job: &Job) -> Vec<Stage> {
    let Some(dir) = &job.work_dir else {
        return vec![Stage::Metadata];
    };

    pipeline_stages()
        .into_iter()
        .filter(|stage| retained_artifact_required(job, *stage))
        .filter(|stage| match stage {
            Stage::FinalDocument => !final_document_valid(job),
            _ => !artifact_valid(*stage, dir, job),
        })
        .collect()
}

fn retained_artifact_required(job: &Job, stage: Stage) -> bool {
    if matches!(stage, Stage::Queued | Stage::Cleanup | Stage::Completed) {
        return false;
    }
    if stage == Stage::Screenshots
        || (stage == Stage::ReadableDocument && job.stage_state(stage) == StageState::Skipped)
    {
        return false;
    }
    !artifact_expected_removed(job, stage)
}

fn earliest_rebuild_stage(job: &Job, invalid: &[Stage]) -> Stage {
    let first_invalid = pipeline_stages()
        .into_iter()
        .find(|stage| invalid.contains(stage))
        .unwrap_or(Stage::Metadata);
    let Some(dir) = &job.work_dir else {
        return Stage::Metadata;
    };

    let mut first = first_invalid;
    if stage_at_or_after(first, Stage::RawDocument)
        && !transcript_valid(&dir.join("transcript.raw.json"))
    {
        first = Stage::Transcribe;
    }
    if first == Stage::Transcribe && !nonempty_file(&dir.join("normalized.wav")) {
        first = Stage::NormalizeAudio;
    }
    if first == Stage::NormalizeAudio && !nonempty_file(&dir.join("source.audio")) {
        first = Stage::DownloadAudio;
    }
    if first == Stage::DownloadAudio && job.cid.is_none() {
        first = Stage::Metadata;
    }
    first
}

fn stage_at_or_after(stage: Stage, boundary: Stage) -> bool {
    let stages = pipeline_stages();
    let stage_index = stages.iter().position(|candidate| *candidate == stage);
    let boundary_index = stages.iter().position(|candidate| *candidate == boundary);
    matches!((stage_index, boundary_index), (Some(stage), Some(boundary)) if stage >= boundary)
}

fn nonempty_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false)
}

fn reset_pipeline_from(job: &mut Job, first: Stage, changed: &mut Vec<Stage>) {
    let mut reset = false;
    for stage in pipeline_stages() {
        reset |= stage == first;
        if !reset {
            continue;
        }
        let state = if stage == Stage::Screenshots {
            StageState::Skipped
        } else {
            StageState::Pending
        };
        if job.stage_state(stage) != state {
            job.set_stage_state(stage, state);
            changed.push(stage);
        }
    }
}

/// Remove any `*.tmp` files left by an interrupted atomic write.
fn cleanup_tmp_files(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("tmp") {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

/// Find the next Pending stage to resume from.
fn next_pending_stage(job: &Job) -> Option<Stage> {
    pipeline_stages()
        .into_iter()
        .find(|s| job.stage_state(*s) == StageState::Pending)
}

/// Validate a stage's required artifact.
///
/// `dir` is the job's work directory. Returns true if the artifact exists and
/// passes a structural check; false if missing/invalid (so the stage re-runs).
pub fn artifact_valid(stage: Stage, dir: &Path, job: &Job) -> bool {
    match stage {
        Stage::Metadata => {
            let p = dir.join("metadata.json");
            if let Ok(data) = std::fs::read_to_string(&p) {
                let v: serde_json::Value =
                    serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
                v.get("bvid").and_then(|v| v.as_str()).is_some()
                    && v.get("cid").and_then(|v| v.as_u64()).is_some()
                    && v.get("title").and_then(|v| v.as_str()).is_some()
            } else {
                false
            }
        }
        Stage::DownloadAudio => {
            // source.audio exists and size > 0
            std::fs::metadata(dir.join("source.audio"))
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        }
        Stage::NormalizeAudio => {
            // normalized.wav exists and is non-empty (ffmpeg readability is
            // checked again at run time; here a cheap existence+size check).
            std::fs::metadata(dir.join("normalized.wav"))
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        }
        Stage::Transcribe => transcript_valid(&dir.join("transcript.raw.json")),
        Stage::RawDocument => md_has_job_id(&dir.join("transcript.raw.md"), job),
        Stage::ReadableDocument => std::fs::read_to_string(dir.join("transcript.readable.md"))
            .map(|body| !body.trim().is_empty())
            .unwrap_or(false),
        Stage::Screenshots => {
            // Skipped stages are always "valid" (nothing to check).
            true
        }
        Stage::FinalDocument => md_has_job_id(&dir.join("full.md"), job),
        Stage::Cleanup | Stage::Completed => true,
        _ => true,
    }
}

/// `transcript.raw.json` parses and has unique utterance ids + sane timestamps.
fn transcript_valid(path: &Path) -> bool {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let utterances: Vec<crate::funasr::Utterance> = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mut ids = std::collections::HashSet::new();
    for utterance in &utterances {
        if !ids.insert(&utterance.id) {
            return false;
        }
        if utterance.start_ms > utterance.end_ms {
            return false;
        }
    }
    true
}

/// A `.md` artifact exists and embeds the job id marker.
fn md_has_job_id(path: &Path, job: &Job) -> bool {
    match std::fs::read_to_string(path) {
        Ok(s) => s.contains(&job.id.to_string()),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static HOME_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn stage_name_roundtrip() {
        for s in pipeline_stages() {
            assert_eq!(Stage::from_name(s.name()), Some(s));
        }
    }

    #[test]
    fn new_job_all_pending() {
        let j = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        for s in pipeline_stages() {
            assert_eq!(j.stage_state(s), StageState::Pending);
        }
        assert_eq!(j.total_progress(), 0);
    }

    #[test]
    fn total_progress_is_monotonic_and_completed_is_exactly_100() {
        let mut previous = 0;
        for stage in pipeline_stages() {
            for stage_progress in [0, 1, 50, 99, 100] {
                let current = total_progress_for(stage, stage_progress);
                assert!(
                    current >= previous,
                    "{stage:?} at {stage_progress}% regressed"
                );
                assert!(current <= 100);
                previous = current;
            }
        }
        assert_eq!(total_progress_for(Stage::Completed, 0), 100);

        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        job.stage = Stage::Cleanup;
        job.stage_progress = 99;
        job.status = JobStatus::Completed;
        assert_eq!(job.total_progress(), 100);
    }

    #[test]
    fn total_progress_has_stable_boundary_values() {
        assert_eq!(total_progress_for(Stage::Queued, 0), 0);
        assert_eq!(total_progress_for(Stage::Metadata, 50), 13);
        assert_eq!(total_progress_for(Stage::Cleanup, 99), 90);
        assert_eq!(total_progress_for(Stage::Completed, 100), 100);
    }

    #[test]
    fn atomic_write_replaces() {
        let dir = std::env::temp_dir().join("bimyscribe-jobs-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.json");
        atomic_write(&p, b"first").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
        atomic_write(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
        // No leftover .tmp
        assert!(!dir.join("f.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recover_resets_running_to_pending() {
        let id = Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("bimyscribe-recover-{}", id));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut job = Job::new(id, "BV1test".into(), 1);
        job.work_dir = Some(dir.clone());
        job.set_stage_state(Stage::Metadata, StageState::Completed);
        job.set_stage_state(Stage::DownloadAudio, StageState::Running);
        job.status = JobStatus::Running;
        job.stage = Stage::DownloadAudio;
        // metadata.json absent -> Completed will be revalidated to Pending.
        let mut q = Queue { jobs: vec![job] };
        let reports = recover(&mut q);
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert!(r.reset_stages.contains(&Stage::DownloadAudio));
        assert!(r.revalidated_stages.contains(&Stage::Metadata));
        let job = &q.jobs[0];
        assert_eq!(job.stage_state(Stage::DownloadAudio), StageState::Pending);
        assert_eq!(job.stage_state(Stage::Metadata), StageState::Pending);
        assert_eq!(job.status, JobStatus::Queued);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recover_keeps_valid_completed() {
        let id = Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("bimyscribe-recover2-{}", id));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        // Write a valid metadata.json.
        let md = r#"{"bvid":"BV1test","cid":123,"title":"t"}"#;
        std::fs::write(dir.join("metadata.json"), md).unwrap();
        let mut job = Job::new(id, "BV1test".into(), 1);
        job.work_dir = Some(dir.clone());
        job.set_stage_state(Stage::Metadata, StageState::Completed);
        let mut q = Queue { jobs: vec![job] };
        let reports = recover(&mut q);
        let r = &reports[0];
        assert!(!r.revalidated_stages.contains(&Stage::Metadata));
        assert_eq!(
            q.jobs[0].stage_state(Stage::Metadata),
            StageState::Completed
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recover_is_in_memory_and_does_not_touch_the_app_queue() {
        let _guard = HOME_ENV_LOCK.lock().unwrap();
        let original_home = std::env::var_os("HOME");
        let home = std::env::temp_dir().join(format!("bimyscribe-recover-home-{}", Uuid::new_v4()));
        let app_dir = home
            .join("Library")
            .join("Application Support")
            .join("BiMyScribe");
        std::fs::create_dir_all(&app_dir).unwrap();
        let queue_file = app_dir.join("queue.json");
        let sentinel = b"production queue sentinel";
        std::fs::write(&queue_file, sentinel).unwrap();
        std::env::set_var("HOME", &home);

        let job_dir = home.join("isolated-job");
        std::fs::create_dir_all(&job_dir).unwrap();
        let state_file = job_dir.join("state.json");
        let state_sentinel = b"job state sentinel";
        std::fs::write(&state_file, state_sentinel).unwrap();
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        job.work_dir = Some(job_dir);
        let mut queue = Queue { jobs: vec![job] };
        recover(&mut queue);
        let after = std::fs::read(&queue_file).unwrap();
        let state_after = std::fs::read(&state_file).unwrap();

        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(home).ok();
        assert_eq!(after, sentinel);
        assert_eq!(state_after, state_sentinel);
    }

    fn completed_job_with_retained_artifacts(retention: RetentionPolicy) -> (Job, PathBuf) {
        let id = Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("bimyscribe-completed-{id}"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut job = Job::new(id, "BV1test".into(), 1);
        job.work_dir = Some(dir.clone());
        job.final_output_dir = Some(dir.clone());
        job.retention = retention;
        job.status = JobStatus::Completed;
        job.stage = Stage::Completed;
        job.stage_progress = 100;
        job.cid = Some(123);
        for stage in pipeline_stages() {
            job.set_stage_state(stage, StageState::Completed);
        }
        std::fs::write(
            dir.join("metadata.json"),
            r#"{"bvid":"BV1test","cid":123,"title":"fixture"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("source.audio"), b"source audio").unwrap();
        std::fs::write(dir.join("normalized.wav"), b"normalized audio").unwrap();
        std::fs::write(dir.join("transcript.raw.json"), b"[]").unwrap();
        std::fs::write(
            dir.join("transcript.raw.md"),
            format!("<!-- job-id: {} -->\nraw", job.id),
        )
        .unwrap();
        std::fs::write(dir.join("transcript.readable.md"), "readable").unwrap();
        std::fs::write(
            dir.join("full.md"),
            format!("<!-- job-id: {} -->\ncomplete", job.id),
        )
        .unwrap();

        match retention {
            RetentionPolicy::KeepAll => {}
            RetentionPolicy::Recommended => {
                std::fs::remove_file(dir.join("source.audio")).unwrap();
                std::fs::remove_file(dir.join("normalized.wav")).unwrap();
            }
            RetentionPolicy::DocumentsOnly => {
                for artifact in [
                    "metadata.json",
                    "source.audio",
                    "normalized.wav",
                    "transcript.raw.json",
                    "transcript.readable.md",
                ] {
                    std::fs::remove_file(dir.join(artifact)).unwrap();
                }
            }
        }
        (job, dir)
    }

    #[test]
    fn recover_completed_recommended_ignores_expected_deleted_intermediates() {
        let (job, dir) = completed_job_with_retained_artifacts(RetentionPolicy::Recommended);
        assert!(artifact_valid(Stage::Metadata, &dir, &job));
        assert!(artifact_valid(Stage::Transcribe, &dir, &job));
        assert!(artifact_valid(Stage::RawDocument, &dir, &job));
        assert!(artifact_valid(Stage::ReadableDocument, &dir, &job));
        assert!(artifact_valid(Stage::FinalDocument, &dir, &job));
        assert!(!dir.join("source.audio").exists());
        assert!(!dir.join("normalized.wav").exists());
        let mut queue = Queue { jobs: vec![job] };
        recover(&mut queue);
        let recovered = &queue.jobs[0];
        assert_eq!(recovered.status, JobStatus::Completed);
        assert_eq!(recovered.total_progress(), 100);
        assert_eq!(
            recovered.stage_state(Stage::DownloadAudio),
            StageState::Completed
        );
        assert_eq!(
            recovered.stage_state(Stage::NormalizeAudio),
            StageState::Completed
        );
        assert_eq!(
            recovered.stage_state(Stage::Screenshots),
            StageState::Skipped
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recover_completed_documents_only_accepts_expected_cleanup_deletions() {
        let (mut job, dir) = completed_job_with_retained_artifacts(RetentionPolicy::DocumentsOnly);
        assert!(artifact_valid(Stage::RawDocument, &dir, &job));
        assert!(artifact_valid(Stage::FinalDocument, &dir, &job));
        assert!(!dir.join("metadata.json").exists());
        assert!(!dir.join("transcript.raw.json").exists());
        // Reproduce the old invalid Completed + Pending mixture.
        job.set_stage_state(Stage::DownloadAudio, StageState::Pending);
        job.set_stage_state(Stage::ReadableDocument, StageState::Skipped);
        let mut queue = Queue { jobs: vec![job] };
        recover(&mut queue);
        let recovered = &queue.jobs[0];
        assert_eq!(recovered.status, JobStatus::Completed);
        assert!(!recovered
            .stages
            .values()
            .any(|state| *state == StageState::Pending));
        assert_eq!(
            recovered.stage_state(Stage::ReadableDocument),
            StageState::Skipped
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recover_completed_keep_all_stays_completed() {
        let (job, dir) = completed_job_with_retained_artifacts(RetentionPolicy::KeepAll);
        let mut queue = Queue { jobs: vec![job] };
        recover(&mut queue);
        assert_eq!(queue.jobs[0].status, JobStatus::Completed);
        assert_eq!(queue.jobs[0].total_progress(), 100);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recover_completed_keep_all_requeues_at_missing_retained_source() {
        let (job, dir) = completed_job_with_retained_artifacts(RetentionPolicy::KeepAll);
        std::fs::remove_file(dir.join("source.audio")).unwrap();
        let mut queue = Queue { jobs: vec![job] };

        recover(&mut queue);

        let recovered = &queue.jobs[0];
        assert_eq!(recovered.status, JobStatus::Queued);
        assert_eq!(recovered.stage, Stage::DownloadAudio);
        assert_eq!(
            recovered.stage_state(Stage::DownloadAudio),
            StageState::Pending
        );
        assert_eq!(recovered.stage_state(Stage::Cleanup), StageState::Pending);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recover_completed_recommended_requeues_at_missing_retained_raw_document() {
        let (job, dir) = completed_job_with_retained_artifacts(RetentionPolicy::Recommended);
        std::fs::remove_file(dir.join("transcript.raw.md")).unwrap();
        let mut queue = Queue { jobs: vec![job] };

        recover(&mut queue);

        let recovered = &queue.jobs[0];
        assert_eq!(recovered.status, JobStatus::Queued);
        assert_eq!(recovered.stage, Stage::RawDocument);
        assert_eq!(
            recovered.stage_state(Stage::RawDocument),
            StageState::Pending
        );
        assert_eq!(recovered.stage_state(Stage::Cleanup), StageState::Pending);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recover_documents_only_missing_final_resets_rebuild_chain() {
        let (job, dir) = completed_job_with_retained_artifacts(RetentionPolicy::DocumentsOnly);
        std::fs::remove_file(dir.join("full.md")).unwrap();
        let mut queue = Queue { jobs: vec![job] };
        recover(&mut queue);
        let recovered = &queue.jobs[0];
        assert_eq!(recovered.status, JobStatus::Queued);
        assert_eq!(recovered.stage, Stage::DownloadAudio);
        for stage in [
            Stage::DownloadAudio,
            Stage::NormalizeAudio,
            Stage::Transcribe,
            Stage::RawDocument,
            Stage::ReadableDocument,
            Stage::FinalDocument,
            Stage::Completed,
        ] {
            assert_eq!(
                recovered.stage_state(stage),
                StageState::Pending,
                "{stage:?} must be rebuilt"
            );
        }
        assert_eq!(
            recovered.stage_state(Stage::Screenshots),
            StageState::Skipped
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recover_missing_final_requeues_from_earliest_rebuildable_stage() {
        let (job, dir) = completed_job_with_retained_artifacts(RetentionPolicy::Recommended);
        std::fs::remove_file(dir.join("full.md")).unwrap();
        std::fs::write(dir.join("transcript.raw.json"), "[]").unwrap();
        let mut queue = Queue { jobs: vec![job] };
        let reports = recover(&mut queue);
        let recovered = &queue.jobs[0];
        assert_eq!(recovered.status, JobStatus::Queued);
        assert_eq!(recovered.stage, Stage::FinalDocument);
        assert_eq!(
            recovered.stage_state(Stage::FinalDocument),
            StageState::Pending
        );
        assert_eq!(recovered.stage_state(Stage::Completed), StageState::Pending);
        assert!(reports[0]
            .revalidated_stages
            .contains(&Stage::FinalDocument));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn capabilities_for_queued_job() {
        let job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        let caps = JobCapabilities::from_job(&job);
        assert!(caps.can_cancel);
        assert!(!caps.can_retry);
        assert!(!caps.can_open_document);
        assert!(!caps.can_edit_speakers); // no work_dir yet
    }

    #[test]
    fn capabilities_for_completed_job() {
        let dir = std::env::temp_dir().join(format!("bimyscribe-caps-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("transcript.raw.json"), b"[]").unwrap();
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        job.status = JobStatus::Completed;
        job.work_dir = Some(dir.clone());
        let caps = JobCapabilities::from_job(&job);
        assert!(!caps.can_cancel);
        assert!(caps.can_open_document);
        assert!(caps.can_reveal);
        assert!(caps.can_edit_speakers);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn capabilities_for_failed_job() {
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        job.status = JobStatus::Failed;
        let caps = JobCapabilities::from_job(&job);
        assert!(!caps.can_cancel);
        assert!(caps.can_retry);
    }

    #[test]
    fn capabilities_for_cancelled_job() {
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        job.status = JobStatus::Cancelled;
        let caps = JobCapabilities::from_job(&job);
        assert!(!caps.can_cancel);
        assert!(!caps.can_retry);
    }

    #[test]
    fn snapshot_reads_stage_state_directly() {
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        // Mark Metadata as Completed, ReadableDocument as Skipped.
        job.set_stage_state(Stage::Metadata, StageState::Completed);
        job.set_stage_state(Stage::ReadableDocument, StageState::Skipped);
        let snap = JobViewSnapshot::from_job(&job, chrono::Utc::now());
        // Find the metadata and readable_document stage snapshots.
        let meta = snap.stages.iter().find(|s| s.name == "metadata").unwrap();
        assert_eq!(meta.state, StageState::Completed);
        let readable = snap
            .stages
            .iter()
            .find(|s| s.name == "readable_document")
            .unwrap();
        assert_eq!(readable.state, StageState::Skipped);
        // Skipped must not show as Completed.
        assert_ne!(readable.state, StageState::Completed);
    }

    #[test]
    fn snapshot_elapsed_zero_before_start() {
        let job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        let snap = JobViewSnapshot::from_job(&job, chrono::Utc::now());
        assert_eq!(snap.elapsed_secs, 0);
    }

    #[test]
    fn snapshot_elapsed_frozen_after_finish() {
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        let start = chrono::Utc::now() - chrono::Duration::seconds(100);
        let end = start + chrono::Duration::seconds(50);
        job.started_at = Some(start);
        job.finished_at = Some(end);
        let snap = JobViewSnapshot::from_job(&job, end + chrono::Duration::seconds(500));
        // Elapsed is frozen at finish - start, not now - start.
        assert_eq!(snap.elapsed_secs, 50);
    }

    #[test]
    fn snapshot_elapsed_grows_during_run() {
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        let start = chrono::Utc::now() - chrono::Duration::seconds(30);
        job.started_at = Some(start);
        let now = chrono::Utc::now();
        let snap = JobViewSnapshot::from_job(&job, now);
        assert!(snap.elapsed_secs >= 30);
    }

    #[test]
    fn old_state_json_loads_without_time_fields() {
        // A state.json from before the time fields were added should still parse.
        let id = Uuid::new_v4();
        let old_json = format!(
            r#"{{"id":"{}","bvid":"BV1test","page":1,"title":"t","stage":"queued","status":"queued","stage_progress":0,"error":null,"work_dir":null,"stages":{{}},"speaker_map":{{}}}}"#,
            id
        );
        let job: Job = serde_json::from_str(&old_json).unwrap();
        assert_eq!(job.id, id);
        assert!(job.created_at.is_none());
        assert!(job.started_at.is_none());
        assert!(job.finished_at.is_none());
        assert!(job.warning.is_none());
        assert_eq!(job.retention, RetentionPolicy::Recommended);
    }

    #[test]
    fn job_warning_label() {
        let w = JobWarning::LlmFallback {
            message: "timeout".into(),
        };
        assert!(w.label().contains("LLM"));
        assert!(w.label().contains("timeout"));
        let w = JobWarning::OptionalStageSkipped {
            stage: "screenshots".into(),
            reason: "disabled".into(),
        };
        assert!(w.label().contains("screenshots"));
    }

    #[test]
    fn job_warning_serde_roundtrip() {
        let w = JobWarning::LlmFallback {
            message: "timeout".into(),
        };
        let json = serde_json::to_string(&w).unwrap();
        let w2: JobWarning = serde_json::from_str(&json).unwrap();
        match w2 {
            JobWarning::LlmFallback { message } => assert_eq!(message, "timeout"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn retention_field_defaults_to_recommended() {
        let job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        assert_eq!(job.retention, RetentionPolicy::Recommended);
    }
}
