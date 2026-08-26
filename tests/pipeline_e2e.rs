//! Fixture-driven full-pipeline integration tests: happy paths, scripted
//! failure + production retry, crash recovery, and language-freeze regression.
//!
//! `FixtureDeps` replaces the three external side effects of the pipeline
//! (metadata, audio download, transcription) while every other stage —
//! including the real ffmpeg normalization subprocess — runs unchanged through
//! [`bimyscribe::pipeline::run_job_with_deps`]. No network, Docker, uv, or
//! external drive is required.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use slint::Weak;
use uuid::Uuid;

use bimyscribe::cancel::CancellationToken;
use bimyscribe::config::Config;
use bimyscribe::funasr::{
    parse_transcript_outcome, FunasrError, TranscribeOutcome, TranscribeRequest,
};
use bimyscribe::jobs::SourceLanguage;
use bimyscribe::jobs::{
    self, CreatedFrom, Job, JobStatus, RetentionPolicy, RuntimeBackend, RuntimeSource, Stage,
    StageState, TranscriptionSelection,
};
use bimyscribe::pipeline::{self, PipelineDeps, PipelineError};
use bimyscribe::App;

const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const FIXTURE_WAV: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/audio/sample.wav"
);
const FIXTURE_RUNTIME_IDENTITY: &str = "contract-v2:fixture:e2e";

/// HOME is process-global; serialize the two scenarios (and any other test
/// that swaps it) so they never observe each other's sandbox.
static HOME_LOCK: Mutex<()> = Mutex::new(());

// ---- Fixture dependency implementation ----

/// Fixture side-effect provider: canned metadata, a local WAV copy, and a
/// template-based fake Runtime that reproduces the exact file shape of
/// `funasr::run` (normalized.json under `funasr/output/`, then
/// `transcript.raw.json` in the job dir, both via the real parser).
struct FixtureDeps {
    language: SourceLanguage,
    title: &'static str,
    template: &'static str,
    /// Programmable switch: the first N transcribe calls return a scripted
    /// error (scenario 3); subsequent calls behave normally.
    transcribe_failures: usize,
    /// How many times `transcribe` has actually been invoked. Scenarios 4 and 5
    /// assert on it to prove completed stages are skipped on resume.
    transcribe_calls: Arc<AtomicUsize>,
}

impl FixtureDeps {
    fn zh() -> Self {
        Self {
            language: SourceLanguage::Zh,
            title: "【集成测试】中文示例视频",
            template: "transcript.zh.json",
            transcribe_failures: 0,
            transcribe_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn en() -> Self {
        Self {
            language: SourceLanguage::En,
            title: "Integration Test English Sample",
            template: "transcript.en.json",
            transcribe_failures: 0,
            transcribe_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Fail the first `n` transcribe calls with a scripted error.
    fn with_transcribe_failures(mut self, n: usize) -> Self {
        self.transcribe_failures = n;
        self
    }

    fn with_title(mut self, title: &'static str) -> Self {
        self.title = title;
        self
    }

    fn transcribe_call_count(&self) -> usize {
        self.transcribe_calls.load(Ordering::SeqCst)
    }

    fn template_path(&self) -> PathBuf {
        Path::new(FIXTURE_ROOT).join(self.template)
    }
}

impl PipelineDeps for FixtureDeps {
    fn fetch_metadata(
        &self,
        bvid: &str,
        _page: Option<u32>,
    ) -> Result<bimyscribe::bilibili::Metadata, PipelineError> {
        Ok(bimyscribe::bilibili::Metadata {
            bvid: bvid.to_string(),
            cid: 4242,
            title: self.title.to_string(),
            part_title: Some("fixture-part".into()),
            up_name: "fixture-up".into(),
            duration_ms: 10_000,
        })
    }

    fn fetch_audio(
        &self,
        _bvid: &str,
        _cid: u64,
        dest: &Path,
        _progress: &dyn Fn(u64, u64),
        _cancel_token: &CancellationToken,
    ) -> Result<(), PipelineError> {
        std::fs::copy(FIXTURE_WAV, dest)?;
        Ok(())
    }

    fn transcribe(&self, request: TranscribeRequest<'_>) -> Result<TranscribeOutcome, FunasrError> {
        let call = self.transcribe_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call <= self.transcribe_failures {
            return Err(FunasrError::Validation(
                "scripted transcribe failure".into(),
            ));
        }
        // The Job's frozen language choice must reach the Runtime untouched.
        assert_eq!(
            request.selection.requested_language, self.language,
            "Job 冻结的 transcription selection 语言必须贯穿到 Runtime 请求"
        );
        // Normalize must have produced a real, non-empty audio file.
        let audio = std::fs::metadata(request.normalized_wav).map_err(FunasrError::Read)?;
        assert!(audio.len() > 0, "normalized.wav 必须存在且非空");

        // Mirror the real adapter's output layout: normalized.json under
        // funasr/output/, parsed by the real schema validator.
        let output_dir = request.job_dir.join("funasr").join("output");
        std::fs::create_dir_all(&output_dir).map_err(FunasrError::Read)?;
        let normalized_json = output_dir.join("normalized.json");
        std::fs::copy(self.template_path(), &normalized_json).map_err(FunasrError::Read)?;
        let outcome = parse_transcript_outcome(&normalized_json, request.duration_ms)?;
        // Same identity guard as funasr::run.
        if outcome
            .result
            .reported_runtime_identity
            .as_deref()
            .is_some_and(|reported| reported != request.selection.runtime_identity)
        {
            return Err(FunasrError::IdentityChanged);
        }

        // Same persisted file shape as funasr::run.
        let dest = request.job_dir.join("transcript.raw.json");
        let data = serde_json::to_vec_pretty(&outcome.utterances).map_err(FunasrError::Parse)?;
        bimyscribe::jobs::atomic_write(&dest, &data)
            .map_err(|error| FunasrError::Io(error.to_string()))?;
        Ok(outcome)
    }
}

// ---- Sandbox and headless-UI helpers ----

struct Sandbox {
    previous_home: Option<OsString>,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bimyscribe-pipeline-e2e-{}", Uuid::new_v4()));
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(root.join("working")).unwrap();
        std::fs::create_dir_all(root.join("output")).unwrap();
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        Self {
            previous_home,
            root,
        }
    }

    fn working_root(&self) -> PathBuf {
        self.root.join("working")
    }

    fn output_root(&self) -> PathBuf {
        self.root.join("output")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous_home {
            std::env::set_var("HOME", previous);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Headless UI degradation. `e2e_smoke` takes a weak handle from a live App
/// and drops the strong one; in `cargo test` that is not an option: on macOS
/// winit panics ("EventLoop must be created on the main thread") when an App
/// is created on a test worker thread, so even probing `App::new()` would
/// abort the test instead of returning `Err`. An empty `Weak<App>` is the
/// equivalent degraded state — every `ui_bridge` upgrade becomes a no-op and
/// the pipeline runs identically without UI updates.
fn headless_weak() -> Weak<App> {
    Weak::default()
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

// ---- Shared scenario runner ----

/// A queued KeepAll job whose frozen transcription selection points at the
/// fixture Runtime identity, exactly like the App would create it.
fn fixture_job(sandbox_root: &Path, language: SourceLanguage) -> Job {
    let mut job = Job::new(Uuid::new_v4(), "BV1fixtureE2E".into(), 1);
    job.source_url = Some("https://www.bilibili.com/video/BV1fixtureE2E".into());
    job.retention = RetentionPolicy::KeepAll;
    job.transcription_selection = Some(TranscriptionSelection::new(
        RuntimeSource::External,
        sandbox_root.join("fixture-runtime-project"),
        sandbox_root.join("fixture-runtime-data"),
        FIXTURE_RUNTIME_IDENTITY.into(),
        RuntimeBackend::DockerCompose,
        Some("fixture model".into()),
        language,
        CreatedFrom::Cli,
    ));
    for stage in jobs::pipeline_stages() {
        job.set_stage_state(stage, StageState::Pending);
    }
    mark_v05_unavailable(job)
}

fn mark_v05_unavailable(job: Job) -> Job {
    let mut value = serde_json::to_value(job).unwrap();
    value["content_setup"] = serde_json::json!({
        "schema_version": 1,
        "enhancements_requested": true,
        "recipe_set_version": 1,
        "initial_target": {
            "kind": "unavailable",
            "code": "connection_missing",
            "message": "本地 AI 未运行",
            "invalid_field": null
        }
    });
    serde_json::from_value(value).unwrap()
}

fn run_happy_path(deps: FixtureDeps) {
    let _home_lock = HOME_LOCK.lock().unwrap();
    if !ffmpeg_available() {
        eprintln!("skipping pipeline_e2e: ffmpeg 不在 PATH 上，无法执行真实标准化阶段");
        return;
    }
    let sandbox = Sandbox::new();
    let cfg = Config {
        working_dir: sandbox.working_root(),
        output_dir: sandbox.output_root(),
        ..Config::default()
    };

    // Sentinel: the production queue.json must stay byte-identical.
    let production_queue = bimyscribe::jobs::queue_path().unwrap();
    std::fs::create_dir_all(production_queue.parent().unwrap()).unwrap();
    let sentinel = br#"{"production":"queue must stay byte-identical"}"#;
    std::fs::write(&production_queue, sentinel).unwrap();

    let mut job = fixture_job(&sandbox.root, deps.language);

    let weak = headless_weak();
    let cancel_token = CancellationToken::new();
    pipeline::run_job_with_deps(&mut job, &cfg, &weak, &cancel_token, &deps)
        .expect("happy path 全阶段应成功");

    // ---- All structural stages Completed (Screenshots Skipped by design). ----
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.stage, Stage::Completed);
    assert!(jobs::pipeline_stages().into_iter().all(|stage| {
        matches!(
            job.stage_state(stage),
            StageState::Completed | StageState::Skipped
        )
    }));
    assert_eq!(job.stage_state(Stage::Screenshots), StageState::Skipped);

    let work_dir = job.work_dir.clone().expect("工作目录应已建立");
    assert_eq!(job.title, deps.title);
    assert_eq!(job.part_title.as_deref(), Some("fixture-part"));
    let metadata: serde_json::Value = serde_json::from_slice(
        &std::fs::read(work_dir.join("metadata.json")).expect("metadata.json 应存在"),
    )
    .unwrap();
    assert_eq!(metadata["title"].as_str(), Some(deps.title));
    assert_eq!(metadata["part_title"].as_str(), Some("fixture-part"));

    // ---- raw.json: parsed via the real loader, contents from the template. ----
    let utterances = pipeline::load_utterances(&work_dir.join("transcript.raw.json"))
        .expect("transcript.raw.json 应存在且可解析");
    assert_eq!(utterances.len(), 3);
    assert_eq!(utterances[0].speaker_id, 1);
    assert_eq!(utterances[1].speaker_id, 2);
    assert_eq!(utterances[2].speaker_id, 0, "null speaker 必须映射为 0");

    // ---- raw.md: header, job id, first utterance. ----
    let raw_md = std::fs::read_to_string(work_dir.join("transcript.raw.md")).unwrap();
    assert!(raw_md.contains("# 原始逐字稿"));
    assert!(raw_md.contains(&job.id.to_string()));
    assert!(raw_md.contains(&utterances[0].text));

    // ---- full.md in the configured output dir: title, timestamp, speaker. ----
    let final_dir = job
        .final_output_dir
        .clone()
        .expect("final_output_dir 应记录");
    let full_md = std::fs::read_to_string(final_dir.join("full.md")).unwrap();
    assert!(
        full_md.starts_with(&format!("# {}\n", deps.title)),
        "full.md 应以标题开头"
    );
    assert!(full_md.contains(&job.id.to_string()));
    assert!(
        full_md.contains("**[Speaker 1]**"),
        "full.md 应包含 speaker 标签"
    );
    assert!(full_md.contains("**[Speaker 2]**"));
    assert!(full_md.contains("[00:00:00]("), "full.md 应包含时间戳链接");
    assert!(full_md.contains(&utterances[0].text));
    assert!(full_md.contains(&utterances[1].text));
    assert!(work_dir.join("content-current.v1.json").is_file());
    assert!(work_dir.join("transcript.readable.md").is_file());
    assert!(
        !work_dir.join("full.md").exists(),
        "v0.5 外部输出不应维护 work-dir full.md 镜像"
    );
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(work_dir.join("content-current.v1.json")).unwrap())
            .unwrap();
    assert_eq!(current["source_context"]["part_title"], "fixture-part");
    assert!(current["slots"]["faithful_text"]["current"].is_object());
    for kind in ["default_summary", "highlights", "chapters"] {
        assert!(
            current["slots"][kind]["last_failure"].is_object(),
            "Unavailable setup should settle {kind} independently"
        );
    }
    // Language identity reported by the fixture Runtime flows into the doc.
    let reported = job
        .transcription_result
        .as_ref()
        .and_then(|result| result.reported_language)
        .expect("transcription_result 应记录回报语言");
    assert_eq!(reported, deps.language);

    // ---- Persisted state.json agrees with the in-memory Job. ----
    let persisted = jobs::load_job_state(&work_dir).expect("state.json 应存在");
    assert_eq!(persisted.status, JobStatus::Completed);
    assert_eq!(persisted.stage, Stage::Completed);
    assert_eq!(
        persisted
            .transcription_selection
            .map(|s| s.requested_language),
        Some(deps.language)
    );

    // ---- The production queue.json sentinel is untouched. ----
    assert_eq!(
        std::fs::read(&production_queue).unwrap(),
        sentinel,
        "集成测试不得改动生产 queue.json"
    );
}

// ---- Scenarios ----

#[test]
fn chinese_happy_path_produces_complete_documents() {
    run_happy_path(FixtureDeps::zh());
}

#[test]
fn english_happy_path_carries_language_through_artifacts() {
    // run_happy_path asserts that the full.md and raw.md contents come from
    // the utterances parsed out of the selected template, so passing the
    // English fixture proves the frozen `en` choice flows end to end.
    run_happy_path(FixtureDeps::en());
}

// ---- Scenario 3: failure evidence + production retry ----

#[test]
fn transcribe_failure_is_recorded_and_production_retry_completes() {
    let _home_lock = HOME_LOCK.lock().unwrap();
    if !ffmpeg_available() {
        eprintln!("skipping pipeline_e2e: ffmpeg 不在 PATH 上，无法执行真实标准化阶段");
        return;
    }
    let sandbox = Sandbox::new();
    let cfg = Config {
        working_dir: sandbox.working_root(),
        output_dir: sandbox.output_root(),
        ..Config::default()
    };
    let deps = FixtureDeps::zh().with_transcribe_failures(1);
    let mut job = fixture_job(&sandbox.root, SourceLanguage::Zh);
    let weak = headless_weak();
    let cancel_token = CancellationToken::new();

    // First run: the fake Runtime fails Transcribe with a scripted error.
    let error = pipeline::run_job_with_deps(&mut job, &cfg, &weak, &cancel_token, &deps)
        .expect_err("第一次转写失败应使整个任务失败");
    assert!(
        error.to_string().contains("scripted transcribe failure"),
        "错误信息应携带脚本化失败原因，实际：{error}"
    );
    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.stage_state(Stage::Transcribe), StageState::Failed);
    // Stages that finished before the failure keep their Completed evidence.
    for stage in [Stage::Metadata, Stage::DownloadAudio, Stage::NormalizeAudio] {
        assert_eq!(job.stage_state(stage), StageState::Completed);
    }
    assert_eq!(deps.transcribe_call_count(), 1);

    let work_dir = job.work_dir.clone().expect("失败前工作目录应已建立");
    assert!(
        !work_dir.join("transcript.raw.json").exists(),
        "失败的转写不得留下 raw 产物"
    );
    // Failure evidence is durable: state.json keeps the Failed status + error.
    let persisted = jobs::load_job_state(&work_dir).expect("state.json 应存在");
    assert_eq!(persisted.status, JobStatus::Failed);
    assert!(persisted
        .error
        .as_deref()
        .is_some_and(|e| e.contains("scripted transcribe failure")));

    // Production retry, mirroring desktop.rs WorkMsg::Retry: reset status to
    // Queued, clear the error, and reset the failed stage back to Pending.
    job.status = JobStatus::Queued;
    job.error = None;
    let failed = jobs::pipeline_stages()
        .into_iter()
        .find(|s| job.stage_state(*s) == StageState::Failed)
        .expect("失败后应能定位 Failed 阶段");
    assert_eq!(failed, Stage::Transcribe);
    job.set_stage_state(failed, StageState::Pending);
    job.stage = failed;
    jobs::save_job_state(&job).unwrap();

    // Retry rerun: already-Completed stages are skipped, Transcribe succeeds.
    pipeline::run_job_with_deps(&mut job, &cfg, &weak, &cancel_token, &deps)
        .expect("重试后全阶段应成功");
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.stage, Stage::Completed);
    assert_eq!(
        deps.transcribe_call_count(),
        2,
        "重试应恰好再调用一次转写（跳过已完成阶段）"
    );

    let persisted = jobs::load_job_state(&work_dir).unwrap();
    assert_eq!(persisted.status, JobStatus::Completed);
    assert!(persisted.error.is_none(), "重试成功后应清除错误记录");
    let final_dir = job
        .final_output_dir
        .clone()
        .expect("final_output_dir 应记录");
    let full_md = std::fs::read_to_string(final_dir.join("full.md")).unwrap();
    assert!(full_md.contains("大家好，欢迎来到中文集成测试示例视频。"));
}

// ---- Scenario 4: crash recovery skips completed stages ----

#[test]
fn crash_recovery_resumes_and_skips_completed_transcribe() {
    let _home_lock = HOME_LOCK.lock().unwrap();
    if !ffmpeg_available() {
        eprintln!("skipping pipeline_e2e: ffmpeg 不在 PATH 上，无法执行真实标准化阶段");
        return;
    }
    let sandbox = Sandbox::new();
    let cfg = Config {
        working_dir: sandbox.working_root(),
        output_dir: sandbox.output_root(),
        ..Config::default()
    };
    let deps = FixtureDeps::zh();
    let mut job = fixture_job(&sandbox.root, SourceLanguage::Zh);
    let weak = headless_weak();
    let cancel_token = CancellationToken::new();

    pipeline::run_job_with_deps(&mut job, &cfg, &weak, &cancel_token, &deps)
        .expect("首次完整运行应成功");
    assert_eq!(deps.transcribe_call_count(), 1);
    let work_dir = job.work_dir.clone().expect("工作目录应已建立");

    // Simulate a crash mid-run: Transcribe (and everything before it) had
    // finished, but RawDocument was caught Running with the stages after it
    // never reached. Overwrite the durable state.json with this picture.
    job.status = JobStatus::Running;
    job.stage = Stage::RawDocument;
    job.stage_progress = 0;
    job.finished_at = None;
    job.error = None;
    job.set_stage_state(Stage::RawDocument, StageState::Running);
    for stage in [
        Stage::ReadableDocument,
        Stage::Screenshots,
        Stage::FinalDocument,
        Stage::Cleanup,
    ] {
        job.set_stage_state(stage, StageState::Pending);
    }
    jobs::save_job_state(&job).unwrap();

    // Simulate app restart: reload the crashed state from disk and run the
    // production startup recovery over the in-memory queue.
    let mut queue = jobs::Queue {
        jobs: vec![jobs::load_job_state(&work_dir).expect("崩溃后的 state.json 应可加载")],
    };
    let reports = jobs::recover(&mut queue);
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].reset_stages,
        vec![Stage::RawDocument],
        "崩溃时 Running 的阶段应被重置为 Pending"
    );
    assert!(
        reports[0].revalidated_stages.is_empty(),
        "KeepAll 策略下已完成阶段的产物应全部有效，不应被重置"
    );
    let mut job = queue.jobs.remove(0);
    assert_eq!(job.status, JobStatus::Queued);
    assert_eq!(job.stage_state(Stage::Transcribe), StageState::Completed);

    // Resume through the same scheduler entry condition (status Queued). The
    // completed Transcribe must be skipped, so a fresh fake Runtime that would
    // fail loudly on wrong assumptions is never invoked.
    let resume_deps = FixtureDeps::zh();
    pipeline::run_job_with_deps(&mut job, &cfg, &weak, &cancel_token, &resume_deps)
        .expect("断点恢复重跑应成功");
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(
        resume_deps.transcribe_call_count(),
        0,
        "已完成且产物有效的 Transcribe 阶段不得再次调用 Runtime"
    );

    // The resumed run only rebuilt the document stages from the preserved
    // raw transcript.
    let final_dir = job
        .final_output_dir
        .clone()
        .expect("final_output_dir 应记录");
    let full_md = std::fs::read_to_string(final_dir.join("full.md")).unwrap();
    assert!(full_md.contains("感谢观看中文模板渲染。"));
    let persisted = jobs::load_job_state(&work_dir).unwrap();
    assert_eq!(persisted.status, JobStatus::Completed);
}

// ---- Scenario 6: recommended retention cleanup ----

#[test]
fn recommended_retention_removes_intermediates_and_keeps_documents() {
    let _home_lock = HOME_LOCK.lock().unwrap();
    if !ffmpeg_available() {
        eprintln!("skipping pipeline_e2e: ffmpeg 不在 PATH 上，无法执行真实标准化阶段");
        return;
    }
    let sandbox = Sandbox::new();
    let cfg = Config {
        working_dir: sandbox.working_root(),
        output_dir: sandbox.output_root(),
        ..Config::default()
    };
    let deps = FixtureDeps::zh();
    let mut job = fixture_job(&sandbox.root, SourceLanguage::Zh);
    job.retention = RetentionPolicy::Recommended;
    let weak = headless_weak();
    let cancel_token = CancellationToken::new();

    pipeline::run_job_with_deps(&mut job, &cfg, &weak, &cancel_token, &deps)
        .expect("recommended 策略下全流水线应成功");
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.stage_state(Stage::Cleanup), StageState::Completed);

    let work_dir = job.work_dir.clone().expect("工作目录应已建立");
    let final_dir = job
        .final_output_dir
        .clone()
        .expect("final_output_dir 应记录");

    // stage_cleanup 的 Recommended 分支（src/pipeline.rs）：删除
    // source.audio / source.video / normalized.wav。source.video 从未产生，
    // 另外两个必须真实消失。
    for removed in ["source.audio", "source.video", "normalized.wav"] {
        assert!(
            !work_dir.join(removed).exists(),
            "Recommended 策略应删除中间音频产物 {removed}"
        );
    }

    // 同一分支保留 metadata / raw / readable / 最终文档：这些是
    // artifact_expected_removed 中 Recommended 明确不视为已删除的阶段产物。
    for kept in [
        "metadata.json",
        "transcript.raw.json",
        "transcript.raw.md",
        "transcript.readable.md",
        "content-current.v1.json",
        "state.json",
    ] {
        assert!(
            work_dir.join(kept).exists(),
            "Recommended 策略应保留 {kept}"
        );
    }
    let full_md = std::fs::read_to_string(final_dir.join("full.md")).unwrap();
    assert!(full_md.contains("大家好，欢迎来到中文集成测试示例视频。"));

    // Restart recovery over the cleaned-up job must treat the missing audio
    // intermediates as expected removals (jobs.rs `artifact_expected_removed`
    // Recommended => DownloadAudio | NormalizeAudio), not requeue anything.
    let reloaded = jobs::load_job_state(&work_dir).unwrap();
    let mut queue = jobs::Queue {
        jobs: vec![reloaded],
    };
    let reports = jobs::recover(&mut queue);
    assert_eq!(reports.len(), 1);
    assert!(
        reports[0].reset_stages.is_empty(),
        "Recommended 清理后的缺失音频不应触发阶段重置"
    );
    assert!(reports[0].revalidated_stages.is_empty());
    assert_eq!(queue.jobs[0].status, JobStatus::Completed);
}

#[test]
fn v05_documents_only_retains_evidence_current_and_unique_final() {
    let _home_lock = HOME_LOCK.lock().unwrap();
    if !ffmpeg_available() {
        eprintln!("skipping pipeline_e2e: ffmpeg 不在 PATH 上，无法执行真实标准化阶段");
        return;
    }
    let sandbox = Sandbox::new();
    let cfg = Config {
        working_dir: sandbox.working_root(),
        output_dir: sandbox.output_root(),
        ..Config::default()
    };
    let deps = FixtureDeps::zh();
    let mut job = fixture_job(&sandbox.root, SourceLanguage::Zh);
    job.retention = RetentionPolicy::DocumentsOnly;
    pipeline::run_job_with_deps(
        &mut job,
        &cfg,
        &headless_weak(),
        &CancellationToken::new(),
        &deps,
    )
    .expect("v0.5 DocumentsOnly 应完成");

    let work_dir = job.work_dir.clone().unwrap();
    for kept in [
        "transcript.raw.json",
        "content-current.v1.json",
        "transcript.raw.md",
        "transcript.readable.md",
        "metadata.json",
        "state.json",
    ] {
        assert!(work_dir.join(kept).is_file(), "必须保留 {kept}");
    }
    assert!(!work_dir.join("source.audio").exists());
    assert!(!work_dir.join("normalized.wav").exists());
    let final_dir = job.final_output_dir.clone().unwrap();
    assert!(final_dir.join("full.md").is_file());
    assert!(!work_dir.join("full.md").exists());

    let reloaded = jobs::load_job_state(&work_dir).unwrap();
    let mut queue = jobs::Queue {
        jobs: vec![reloaded],
    };
    let reports = jobs::recover(&mut queue);
    assert!(reports[0].reset_stages.is_empty());
    assert!(reports[0].revalidated_stages.is_empty());
    assert_eq!(queue.jobs[0].status, JobStatus::Completed);
    assert!(jobs::artifact_valid(
        Stage::ReadableDocument,
        &work_dir,
        &queue.jobs[0]
    ));

    // Recovery leaves the structured current authoritative. Removing only
    // Presentation files must be repairable without audio/ASR/upstream work.
    std::fs::remove_file(work_dir.join("transcript.readable.md")).unwrap();
    std::fs::remove_file(final_dir.join("full.md")).unwrap();
    let mut repaired = queue.jobs.remove(0);
    std::fs::remove_file(work_dir.join("metadata.json")).unwrap();
    let resume_deps = FixtureDeps::zh().with_title("【集成测试】重建后的标题");
    pipeline::run_job_with_deps(
        &mut repaired,
        &cfg,
        &headless_weak(),
        &CancellationToken::new(),
        &resume_deps,
    )
    .expect("保留的 current + raw 应能重建 Presentation");
    assert_eq!(resume_deps.transcribe_call_count(), 0);
    assert!(work_dir.join("transcript.readable.md").is_file());
    assert!(final_dir.join("full.md").is_file());
    assert!(std::fs::read_to_string(final_dir.join("full.md"))
        .unwrap()
        .starts_with("# 【集成测试】重建后的标题\n"));
}

// ---- Scenario 5: language freeze vs. Config changes ----

#[test]
fn config_changes_do_not_override_frozen_language_selection() {
    let _home_lock = HOME_LOCK.lock().unwrap();
    if !ffmpeg_available() {
        eprintln!("skipping pipeline_e2e: ffmpeg 不在 PATH 上，无法执行真实标准化阶段");
        return;
    }
    let sandbox = Sandbox::new();
    let cfg = Config {
        working_dir: sandbox.working_root(),
        output_dir: sandbox.output_root(),
        ..Config::default()
    };
    let deps = FixtureDeps::zh();
    let mut job = fixture_job(&sandbox.root, SourceLanguage::Zh);
    let weak = headless_weak();
    let cancel_token = CancellationToken::new();

    pipeline::run_job_with_deps(&mut job, &cfg, &weak, &cancel_token, &deps)
        .expect("zh 任务首次运行应成功");
    assert_eq!(deps.transcribe_call_count(), 1);
    let final_dir = job
        .final_output_dir
        .clone()
        .expect("final_output_dir 应记录");
    let original_full_md = std::fs::read_to_string(final_dir.join("full.md")).unwrap();
    assert!(original_full_md.contains("大家好，欢迎来到中文集成测试示例视频。"));

    // The user then changes every Config field that could influence a new
    // job's runtime/language choice: another Runtime project and data dir, and
    // a last_transcription_selection frozen to English.
    let changed_cfg = Config {
        working_dir: sandbox.working_root(),
        output_dir: sandbox.output_root(),
        runtime_project: Some(sandbox.root.join("other-runtime-project")),
        runtime_data_dir: sandbox.root.join("other-runtime-data"),
        last_transcription_selection: Some(TranscriptionSelection::new(
            RuntimeSource::External,
            sandbox.root.join("other-runtime-project"),
            sandbox.root.join("other-runtime-data"),
            "contract-v2:fixture:other".into(),
            RuntimeBackend::DockerCompose,
            Some("other model".into()),
            SourceLanguage::En,
            CreatedFrom::Cli,
        )),
        ..Config::default()
    };

    // Re-running the completed job with the changed Config must not re-run
    // any stage nor overwrite the frozen zh selection.
    let rerun_deps = FixtureDeps::zh();
    pipeline::run_job_with_deps(&mut job, &changed_cfg, &weak, &cancel_token, &rerun_deps)
        .expect("已完成任务重跑应保持成功");
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(
        rerun_deps.transcribe_call_count(),
        0,
        "Config 变化不得触发已完成阶段的重新执行"
    );

    let selection = job
        .transcription_selection
        .as_ref()
        .expect("selection 应保留");
    assert_eq!(selection.requested_language, SourceLanguage::Zh);
    assert_eq!(selection.runtime_identity, FIXTURE_RUNTIME_IDENTITY);
    assert_eq!(
        selection.runtime_project,
        sandbox.root.join("fixture-runtime-project"),
        "冻结的 runtime 路径不得被 Config 覆盖"
    );

    // The artifact is still rendered from the zh template.
    let work_dir = job.work_dir.clone().expect("工作目录应已建立");
    let persisted = jobs::load_job_state(&work_dir).unwrap();
    assert_eq!(
        persisted
            .transcription_selection
            .map(|s| s.requested_language),
        Some(SourceLanguage::Zh)
    );
    let full_md = std::fs::read_to_string(final_dir.join("full.md")).unwrap();
    assert_eq!(full_md, original_full_md, "full.md 不应被 Config 变化重写");
    assert!(!full_md.contains("Hello and welcome to the English integration test fixture."));
}

// ---- v0.6 Scenario: v1 current migration on rebuild ----

#[test]
fn v1_current_file_migration_preserves_slots_and_rebuilds_presentation() {
    let _home_lock = HOME_LOCK.lock().unwrap();
    if !ffmpeg_available() {
        eprintln!("skipping pipeline_e2e: ffmpeg 不在 PATH 上，无法执行真实标准化阶段");
        return;
    }
    let sandbox = Sandbox::new();
    let cfg = Config {
        working_dir: sandbox.working_root(),
        output_dir: sandbox.output_root(),
        ..Config::default()
    };
    let deps = FixtureDeps::zh();
    let mut job = fixture_job(&sandbox.root, SourceLanguage::Zh);
    let weak = headless_weak();
    let cancel_token = CancellationToken::new();

    pipeline::run_job_with_deps(&mut job, &cfg, &weak, &cancel_token, &deps)
        .expect("首次完整运行应成功");

    let work_dir = job.work_dir.clone().unwrap();
    let final_dir = job.final_output_dir.clone().unwrap();
    let current_file = work_dir.join("content-current.v1.json");

    // Rewrite on-disk content to genuine v1: schema_version = 1, no short/long tier slots.
    let mut current_v1: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&current_file).unwrap()).unwrap();
    current_v1["schema_version"] = serde_json::json!(1);
    if let Some(slots) = current_v1.get_mut("slots").and_then(|s| s.as_object_mut()) {
        slots.remove("short_summary");
        slots.remove("long_summary");
    }
    std::fs::write(
        &current_file,
        serde_json::to_vec_pretty(&current_v1).unwrap(),
    )
    .unwrap();

    // Remove presentation markdown files to simulate a recovery rebuild.
    std::fs::remove_file(work_dir.join("transcript.readable.md")).unwrap();
    std::fs::remove_file(final_dir.join("full.md")).unwrap();

    // Rebuild presentation via pipeline recovery.
    let reloaded = jobs::load_job_state(&work_dir).unwrap();
    let mut queue = jobs::Queue {
        jobs: vec![reloaded],
    };
    let reports = jobs::recover(&mut queue);
    assert!(reports[0].reset_stages.is_empty());
    let mut repaired = queue.jobs.remove(0);

    let resume_deps = FixtureDeps::zh();
    pipeline::run_job_with_deps(&mut repaired, &cfg, &weak, &cancel_token, &resume_deps)
        .expect("从 v1 current 重建 Presentation 应成功");
    assert_eq!(resume_deps.transcribe_call_count(), 0);

    assert!(work_dir.join("transcript.readable.md").is_file());
    assert!(final_dir.join("full.md").is_file());
    let full_md = std::fs::read_to_string(final_dir.join("full.md")).unwrap();
    assert!(full_md.contains("## 忠实整理"));
    assert!(full_md.contains("## 全文"));
}

// ---- v0.6 Scenario: versioned reading metadata in full.md ----

#[test]
fn reading_metadata_in_full_md_reflects_versioned_estimate_and_omits_missing_profile() {
    let _home_lock = HOME_LOCK.lock().unwrap();
    if !ffmpeg_available() {
        eprintln!("skipping pipeline_e2e: ffmpeg 不在 PATH 上，无法执行真实标准化阶段");
        return;
    }
    let sandbox = Sandbox::new();
    let cfg = Config {
        working_dir: sandbox.working_root(),
        output_dir: sandbox.output_root(),
        ..Config::default()
    };
    let deps = FixtureDeps::zh();
    let mut job = fixture_job(&sandbox.root, SourceLanguage::Zh);
    let weak = headless_weak();
    let cancel_token = CancellationToken::new();

    pipeline::run_job_with_deps(&mut job, &cfg, &weak, &cancel_token, &deps)
        .expect("zh 任务运行应成功");

    let final_dir = job.final_output_dir.clone().unwrap();
    let full_md = std::fs::read_to_string(final_dir.join("full.md")).unwrap();

    // With zh profile: duration, scale, reading time, and saved time are all present.
    assert!(full_md.contains("- 正文规模: 约"));
    assert!(full_md.contains("- 估算口径: reading-profile v1（中文 400 字/分、英文 240 词/分、1.0x 倍速；依据 Brysbaert 2019 与 2024 中文阅读实验）"));
    assert!(full_md.contains("- 视频时长: 0分10秒"));
    assert!(full_md.contains("- 预计阅读: 约"));
    assert!(full_md.contains("- 预计节省: 约"));
}
