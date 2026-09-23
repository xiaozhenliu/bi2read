//! Task state, stages and the in-memory + persisted queue.
//!
//! Implements the stage enum, per-stage completion state
//! (`StageState`), and atomic file-backed persistence (`queue.json` on the
//! system disk + `state.json` per job on the external drive). State writes use
//! `.tmp` + atomic rename so a crash never leaves a half-written
//! JSON file. Startup recovery resets in-flight stages and re-validates each
//! stage's required artifacts against the documented minimum-artifact table.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::content_results::ContentSetupV1;

/// The source language requested from a transcription Runtime.
///
/// This is deliberately a closed set.  The wire representation is shared by
/// persisted jobs, the CLI and Runtime adapters, so adding another language
/// must be an explicit product/schema change rather than an arbitrary string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SourceLanguage {
    #[default]
    Auto,
    Zh,
    En,
}

impl SourceLanguage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动检测",
            Self::Zh => "中文",
            Self::En => "英文",
        }
    }
}

impl fmt::Display for SourceLanguage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLanguageParseError {
    value: String,
}

impl fmt::Display for SourceLanguageParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid source language {:?}; expected auto, zh, or en",
            self.value
        )
    }
}

impl std::error::Error for SourceLanguageParseError {}

impl FromStr for SourceLanguage {
    type Err = SourceLanguageParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "zh" => Ok(Self::Zh),
            "en" => Ok(Self::En),
            _ => Err(SourceLanguageParseError {
                value: value.to_string(),
            }),
        }
    }
}

/// Whether a Runtime comes from the packaged application or an external
/// project selected by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeSource {
    Bundled,
    External,
}

impl RuntimeSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::External => "external",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Bundled => "内置 Runtime",
            Self::External => "自定义 Runtime",
        }
    }
}

impl fmt::Display for RuntimeSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The execution backend frozen into a job's transcription selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeBackend {
    NativeUv,
    DockerCompose,
}

impl RuntimeBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeUv => "native-uv",
            Self::DockerCompose => "docker-compose",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::NativeUv => "native-uv",
            Self::DockerCompose => "docker-compose",
        }
    }
}

impl fmt::Display for RuntimeBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The product entry point that created a Job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CreatedFrom {
    App,
    Cli,
}

impl CreatedFrom {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Cli => "cli",
        }
    }
}

impl fmt::Display for CreatedFrom {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A frozen Runtime and source-language choice persisted with each new Job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptionSelection {
    pub runtime_source: RuntimeSource,
    pub runtime_project: PathBuf,
    pub runtime_data_dir: PathBuf,
    pub runtime_identity: String,
    pub runtime_backend: RuntimeBackend,
    pub model_description: Option<String>,
    pub requested_language: SourceLanguage,
    pub created_from: CreatedFrom,
}

impl TranscriptionSelection {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runtime_source: RuntimeSource,
        runtime_project: PathBuf,
        runtime_data_dir: PathBuf,
        runtime_identity: String,
        runtime_backend: RuntimeBackend,
        model_description: Option<String>,
        requested_language: SourceLanguage,
        created_from: CreatedFrom,
    ) -> Self {
        Self {
            runtime_source,
            runtime_project,
            runtime_data_dir,
            runtime_identity,
            runtime_backend,
            model_description,
            requested_language,
            created_from,
        }
    }

    /// Validate invariants that are guaranteed by the task-creation seam.
    ///
    /// Deserialization intentionally remains permissive about path existence:
    /// a persisted Job must remain inspectable after a drive is disconnected.
    /// Runtime readiness and identity validation belong to the Runtime seam.
    pub fn validate(&self) -> Result<(), String> {
        if !self.runtime_project.is_absolute() {
            return Err("runtime_project must be an absolute path".into());
        }
        if !self.runtime_data_dir.is_absolute() {
            return Err("runtime_data_dir must be an absolute path".into());
        }
        if self.runtime_identity.trim().is_empty() {
            return Err("runtime_identity must not be empty".into());
        }
        Ok(())
    }
}

/// The processing identity reported by the Runtime after a transcription run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptionResult {
    #[serde(default)]
    pub reported_language: Option<SourceLanguage>,
    #[serde(default)]
    pub reported_model: Option<String>,
    #[serde(default)]
    pub reported_runtime_identity: Option<String>,
}

impl TranscriptionResult {
    pub fn new(
        reported_language: Option<SourceLanguage>,
        reported_model: Option<String>,
        reported_runtime_identity: Option<String>,
    ) -> Self {
        Self {
            reported_language,
            reported_model,
            reported_runtime_identity,
        }
    }
}

/// Inputs accepted by the shared GUI/CLI Job-construction interface.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobCreationInput {
    pub(crate) id: Uuid,
    pub(crate) bvid: String,
    pub(crate) page: u32,
    pub(crate) selection: TranscriptionSelection,
    pub(crate) retention: RetentionPolicy,
    /// Frozen v0.5 content plan. Every production creation path must provide
    /// the resolved Disabled/Ready/Unavailable value explicitly.
    pub(crate) content_setup: ContentSetupV1,
}

impl JobCreationInput {
    pub(crate) fn new(
        id: Uuid,
        bvid: String,
        page: u32,
        selection: TranscriptionSelection,
        retention: RetentionPolicy,
        content_setup: ContentSetupV1,
    ) -> Self {
        Self {
            id,
            bvid,
            page,
            selection,
            retention,
            content_setup,
        }
    }
}

/// Defaults read from Config when constructing a *new* task.
///
/// This intentionally contains no persisted-job parsing or migration logic.
/// A caller must resolve the Runtime description and build a complete
/// [`TranscriptionSelection`] before calling [`Job::from_creation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCreationDefaults {
    pub runtime_project: Option<PathBuf>,
    pub runtime_data_dir: PathBuf,
    pub requested_language: SourceLanguage,
    pub retention: RetentionPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptionSelectionStatus {
    Recorded,
    LegacyUnrecorded,
}

impl TranscriptionSelectionStatus {
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::LegacyUnrecorded => LEGACY_UNRECORDED,
        }
    }
}

pub const LEGACY_UNRECORDED: &str = "legacy-unrecorded";

/// Surfaced when `recover()` finds the Transcribe stage still Running at App
/// startup: the previous attempt was interrupted (Runtime OOM kill, lost
/// Docker engine, or an App restart) and is deliberately not requeued, since
/// requeuing would silently replay the same failure.
/// 转写阶段检测到冻结的 Runtime 身份失配时持久化到 `error` 的错误码前缀。
pub const RUNTIME_IDENTITY_CHANGED: &str = "runtime-identity-changed";

pub const TRANSCRIBE_INTERRUPTED: &str =
    "上次转写在 App 退出或 Runtime 中断时未完成，未自动重跑；请确认 Docker Desktop 内存充足后手动重试";

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
    /// Machine-readable status key, symmetric with `Stage::name()`. Consumed
    /// by the Slint UI (`StageStyle.kind()` / `label()`) so task-level status
    /// can outrank the stage key when the two disagree (e.g. `Cancelling`
    /// while `stage` is still `Transcribe`); task status therefore takes precedence.
    pub fn name(&self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Paused => "paused",
            JobStatus::NeedsUserAction => "needs_user_action",
            JobStatus::WaitingForDrive => "waiting_for_drive",
            JobStatus::Failed => "failed",
            JobStatus::Cancelling => "cancelling",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Completed => "completed",
        }
    }

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

    /// Whether the job reached a state that history management may delete.
    /// Non-terminal jobs (queued, running, cancelling, waiting) can still
    /// make progress and must be cancelled before deletion.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
        )
    }

    /// Whether the persisted state machine accepts an explicit cancellation
    /// request from the user.
    pub const fn can_request_cancel(self) -> bool {
        matches!(
            self,
            JobStatus::Queued
                | JobStatus::Running
                | JobStatus::Paused
                | JobStatus::NeedsUserAction
                | JobStatus::WaitingForDrive
        )
    }
}

/// Deterministic result of applying a cancellation request to a persisted Job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationTransition {
    /// A running pipeline owns resources and must observe its cancellation token.
    Cooperative,
    /// No pipeline resources are owned, so the Job reached `Cancelled` directly.
    Immediate,
    /// The Job was already converging toward cancellation.
    AlreadyCancelling,
    /// Terminal states do not accept cancellation.
    Unavailable,
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
    pub can_rebuild: bool,
    pub can_open_document: bool,
    pub can_reveal: bool,
    pub can_edit_speakers: bool,
    pub can_delete: bool,
}

impl JobCapabilities {
    pub fn from_job(job: &Job) -> Self {
        let is_terminal = job.status.is_terminal();
        let is_done = job.status == JobStatus::Completed;
        let has_transcript = job
            .work_dir
            .as_ref()
            .map(|d| transcript_valid(&d.join("transcript.raw.json")))
            .unwrap_or(false);
        let v05_view = if job.content_setup.is_some() && is_terminal {
            crate::content_results::current(job).ok()
        } else {
            None
        };
        let v05_raw_only = matches!(
            &v05_view,
            Some(crate::content_results::ContentViewV1::RawOnly { .. })
        );
        let v05_current = matches!(
            &v05_view,
            Some(crate::content_results::ContentViewV1::Current(_))
        );
        Self {
            can_cancel: job.status.can_request_cancel(),
            can_rebuild: job.status == JobStatus::NeedsUserAction
                && (job.requires_transcription_rebuild() || job.requires_runtime_reselection()),
            can_retry: (job.status == JobStatus::Failed
                || job.status == JobStatus::NeedsUserAction)
                && job.transcription_selection.is_some()
                && !v05_raw_only,
            // v0.5 result views are terminal-only and can intentionally open
            // Failed/Cancelled jobs as RawOnly when their Evidence survives.
            // Legacy jobs retain the historical Completed-only affordance.
            can_open_document: if job.content_setup.is_some() {
                is_terminal && (v05_current || v05_raw_only)
            } else {
                is_done
            },
            can_reveal: job.work_dir.is_some(),
            can_edit_speakers: if job.content_setup.is_some() {
                is_terminal && v05_current
            } else {
                has_transcript
            },
            can_delete: is_terminal,
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
    pub transcription_runtime: String,
    pub transcription_source: String,
    pub transcription_backend: String,
    pub transcription_model: String,
    pub requested_language: String,
    pub reported_language: String,
    pub reported_model: String,
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
        let (
            transcription_runtime,
            transcription_source,
            transcription_backend,
            transcription_model,
            requested_language,
        ) = match &job.transcription_selection {
            Some(selection) => (
                selection.runtime_source.label().to_string(),
                selection.runtime_source.as_str().to_string(),
                selection.runtime_backend.label().to_string(),
                selection
                    .model_description
                    .clone()
                    .unwrap_or_else(|| "未提供".to_string()),
                selection.requested_language.label().to_string(),
            ),
            None => (
                "未记录".to_string(),
                "未记录".to_string(),
                "未记录".to_string(),
                "未提供".to_string(),
                "未记录".to_string(),
            ),
        };
        let (reported_language, reported_model) = match &job.transcription_result {
            Some(result) => (
                result
                    .reported_language
                    .map(SourceLanguage::label)
                    .unwrap_or("未报告")
                    .to_string(),
                result
                    .reported_model
                    .clone()
                    .unwrap_or_else(|| "未报告".to_string()),
            ),
            None => ("未报告".to_string(), "未报告".to_string()),
        };
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
            transcription_runtime,
            transcription_source,
            transcription_backend,
            transcription_model,
            requested_language,
            reported_language,
            reported_model,
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
    /// Selected Bilibili page (分P) title, filled at metadata time. It remains
    /// separate from the main video title for v0.5 source context.
    #[serde(default)]
    pub part_title: Option<String>,
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
    /// Frozen Runtime and source-language choice. `None` is preserved for
    /// pre-v0.4 jobs and is reported as `legacy-unrecorded` rather than being
    /// filled from the current Config.
    #[serde(default)]
    pub transcription_selection: Option<TranscriptionSelection>,
    /// Runtime-reported processing identity, if the Runtime supplied one.
    #[serde(default)]
    pub transcription_result: Option<TranscriptionResult>,
    /// Explicit v0.5 marker and frozen initial content setup. `None` is
    /// permanently reserved for legacy jobs; it is never inferred from files.
    #[serde(default)]
    pub(crate) content_setup: Option<ContentSetupV1>,
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
            part_title: None,
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
            transcription_selection: None,
            transcription_result: None,
            content_setup: None,
        }
    }

    /// Construct a new Job through the shared GUI/CLI creation seam.
    ///
    /// `selection` is copied into the Job before it can enter the queue and is
    /// never re-derived from Config during recovery or retry.
    pub(crate) fn from_creation(input: JobCreationInput) -> Self {
        let mut job = Self::new(input.id, input.bvid, input.page);
        job.transcription_selection = Some(input.selection);
        job.retention = input.retention;
        job.content_setup = Some(input.content_setup);
        for stage in pipeline_stages() {
            job.set_stage_state(stage, StageState::Pending);
        }
        job
    }

    /// Construct a v0.5 Job for headless package/smoke drivers that already
    /// hold a verified transcription selection. The production App/CLI use
    /// the richer crate-internal creation seam so their Config-derived
    /// Disabled/Ready/Unavailable content setup is frozen explicitly.
    #[doc(hidden)]
    pub fn new_v05_disabled_with_selection(
        id: Uuid,
        bvid: String,
        page: u32,
        selection: TranscriptionSelection,
        retention: RetentionPolicy,
    ) -> Self {
        Self::from_creation(JobCreationInput::new(
            id,
            bvid,
            page,
            selection,
            retention,
            ContentSetupV1::disabled(),
        ))
    }

    pub fn transcription_selection_status(&self) -> TranscriptionSelectionStatus {
        if self.transcription_selection.is_some() {
            TranscriptionSelectionStatus::Recorded
        } else {
            TranscriptionSelectionStatus::LegacyUnrecorded
        }
    }

    /// Stable marker used by queue/state migration and machine-readable error
    /// handling. It must not be replaced with a current Config value.
    pub fn transcription_selection_marker(&self) -> &'static str {
        self.transcription_selection_status().marker()
    }

    pub fn requires_transcription_rebuild(&self) -> bool {
        self.transcription_selection.is_none()
    }

    /// 任务冻结的 Runtime 身份已与当前 Runtime 失配：重试注定再次失败，
    /// 只能用当前 Runtime 重建。判定依据是转写阶段持久化的机器可读错误码。
    pub fn requires_runtime_reselection(&self) -> bool {
        self.status == JobStatus::NeedsUserAction
            && self
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with(RUNTIME_IDENTITY_CHANGED))
    }

    /// Apply the single persisted cancellation state contract.
    ///
    /// Running work remains cooperative so its pipeline/runtime resources can
    /// shut down through the registered token. States that do not own runtime
    /// resources converge immediately and idempotently to a deletable terminal
    /// state. Callers persist the mutated Job and queue before reporting success.
    pub fn request_cancellation(
        &mut self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> CancellationTransition {
        match self.status {
            JobStatus::Running => {
                self.status = JobStatus::Cancelling;
                CancellationTransition::Cooperative
            }
            JobStatus::Queued
            | JobStatus::Paused
            | JobStatus::NeedsUserAction
            | JobStatus::WaitingForDrive => {
                self.status = JobStatus::Cancelled;
                self.stage = Stage::Cancelled;
                self.stage_progress = 0;
                self.error = None;
                self.finished_at = Some(now);
                CancellationTransition::Immediate
            }
            JobStatus::Cancelling => CancellationTransition::AlreadyCancelling,
            JobStatus::Failed | JobStatus::Cancelled | JobStatus::Completed => {
                CancellationTransition::Unavailable
            }
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

/// Remove a terminal job's on-disk artifacts: the job's work directory
/// (including `state.json`) and its per-job output directory.
///
/// A directory that does not exist counts as success — the external drive may
/// be unplugged, or retention may already have removed the path. Callers must
/// still remove the job from `queue.json` (the resurrection path) only after
/// this succeeds, so a failed artifact deletion keeps a visible record the
/// user can retry from.
pub fn delete_job_artifacts(job: &Job) -> Result<(), String> {
    let mut removed: Vec<PathBuf> = Vec::new();
    if let Some(dir) = &job.work_dir {
        remove_job_dir(dir)?;
        removed.push(dir.clone());
    }
    if let Some(dir) = &job.final_output_dir {
        // Guard against a future or legacy layout that points at a shared
        // output root: only job-owned directories contain the final document.
        if !removed.contains(dir) && dir.join("full.md").exists() {
            remove_job_dir(dir)?;
        }
    }
    Ok(())
}

fn remove_job_dir(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}：{error}", dir.display())),
    }
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
        if running.contains(&Stage::Transcribe) {
            // A Runtime OOM kill, a lost Docker engine, or an App restart
            // mid-transcription all leave this stage Running forever
            // (`funasr::run`'s wait loop never returned). Requeuing would
            // silently replay the same OOM instead of surfacing it, so this
            // is the one stage recovery converges to Failed rather than
            // Pending/Queued.
            job.set_stage_state(Stage::Transcribe, StageState::Failed);
            job.status = JobStatus::Failed;
            job.stage = Stage::Transcribe;
            job.stage_progress = 0;
            job.error = Some(TRANSCRIBE_INTERRUPTED.to_string());
        } else {
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
        }

        // A pre-v0.4 Job has no trustworthy Runtime or language identity.
        // Keep completed Evidence readable, but never resume or retry a
        // legacy task using the current Config. A completed legacy task is
        // already an immutable historical result; if it needs processing
        // again, the user must explicitly rebuild it with a new selection.
        if job.requires_transcription_rebuild() {
            if job.status.is_terminal() {
                if job.status == JobStatus::Completed {
                    job.stage = Stage::Completed;
                    job.stage_progress = 100;
                } else if job.status == JobStatus::Cancelled {
                    job.stage = Stage::Cancelled;
                }
                reports.push(report);
                continue;
            }

            job.status = JobStatus::NeedsUserAction;
            job.stage = Stage::NeedsUserAction;
            job.stage_progress = 0;
            job.finished_at = None;
            job.error = Some(format!(
                "{LEGACY_UNRECORDED}: task must be rebuilt with explicit transcription settings"
            ));
            reports.push(report);
            continue;
        }

        // v0.5 terminal jobs are facts, not scheduler work items. A missing or
        // corrupt current is surfaced as RawOnly by ContentResults; recovery
        // must not silently requeue the whole media/ASR pipeline. Presentation
        // files are rebuilt lazily by open/export or an explicit repair.
        if job.content_setup.is_some() && job.status.is_terminal() {
            if job.status == JobStatus::Completed {
                job.stage = Stage::Completed;
                job.stage_progress = 100;
                for stage in pipeline_stages() {
                    if stage == Stage::Screenshots {
                        job.set_stage_state(stage, StageState::Skipped);
                    } else {
                        job.set_stage_state(stage, StageState::Completed);
                    }
                }
            }
            reports.push(report);
            continue;
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
        RetentionPolicy::DocumentsOnly => {
            if job.content_setup.is_some() {
                matches!(stage, Stage::DownloadAudio | Stage::NormalizeAudio)
            } else {
                matches!(
                    stage,
                    Stage::Metadata
                        | Stage::DownloadAudio
                        | Stage::NormalizeAudio
                        | Stage::Transcribe
                        | Stage::ReadableDocument
                )
            }
        }
    }
}

fn final_document_valid(job: &Job) -> bool {
    if job.content_setup.is_some() {
        return job
            .final_output_dir
            .as_ref()
            .map(|dir| md_has_job_id(&dir.join("full.md"), job))
            .unwrap_or(false);
    }
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
        Stage::ReadableDocument => {
            if job.content_setup.is_some() {
                matches!(
                    crate::content_results::current(job),
                    Ok(crate::content_results::ContentViewV1::Current(_))
                )
            } else {
                std::fs::read_to_string(dir.join("transcript.readable.md"))
                    .map(|body| !body.trim().is_empty())
                    .unwrap_or(false)
            }
        }
        Stage::Screenshots => {
            // Skipped stages are always "valid" (nothing to check).
            true
        }
        Stage::FinalDocument => {
            if job.content_setup.is_some() {
                final_document_valid(job)
            } else {
                md_has_job_id(&dir.join("full.md"), job)
            }
        }
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
    if utterances.is_empty() {
        return false;
    }
    let mut ids = std::collections::HashSet::new();
    for utterance in &utterances {
        if !ids.insert(&utterance.id) {
            return false;
        }
        if utterance.text.trim().is_empty() {
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

    fn sample_selection(created_from: CreatedFrom) -> TranscriptionSelection {
        TranscriptionSelection::new(
            RuntimeSource::External,
            PathBuf::from("/tmp/funasr-runtime"),
            PathBuf::from("/tmp/funasr-data"),
            "runtime-test-identity".into(),
            RuntimeBackend::DockerCompose,
            Some("测试模型".into()),
            SourceLanguage::En,
            created_from,
        )
    }

    #[test]
    fn source_language_uses_closed_wire_values() {
        for (language, wire) in [
            (SourceLanguage::Auto, "auto"),
            (SourceLanguage::Zh, "zh"),
            (SourceLanguage::En, "en"),
        ] {
            assert_eq!(
                serde_json::to_string(&language).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(SourceLanguage::from_str(wire).unwrap(), language);
        }
        for invalid in ["", "zh-cn", "中文", "de"] {
            assert!(SourceLanguage::from_str(invalid).is_err());
            assert!(serde_json::from_str::<SourceLanguage>(&format!("\"{invalid}\"")).is_err());
        }
    }

    #[test]
    fn selection_and_result_serde_roundtrip_preserves_identity() {
        let selection = sample_selection(CreatedFrom::Cli);
        let result = TranscriptionResult::new(
            Some(SourceLanguage::En),
            Some("model-en".into()),
            Some("runtime-reported-identity".into()),
        );
        let json = serde_json::to_string(&selection).unwrap();
        let decoded: TranscriptionSelection = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, selection);
        assert!(json.contains("\"runtime_source\":\"external\""));
        assert!(json.contains("\"runtime_backend\":\"docker-compose\""));
        assert!(json.contains("\"requested_language\":\"en\""));
        assert!(json.contains("\"created_from\":\"cli\""));

        let result_json = serde_json::to_string(&result).unwrap();
        let decoded_result: TranscriptionResult = serde_json::from_str(&result_json).unwrap();
        assert_eq!(decoded_result, result);
    }

    #[test]
    fn canonical_job_creation_freezes_selection_and_initializes_stages() {
        let selection = sample_selection(CreatedFrom::App);
        let setup = ContentSetupV1::unavailable(crate::content_results::TargetUnavailableV1 {
            code: crate::content_results::TargetUnavailableCodeV1::ConnectionMissing,
            message: "本地 AI 未运行".into(),
            invalid_field: None,
        });
        let job = Job::from_creation(JobCreationInput::new(
            Uuid::new_v4(),
            "BV1frozen".into(),
            2,
            selection.clone(),
            RetentionPolicy::KeepAll,
            setup.clone(),
        ));
        assert_eq!(job.transcription_selection.as_ref(), Some(&selection));
        assert_eq!(job.retention, RetentionPolicy::KeepAll);
        assert_eq!(job.content_setup, Some(setup));
        assert_eq!(job.transcription_selection_marker(), "recorded");
        for stage in pipeline_stages() {
            assert_eq!(job.stage_state(stage), StageState::Pending);
        }
    }

    #[test]
    fn app_and_cli_creation_share_the_same_job_semantics() {
        let app_job = Job::from_creation(JobCreationInput::new(
            Uuid::new_v4(),
            "BV1same".into(),
            2,
            sample_selection(CreatedFrom::App),
            RetentionPolicy::KeepAll,
            ContentSetupV1::disabled(),
        ));
        let cli_job = Job::from_creation(JobCreationInput::new(
            Uuid::new_v4(),
            "BV1same".into(),
            2,
            sample_selection(CreatedFrom::Cli),
            RetentionPolicy::KeepAll,
            ContentSetupV1::disabled(),
        ));

        assert_eq!(app_job.bvid, cli_job.bvid);
        assert_eq!(app_job.page, cli_job.page);
        assert_eq!(app_job.retention, cli_job.retention);
        assert_eq!(app_job.stages, cli_job.stages);
        let app_selection = app_job.transcription_selection.as_ref().unwrap();
        let cli_selection = cli_job.transcription_selection.as_ref().unwrap();
        assert_eq!(app_selection.runtime_source, cli_selection.runtime_source);
        assert_eq!(app_selection.runtime_project, cli_selection.runtime_project);
        assert_eq!(
            app_selection.runtime_data_dir,
            cli_selection.runtime_data_dir
        );
        assert_eq!(
            app_selection.runtime_identity,
            cli_selection.runtime_identity
        );
        assert_eq!(app_selection.runtime_backend, cli_selection.runtime_backend);
        assert_eq!(
            app_selection.model_description,
            cli_selection.model_description
        );
        assert_eq!(
            app_selection.requested_language,
            cli_selection.requested_language
        );
        assert_eq!(app_selection.created_from, CreatedFrom::App);
        assert_eq!(cli_selection.created_from, CreatedFrom::Cli);
    }

    #[test]
    fn old_queue_and_state_entries_are_readable_without_config_backfill() {
        let id = Uuid::new_v4();
        let old_json = format!(
            r#"{{"id":"{}","bvid":"BV1legacy","page":1,"title":"legacy","stage":"queued","status":"queued","stage_progress":0,"error":null,"work_dir":null,"stages":{{}},"speaker_map":{{}}}}"#,
            id
        );
        let queue_dir = std::env::temp_dir().join(format!("bi2read-queue-fixture-{id}"));
        std::fs::remove_dir_all(&queue_dir).ok();
        std::fs::create_dir_all(&queue_dir).unwrap();
        let queue_path = queue_dir.join("queue.json");
        std::fs::write(&queue_path, format!(r#"{{"jobs":[{}]}}"#, old_json)).unwrap();

        let queue = Queue::load_from(&queue_path).unwrap();
        let job = &queue.jobs[0];
        assert_eq!(job.id, id);
        assert_eq!(job.transcription_selection, None);
        assert_eq!(job.transcription_selection_marker(), LEGACY_UNRECORDED);
        assert!(job.requires_transcription_rebuild());

        let state_dir = queue_dir.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("state.json"), old_json).unwrap();
        let state = load_job_state(&state_dir).unwrap();
        assert_eq!(state.transcription_selection_marker(), LEGACY_UNRECORDED);
        std::fs::remove_dir_all(queue_dir).ok();
    }

    #[test]
    fn cancellation_capability_and_transition_share_one_status_matrix() {
        let now = chrono::Utc::now();
        for (status, can_cancel, transition, final_status) in [
            (
                JobStatus::Queued,
                true,
                CancellationTransition::Immediate,
                JobStatus::Cancelled,
            ),
            (
                JobStatus::Running,
                true,
                CancellationTransition::Cooperative,
                JobStatus::Cancelling,
            ),
            (
                JobStatus::Paused,
                true,
                CancellationTransition::Immediate,
                JobStatus::Cancelled,
            ),
            (
                JobStatus::NeedsUserAction,
                true,
                CancellationTransition::Immediate,
                JobStatus::Cancelled,
            ),
            (
                JobStatus::WaitingForDrive,
                true,
                CancellationTransition::Immediate,
                JobStatus::Cancelled,
            ),
            (
                JobStatus::Cancelling,
                false,
                CancellationTransition::AlreadyCancelling,
                JobStatus::Cancelling,
            ),
            (
                JobStatus::Completed,
                false,
                CancellationTransition::Unavailable,
                JobStatus::Completed,
            ),
            (
                JobStatus::Failed,
                false,
                CancellationTransition::Unavailable,
                JobStatus::Failed,
            ),
            (
                JobStatus::Cancelled,
                false,
                CancellationTransition::Unavailable,
                JobStatus::Cancelled,
            ),
        ] {
            let mut job = Job::new(Uuid::new_v4(), "BV1cancel".into(), 1);
            job.status = status;
            job.stage = Stage::Transcribe;
            assert_eq!(JobCapabilities::from_job(&job).can_cancel, can_cancel);
            assert_eq!(job.request_cancellation(now), transition);
            assert_eq!(job.status, final_status);
            if transition == CancellationTransition::Immediate {
                assert_eq!(job.stage, Stage::Cancelled);
                assert_eq!(job.finished_at, Some(now));
                assert!(!JobCapabilities::from_job(&job).can_cancel);
                assert!(JobCapabilities::from_job(&job).can_delete);
            }
        }
    }

    #[test]
    fn cancelled_legacy_job_persists_reloads_and_does_not_resurrect() {
        let id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("bi2read-cancel-reload-{id}"));
        std::fs::create_dir_all(&root).unwrap();
        let queue_path = root.join("queue.json");
        let mut job = Job::new(id, "BV1legacy-cancel".into(), 1);
        job.status = JobStatus::NeedsUserAction;
        job.stage = Stage::NeedsUserAction;
        job.error = Some(format!("{LEGACY_UNRECORDED}: rebuild required"));
        assert_eq!(
            job.request_cancellation(chrono::Utc::now()),
            CancellationTransition::Immediate
        );
        let queue = Queue { jobs: vec![job] };
        std::fs::write(&queue_path, serde_json::to_vec_pretty(&queue).unwrap()).unwrap();

        let mut reloaded = Queue::load_from(&queue_path).unwrap();
        assert_eq!(reloaded.jobs[0].status, JobStatus::Cancelled);
        assert!(reloaded.jobs[0].transcription_selection.is_none());
        recover(&mut reloaded);
        assert_eq!(reloaded.jobs[0].status, JobStatus::Cancelled);
        assert_eq!(reloaded.jobs[0].stage, Stage::Cancelled);
        assert!(JobCapabilities::from_job(&reloaded.jobs[0]).can_delete);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn legacy_job_marker_is_absent_while_new_job_freezes_disabled_setup() {
        let legacy = Job::new(Uuid::new_v4(), "BV1legacy-marker".into(), 1);
        assert!(legacy.content_setup.is_none());

        let created = Job::from_creation(JobCreationInput::new(
            Uuid::new_v4(),
            "BV1new-marker".into(),
            1,
            sample_selection(CreatedFrom::Cli),
            RetentionPolicy::Recommended,
            ContentSetupV1::disabled(),
        ));
        assert_eq!(created.content_setup, Some(ContentSetupV1::disabled()));
        let bytes = serde_json::to_vec(&created).unwrap();
        let reloaded: Job = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reloaded.content_setup, created.content_setup);
    }

    #[test]
    fn persisted_selection_survives_config_changes_and_reload() {
        let selection = sample_selection(CreatedFrom::Cli);
        let job = Job::from_creation(JobCreationInput::new(
            Uuid::new_v4(),
            "BV1stable".into(),
            1,
            selection.clone(),
            RetentionPolicy::Recommended,
            ContentSetupV1::disabled(),
        ));
        let queue_path =
            std::env::temp_dir().join(format!("bi2read-selection-freeze-{}.json", job.id));
        let queue = Queue { jobs: vec![job] };
        std::fs::write(&queue_path, serde_json::to_vec_pretty(&queue).unwrap()).unwrap();

        // A later settings edit is input for new Jobs only. Loading an
        // existing queue must not consult it or alter the frozen selection.
        let config = crate::config::Config {
            runtime_project: Some(PathBuf::from("/tmp/changed-runtime")),
            runtime_data_dir: PathBuf::from("/tmp/changed-runtime-data"),
            default_retention: RetentionPolicy::KeepAll,
            ..crate::config::Config::default()
        };

        let reloaded = Queue::load_from(&queue_path).unwrap();
        assert_eq!(reloaded.jobs[0].transcription_selection, Some(selection));
        assert_eq!(reloaded.jobs[0].retention, RetentionPolicy::Recommended);
        assert_eq!(config.default_retention, RetentionPolicy::KeepAll);
        std::fs::remove_file(queue_path).ok();
    }

    #[test]
    fn legacy_recovery_requires_explicit_rebuild_but_completed_evidence_stays_readable() {
        let mut legacy = Job::new(Uuid::new_v4(), "BV1legacy".into(), 1);
        legacy.status = JobStatus::Queued;
        let mut queue = Queue { jobs: vec![legacy] };
        recover(&mut queue);
        assert_eq!(queue.jobs[0].status, JobStatus::NeedsUserAction);
        assert_eq!(queue.jobs[0].stage, Stage::NeedsUserAction);
        assert!(queue.jobs[0]
            .error
            .as_deref()
            .is_some_and(|message| message.contains(LEGACY_UNRECORDED)));

        let id = Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("bi2read-legacy-complete-{id}"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("full.md"),
            format!("<!-- job-id: {id} -->\nlegacy evidence"),
        )
        .unwrap();
        let mut completed = Job::new(id, "BV1legacy".into(), 1);
        completed.status = JobStatus::Completed;
        completed.stage = Stage::Completed;
        completed.stage_progress = 100;
        completed.work_dir = Some(dir.clone());
        completed.final_output_dir = Some(dir.clone());
        completed.set_stage_state(Stage::FinalDocument, StageState::Completed);
        let mut queue = Queue {
            jobs: vec![completed],
        };
        recover(&mut queue);
        assert_eq!(queue.jobs[0].status, JobStatus::Completed);
        assert!(queue.jobs[0]
            .work_dir
            .as_ref()
            .unwrap()
            .join("full.md")
            .is_file());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recovery_preserves_recorded_selection_for_retry() {
        let selection = sample_selection(CreatedFrom::App);
        let mut job = Job::from_creation(JobCreationInput::new(
            Uuid::new_v4(),
            "BV1retry".into(),
            1,
            selection.clone(),
            RetentionPolicy::Recommended,
            ContentSetupV1::disabled(),
        ));
        job.status = JobStatus::Running;
        job.stage = Stage::Transcribe;
        job.set_stage_state(Stage::Transcribe, StageState::Running);
        let mut queue = Queue { jobs: vec![job] };

        recover(&mut queue);

        // A Transcribe stage left Running is a Runtime-side interruption
        // (OOM kill, lost Docker engine, App restart) that must converge to
        // Failed rather than being silently requeued (see
        // `recover_marks_interrupted_transcribe_as_failed`); the recorded
        // selection must still survive so the user can retry with it.
        assert_eq!(
            queue.jobs[0].transcription_selection.as_ref(),
            Some(&selection)
        );
        assert_eq!(queue.jobs[0].status, JobStatus::Failed);
        assert_eq!(
            queue.jobs[0].stage_state(Stage::Transcribe),
            StageState::Failed
        );
        assert!(JobCapabilities::from_job(&queue.jobs[0]).can_retry);
    }

    #[test]
    fn stage_name_roundtrip() {
        for s in pipeline_stages() {
            assert_eq!(Stage::from_name(s.name()), Some(s));
        }
    }

    #[test]
    fn job_status_name_is_snake_case_machine_key() {
        // `JobStatus::name()` feeds the Slint status-key priority chain
        // (issue 13); every variant must have a distinct snake_case key so
        // the UI never falls back to matching Chinese `label()` strings.
        let all = [
            JobStatus::Queued,
            JobStatus::Running,
            JobStatus::Paused,
            JobStatus::NeedsUserAction,
            JobStatus::WaitingForDrive,
            JobStatus::Failed,
            JobStatus::Cancelling,
            JobStatus::Cancelled,
            JobStatus::Completed,
        ];
        let names: Vec<&str> = all.iter().map(|s| s.name()).collect();
        for n in &names {
            assert!(
                n.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "not snake_case: {n}"
            );
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate JobStatus name key");
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
        let dir = std::env::temp_dir().join(format!("bi2read-jobs-test-{}", Uuid::new_v4()));
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
        let dir = std::env::temp_dir().join(format!("bi2read-recover-{}", id));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut job = Job::new(id, "BV1test".into(), 1);
        job.transcription_selection = Some(sample_selection(CreatedFrom::App));
        job.content_setup = Some(ContentSetupV1::disabled());
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
    fn runtime_identity_change_opens_rebuild_but_legacy_rule_still_holds() {
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        job.transcription_selection = Some(sample_selection(CreatedFrom::App));
        job.content_setup = Some(ContentSetupV1::disabled());
        job.status = JobStatus::NeedsUserAction;
        job.stage = Stage::NeedsUserAction;
        job.error = Some(format!(
            "{RUNTIME_IDENTITY_CHANGED}: 任务冻结的 Runtime 已改变"
        ));
        assert!(job.requires_runtime_reselection());
        assert!(JobCapabilities::from_job(&job).can_rebuild);

        // 同样的错误码在 Failed 状态下不开放重建：只有等待用户操作才是重建入口。
        job.status = JobStatus::Failed;
        assert!(!job.requires_runtime_reselection());
        assert!(!JobCapabilities::from_job(&job).can_rebuild);

        // 其他等待用户操作的原因（如 Docker 未运行）不冒充身份失配。
        job.status = JobStatus::NeedsUserAction;
        job.error = Some("docker not running".into());
        assert!(!JobCapabilities::from_job(&job).can_rebuild);
    }

    #[test]
    fn recover_marks_interrupted_transcribe_as_failed() {
        let id = Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("bi2read-recover-transcribe-{}", id));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut job = Job::new(id, "BV1test".into(), 1);
        job.transcription_selection = Some(sample_selection(CreatedFrom::App));
        job.content_setup = Some(ContentSetupV1::disabled());
        job.work_dir = Some(dir.clone());
        job.set_stage_state(Stage::DownloadAudio, StageState::Completed);
        job.set_stage_state(Stage::Metadata, StageState::Completed);
        job.set_stage_state(Stage::NormalizeAudio, StageState::Completed);
        job.set_stage_state(Stage::Transcribe, StageState::Running);
        job.status = JobStatus::Running;
        job.stage = Stage::Transcribe;
        let mut q = Queue { jobs: vec![job] };
        let reports = recover(&mut q);
        assert_eq!(reports.len(), 1);
        let job = &q.jobs[0];
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(job.stage, Stage::Transcribe);
        assert_eq!(job.stage_state(Stage::Transcribe), StageState::Failed);
        let error = job.error.as_deref().unwrap_or("");
        assert!(error.contains("未自动重跑"), "unexpected error: {error}");
        // A Runtime failure like this must stay user-retryable.
        assert!(JobCapabilities::from_job(job).can_retry);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recover_keeps_valid_completed() {
        let id = Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("bi2read-recover2-{}", id));
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
        let home = std::env::temp_dir().join(format!("bi2read-recover-home-{}", Uuid::new_v4()));
        let app_dir = home
            .join("Library")
            .join("Application Support")
            .join("bi2read");
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
        let dir = std::env::temp_dir().join(format!("bi2read-completed-{id}"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut job = Job::new(id, "BV1test".into(), 1);
        job.transcription_selection = Some(sample_selection(CreatedFrom::App));
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
        std::fs::write(
            dir.join("transcript.raw.json"),
            br#"[{"id":"u0001","text":"fixture","start_ms":0,"end_ms":1,"speaker_id":0}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("transcript.raw.md"),
            format!("<!-- job-id: {} -->\nraw", job.id),
        )
        .unwrap();
        std::fs::write(dir.join("transcript.readable.md"), "readable").unwrap();
        // Deliberately keep a same-named v0.5 file on this legacy fixture:
        // the absent Job marker must remain authoritative and ignore it.
        std::fs::write(dir.join("content-current.v1.json"), "current fixture").unwrap();
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
                    "source.audio",
                    "normalized.wav",
                    "transcript.raw.json",
                    "transcript.readable.md",
                    "metadata.json",
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
        assert!(!dir.join("transcript.readable.md").exists());
        assert!(dir.join("content-current.v1.json").exists());
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
        // Legacy DocumentsOnly removed raw/metadata, so rebuilding a missing
        // final document starts at the earliest stage that can recreate them.
        assert_eq!(recovered.stage, Stage::DownloadAudio);
        for stage in [
            Stage::DownloadAudio,
            Stage::NormalizeAudio,
            Stage::Transcribe,
            Stage::RawDocument,
            Stage::ReadableDocument,
            Stage::FinalDocument,
            Stage::Cleanup,
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
    fn v05_terminal_missing_or_corrupt_current_stays_terminal_for_raw_only_repair() {
        for current in [None, Some(br"not-json".as_slice())] {
            let id = Uuid::new_v4();
            let dir = std::env::temp_dir().join(format!("bi2read-v05-raw-only-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            let mut job = Job::new(id, "BV1v05rawonly".into(), 1);
            job.content_setup = Some(ContentSetupV1::disabled());
            job.work_dir = Some(dir.clone());
            job.final_output_dir = Some(dir.clone());
            job.cid = Some(7);
            job.duration_ms = Some(1000);
            job.status = JobStatus::Completed;
            job.stage = Stage::Completed;
            job.stage_progress = 100;
            for stage in pipeline_stages() {
                job.set_stage_state(
                    stage,
                    if stage == Stage::Screenshots {
                        StageState::Skipped
                    } else {
                        StageState::Completed
                    },
                );
            }
            std::fs::write(
                dir.join("transcript.raw.json"),
                br#"[{"id":"u1","text":"raw","start_ms":0,"end_ms":1,"speaker_id":0}]"#,
            )
            .unwrap();
            if let Some(current) = current {
                std::fs::write(dir.join("content-current.v1.json"), current).unwrap();
            }
            let mut queue = Queue { jobs: vec![job] };
            let reports = recover(&mut queue);
            assert!(reports[0].revalidated_stages.is_empty());
            assert_eq!(queue.jobs[0].status, JobStatus::Completed);
            assert_eq!(queue.jobs[0].stage, Stage::Completed);
            assert!(matches!(
                crate::content_results::current(&queue.jobs[0]),
                Ok(crate::content_results::ContentViewV1::RawOnly { .. })
            ));
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn recover_missing_final_requeues_from_earliest_rebuildable_stage() {
        let (job, dir) = completed_job_with_retained_artifacts(RetentionPolicy::Recommended);
        std::fs::remove_file(dir.join("full.md")).unwrap();
        std::fs::write(
            dir.join("transcript.raw.json"),
            r#"[{"id":"u0001","text":"fixture","start_ms":0,"end_ms":1,"speaker_id":0}]"#,
        )
        .unwrap();
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
        let dir = std::env::temp_dir().join(format!("bi2read-caps-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("transcript.raw.json"),
            br#"[{"id":"u0001","text":"fixture","start_ms":0,"end_ms":1,"speaker_id":0}]"#,
        )
        .unwrap();
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
        job.transcription_selection = Some(sample_selection(CreatedFrom::Cli));
        job.status = JobStatus::Failed;
        let caps = JobCapabilities::from_job(&job);
        assert!(!caps.can_cancel);
        assert!(caps.can_retry);
    }

    #[test]
    fn v05_raw_only_failed_job_uses_repair_instead_of_generic_retry() {
        let id = Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("bi2read-caps-raw-only-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("transcript.raw.json"),
            br#"[{"id":"u1","text":"raw","start_ms":0,"end_ms":1,"speaker_id":0}]"#,
        )
        .unwrap();
        let mut job = Job::new(id, "BV1rawonly".into(), 1);
        job.content_setup = Some(ContentSetupV1::disabled());
        job.transcription_selection = Some(sample_selection(CreatedFrom::Cli));
        job.work_dir = Some(dir.clone());
        job.cid = Some(7);
        job.duration_ms = Some(1000);
        job.status = JobStatus::Failed;
        let caps = JobCapabilities::from_job(&job);
        assert!(!caps.can_retry);
        assert!(caps.can_open_document);
        assert!(!caps.can_edit_speakers);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn v05_completed_job_with_missing_or_corrupt_evidence_has_no_result_entry() {
        for raw in [
            None,
            Some(b"not-json".as_slice()),
            Some(
                br#"[{"id":"u1","text":"raw","start_ms":0,"end_ms":5000,"speaker_id":0}]"#
                    .as_slice(),
            ),
        ] {
            let id = Uuid::new_v4();
            let dir = std::env::temp_dir().join(format!("bi2read-caps-invalid-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            if let Some(raw) = raw {
                std::fs::write(dir.join("transcript.raw.json"), raw).unwrap();
            }
            let mut job = Job::new(id, "BV1invalid".into(), 1);
            job.content_setup = Some(ContentSetupV1::disabled());
            job.work_dir = Some(dir.clone());
            job.cid = Some(7);
            job.duration_ms = Some(1000);
            job.status = JobStatus::Completed;

            let caps = JobCapabilities::from_job(&job);
            assert!(!caps.can_open_document);
            assert!(!caps.can_edit_speakers);
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn capabilities_for_cancelled_job() {
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        job.status = JobStatus::Cancelled;
        let caps = JobCapabilities::from_job(&job);
        assert!(!caps.can_cancel);
        assert!(!caps.can_retry);
        assert!(caps.can_delete);
    }

    #[test]
    fn delete_capability_matches_terminal_statuses_only() {
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        for status in [
            JobStatus::Queued,
            JobStatus::Running,
            JobStatus::Cancelling,
            JobStatus::NeedsUserAction,
            JobStatus::WaitingForDrive,
        ] {
            job.status = status;
            assert!(
                !JobCapabilities::from_job(&job).can_delete,
                "{status:?} must not be deletable"
            );
        }
        for status in [
            JobStatus::Completed,
            JobStatus::Failed,
            JobStatus::Cancelled,
        ] {
            job.status = status;
            assert!(
                JobCapabilities::from_job(&job).can_delete,
                "{status:?} must be deletable"
            );
        }
    }

    #[test]
    fn delete_job_artifacts_removes_work_and_output_dirs_across_retentions() {
        for retention in [
            RetentionPolicy::KeepAll,
            RetentionPolicy::Recommended,
            RetentionPolicy::DocumentsOnly,
        ] {
            let (mut job, work_dir) = completed_job_with_retained_artifacts(retention);
            // Give the job a separate per-job output directory, as the pipeline
            // does when `output_dir` is configured.
            let out_dir = std::env::temp_dir().join(format!(
                "bi2read-delete-out-{}-{}",
                job.id,
                retention.label()
            ));
            std::fs::remove_dir_all(&out_dir).ok();
            std::fs::create_dir_all(&out_dir).unwrap();
            std::fs::write(out_dir.join("full.md"), "final document").unwrap();
            job.final_output_dir = Some(out_dir.clone());

            delete_job_artifacts(&job).unwrap();

            assert!(
                !work_dir.exists(),
                "work dir must be removed ({retention:?})"
            );
            assert!(
                !out_dir.exists(),
                "output dir must be removed ({retention:?})"
            );
        }
    }

    #[test]
    fn delete_job_artifacts_treats_missing_dirs_as_success() {
        let id = Uuid::new_v4();
        let mut job = Job::new(id, "BV1test".into(), 1);
        job.status = JobStatus::Cancelled;
        job.work_dir = Some(std::env::temp_dir().join(format!("bi2read-absent-{id}")));
        job.final_output_dir = Some(std::env::temp_dir().join(format!("bi2read-absent-out-{id}")));
        // External drive unplugged: neither directory exists.
        delete_job_artifacts(&job).unwrap();
    }

    #[test]
    fn delete_job_artifacts_never_removes_shared_output_root() {
        let (job, work_dir) = completed_job_with_retained_artifacts(RetentionPolicy::KeepAll);
        // A final_output_dir without full.md is not recognizable as a
        // job-owned directory (e.g. a legacy shared root) and must survive.
        let shared_root = std::env::temp_dir().join(format!("bi2read-shared-{}", job.id));
        std::fs::remove_dir_all(&shared_root).ok();
        std::fs::create_dir_all(&shared_root).unwrap();
        std::fs::write(shared_root.join("other.txt"), "keep me").unwrap();
        let mut job = job;
        job.final_output_dir = Some(shared_root.clone());

        delete_job_artifacts(&job).unwrap();

        assert!(!work_dir.exists());
        assert!(shared_root.join("other.txt").exists());
        std::fs::remove_dir_all(shared_root).ok();
    }

    #[test]
    fn deleted_job_does_not_resurrect_after_reload() {
        let (mut terminal, work_dir) =
            completed_job_with_retained_artifacts(RetentionPolicy::Recommended);
        terminal.status = JobStatus::Cancelled;
        let live = {
            let mut job = Job::new(Uuid::new_v4(), "BV1live".into(), 1);
            job.status = JobStatus::Running;
            job
        };
        let queue_path =
            std::env::temp_dir().join(format!("bi2read-delete-{}.json", Uuid::new_v4()));
        {
            let mut queue = Queue {
                jobs: vec![terminal.clone(), live.clone()],
            };
            // Deletion flow: artifacts first, then drop the record and persist.
            delete_job_artifacts(&terminal).unwrap();
            queue.jobs.retain(|job| job.id != terminal.id);
            std::fs::write(&queue_path, serde_json::to_vec_pretty(&queue).unwrap()).unwrap();
        }

        // Startup reload (what `Queue::load` does) must not bring the job back.
        let reloaded = Queue::load_from(&queue_path).unwrap();
        assert_eq!(reloaded.jobs.len(), 1);
        assert_eq!(reloaded.jobs[0].id, live.id);
        assert!(!work_dir.exists());
        std::fs::remove_file(queue_path).ok();
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
