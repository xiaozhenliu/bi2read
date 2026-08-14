// The library crate (`src/lib.rs`) owns all modules + the Slint-generated App.
use bimyscribe::*;

use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct PackageReleaseManifest {
    app_version: String,
    build_number: u64,
    public_revision: String,
    private_revision: String,
    runtime_tag: String,
    runtime_revision: String,
    runtime_backend: String,
    runtime_contract: u32,
    uv_version: String,
    target_platform: String,
    target_arch: String,
    binary_sha256: String,
    uv_sha256: String,
    runtime_manifest_sha256: String,
}

fn sha256(path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(format!("cannot hash packaged file: {}", path.display()).into());
    }
    String::from_utf8(output.stdout)?
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "shasum returned no digest".into())
}

use bimyscribe::config::Config;
use bimyscribe::jobs::{Job, JobStatus, Queue, Stage, StageState};
use bimyscribe::scheduler::{DrainOutcome, Scheduler};

/// Messages from the UI thread to the single background worker.
#[allow(dead_code)]
enum WorkMsg {
    /// Wake the scheduler: drain all runnable jobs until idle or waiting.
    Run,
    /// Cancel the currently running job. The token is set
    /// directly by the Controller; this message updates the persisted state.
    Cancel(Uuid),
    /// Retry a failed job from its failed stage.
    Retry(Uuid),
    /// A speaker name was edited; regenerate the markdown documents only.
    SpeakerChanged(Uuid),
    Shutdown,
}

/// Parse the opt-in visual fixture selector. Empty/boolean values keep the
/// original completed fixture behavior used by existing screenshot scripts.
fn parse_ui_fixture(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "running" => Some("running"),
        "failed" => Some("failed"),
        "" | "1" | "true" | "completed" => Some("completed"),
        _ => None,
    }
}

// Keep visual fixtures at a deterministic, visible position. Test tooling may
// override this without encoding a developer-specific monitor arrangement.
const UI_FIXTURE_WINDOW_POSITION: (i32, i32) = (40, 40);

fn parse_ui_fixture_position(value: Option<&str>) -> (i32, i32) {
    let Some((x, y)) = value.and_then(|value| value.trim().split_once(',')) else {
        return UI_FIXTURE_WINDOW_POSITION;
    };
    match (x.trim().parse(), y.trim().parse()) {
        (Ok(x), Ok(y)) => (x, y),
        _ => UI_FIXTURE_WINDOW_POSITION,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--package-self-check")) {
        let runtime = bimyscribe::funasr::resolve_runtime_project(None)
            .ok_or("bundled Runtime or uv is missing")?;
        let manifest = bimyscribe::funasr::load_runtime(&runtime)?;
        if manifest.backend != bimyscribe::funasr::RuntimeBackend::NativeUv {
            return Err("bundled Runtime backend must be native-uv".into());
        }
        let resources = runtime
            .parent()
            .ok_or("bundled Runtime has no Resources directory")?;
        let release: PackageReleaseManifest =
            serde_json::from_slice(&std::fs::read(resources.join("release-manifest.json"))?)?;
        let uv = resources.join("bin/uv");
        let uv_output = std::process::Command::new(&uv).arg("--version").output()?;
        if !uv_output.status.success() {
            return Err("bundled uv --version failed".into());
        }
        let actual_uv = String::from_utf8(uv_output.stdout)?
            .split_whitespace()
            .nth(1)
            .ok_or("bundled uv returned an invalid version")?
            .to_string();
        if release.app_version != env!("CARGO_PKG_VERSION")
            || release.build_number == 0
            || release.runtime_backend != "native-uv"
            || release.runtime_contract != manifest.contract_version
            || release.uv_version != actual_uv
            || release.target_platform != "macos"
            || release.target_arch != "arm64"
            || release.public_revision.len() != 40
            || release.private_revision.len() != 40
            || release.runtime_revision.len() != 40
            || release.runtime_tag.is_empty()
            || release.binary_sha256 != sha256(&std::env::current_exe()?)?
            || release.uv_sha256 != sha256(&uv)?
            || release.runtime_manifest_sha256
                != sha256(&runtime.join(bimyscribe::funasr::RUNTIME_MANIFEST))?
        {
            return Err("release manifest does not match packaged inputs".into());
        }
        println!(
            "BiMyScribe package is ready: {} · contract v{} · Runtime {} · uv {} · build {}",
            manifest.backend.label(),
            manifest.contract_version,
            release.runtime_tag,
            release.uv_version,
            release.build_number
        );
        return Ok(());
    }
    env_logger::init();

    if let Some(status) = bimyscribe::cli::dispatch_requested()? {
        if status == std::process::ExitCode::SUCCESS {
            return Ok(());
        }
        std::process::exit(1);
    }

    run_gui()
}

fn run_gui() -> Result<(), Box<dyn std::error::Error>> {
    let app_paths = bimyscribe::paths::AppPaths::discover()?;
    let _instance_lock = match bimyscribe::paths::InstanceLock::acquire(&app_paths) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            show_already_running();
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let ui_fixture = std::env::var("BIMYSCRIBE_UI_FIXTURE")
        .ok()
        .and_then(|value| match parse_ui_fixture(&value) {
            Some(state) => Some(state),
            None => {
                log::warn!(
                    "ignoring unknown BIMYSCRIBE_UI_FIXTURE={value:?}; expected running, failed, or completed"
                );
                None
            }
        });
    if ui_fixture.is_none() {
        bimyscribe::paths::initialize_or_migrate(&app_paths)?;
    }
    let cfg = if ui_fixture.is_some() {
        Config::for_paths(&app_paths)
    } else {
        Config::load()?
    };
    let queue = if ui_fixture.is_some() {
        // Visual fixtures must never recover, schedule, or persist the user's
        // production queue while screenshot tooling launches the real binary.
        Queue::default()
    } else {
        // Load the persisted queue and run crash recovery: reset
        // in-flight stages, re-validate completed artifacts.
        let mut queue = Queue::load()?;
        let _reports = jobs::recover(&mut queue);
        // Recovery mutates only the supplied state. The real application
        // startup path explicitly owns persistence.
        for job in &queue.jobs {
            jobs::save_job_state(job)?;
        }
        queue.save()?;
        queue
    };
    if ui_fixture.is_none() {
        bimyscribe::funasr::cleanup_residual_containers(
            queue.jobs.iter().map(|job| &job.id),
            &cfg.instance_nonce,
        );
    }

    let app = App::new()?;
    if let Some(state) = ui_fixture {
        app.set_ui_verification_fixture(state.into());
        let (x, y) = parse_ui_fixture_position(
            std::env::var("BIMYSCRIBE_UI_FIXTURE_POSITION")
                .ok()
                .as_deref(),
        );
        app.window()
            .set_position(slint::PhysicalPosition::new(x, y));
    }

    // Force built-in widgets (LineEdit/CheckBox/SpinBox) into light mode regardless
    // of the macOS system appearance, so they match our fixed-light Theme tokens.
    // ColorScheme is a builtin enum reached via slint's private_unstable_api; the
    // `light-scheme` property is two-way bound to the Palette global in app.slint.
    use slint::private_unstable_api::re_exports::ColorScheme;
    app.set_light_scheme(ColorScheme::Light);
    let weak = app.as_weak();

    // ---- Models owned on the UI thread ----
    let jobs_model: Rc<VecModel<JobRow>> = Rc::new(VecModel::default());
    app.set_jobs(ModelRc::from(jobs_model.clone()));

    let stages_model: Rc<VecModel<StageView>> = Rc::new(VecModel::default());
    app.set_stages(ModelRc::from(stages_model.clone()));

    let speakers_model: Rc<VecModel<SpeakerEntry>> = Rc::new(VecModel::default());
    app.set_speakers(ModelRc::from(speakers_model.clone()));

    // The queue is shared: the UI thread reads/mutates it for add/move/speaker
    // edits, the worker reads it to find the next pending job and mutates the
    // running job in place (behind the mutex).
    let queue = Arc::new(Mutex::new(queue));
    let (tx, rx) = mpsc::channel::<WorkMsg>();

    // The scheduler owns the cancellation registry and exposes the drain loop.
    let scheduler = Arc::new(Scheduler::new(queue.clone()));

    // ---- Repopulate the job list from the recovered queue ----
    {
        let q = queue.lock().unwrap();
        for job in &q.jobs {
            jobs_model.push(job_to_row(job, false));
        }
    }

    let shared_config = Arc::new(Mutex::new(cfg.clone()));
    let controller = Rc::new(Controller {
        app: weak.clone(),
        jobs_model: jobs_model.clone(),
        stages_model: stages_model.clone(),
        speakers_model: speakers_model.clone(),
        config: shared_config.clone(),
        queue: queue.clone(),
        scheduler: scheduler.clone(),
        tx: tx.clone(),
        speaker_timer: Mutex::new(None),
    });

    // ---- Spawn the single worker thread; only one job runs at a time. ----
    {
        let cfg2 = shared_config.clone();
        let weak2 = weak.clone();
        let sched = scheduler.clone();
        let _handle: JoinHandle<()> = std::thread::spawn(move || {
            worker_loop(rx, sched, cfg2, weak2);
        });
    }

    // ---- Wire callbacks ----
    {
        let c = controller.clone();
        app.on_url_edited(move |v| {
            if let Some(app) = c.app.upgrade() {
                app.set_url(v);
                // Editing is an explicit recovery action after a validation
                // error; clear the old message without touching the draft.
                app.set_hint("".into());
                app.set_add_feedback("".into());
                app.set_add_state("idle".into());
            }
        });
    }
    {
        let c = controller.clone();
        app.on_add_clicked(move || {
            c.add_current_job();
        });
    }
    {
        let c = controller.clone();
        app.on_settings_clicked(move || {
            if let Some(app) = c.app.upgrade() {
                app.set_settings_feedback("".into());
                app.set_settings_error_field("".into());
                app.set_settings_error_message("".into());
                app.set_settings_saving(false);
                app.set_settings_visible(true);
            }
        });
    }
    {
        let c = controller.clone();
        app.on_job_selected(move |idx| {
            c.select_job(idx);
        });
    }
    {
        let c = controller.clone();
        app.on_job_navigate(move |idx, delta| {
            c.select_relative(idx, delta);
        });
    }
    {
        let app_weak = app.as_weak();
        app.on_inspector_focus_requested(move |collapsed| {
            let app_weak = app_weak.clone();
            let _ = app_weak.upgrade_in_event_loop(move |app| {
                if collapsed {
                    app.set_inspector_toggle_focus_generation(
                        app.get_inspector_toggle_focus_generation().wrapping_add(1),
                    );
                } else {
                    app.set_inspector_tab_focus_generation(
                        app.get_inspector_tab_focus_generation().wrapping_add(1),
                    );
                }
            });
        });
    }
    {
        let c = controller.clone();
        app.on_move_up(move || {
            c.move_selected(true);
        });
    }
    {
        let c = controller.clone();
        app.on_move_down(move || {
            c.move_selected(false);
        });
    }
    {
        let c = controller.clone();
        app.on_cancel_job(move || {
            c.cancel_selected();
        });
    }
    {
        let c = controller.clone();
        app.on_retry_job(move || {
            c.retry_selected();
        });
    }
    {
        let c = controller.clone();
        app.on_open_md(move || {
            c.open_selected_md();
        });
    }
    {
        let c = controller.clone();
        app.on_reveal_in_finder(move || {
            c.reveal_selected();
        });
    }
    {
        app.on_toggle_screenshots(move |_v| {
            // Screenshots are deferred this round; no-op.
        });
    }
    {
        let c = controller.clone();
        app.on_speaker_name_edited(move |id, name| {
            c.set_speaker_name(id as usize, name.to_string());
        });
    }
    {
        let c = controller.clone();
        app.on_save_settings(move |s| {
            c.save_settings(s);
        });
    }
    {
        let c = controller.clone();
        app.on_install_runtime(move |s| {
            c.install_runtime(s);
        });
    }
    {
        let c = controller.clone();
        app.on_cancel_settings(move || {
            if let Some(app) = c.app.upgrade() {
                app.set_settings_saving(false);
                app.set_settings_feedback("".into());
                app.set_settings_error_field("".into());
                app.set_settings_error_message("".into());
            }
            // Cancellation just hides; changes weren't persisted.
        });
    }
    {
        app.on_choose_directory(move |_kind| {
            choose_directory()
                .map(|path| path.to_string_lossy().to_string().into())
                .unwrap_or_default()
        });
    }
    {
        let paths = app_paths.clone();
        app.on_default_directory(move |kind| match kind.as_str() {
            "working" => paths.jobs_dir().to_string_lossy().to_string().into(),
            "output" => paths
                .markdown_output_dir()
                .to_string_lossy()
                .to_string()
                .into(),
            "runtime-data" => paths
                .funasr_runtime_data_dir()
                .to_string_lossy()
                .to_string()
                .into(),
            _ => slint::SharedString::default(),
        });
    }

    // ---- Seed initial state ----
    sync_settings(&app, &cfg);

    // Keep the worker tx alive for the app lifetime; on drop it shuts down.
    let _keep_tx = tx.clone();

    // Wake the scheduler after recovery so recovered queued jobs auto-start.
    let _ = tx.send(WorkMsg::Run);

    app.run()?;
    Ok(())
}

fn choose_directory() -> Option<std::path::PathBuf> {
    let output = std::process::Command::new("osascript")
        .args([
            "-e",
            "POSIX path of (choose folder with prompt \"选择目录\")",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| std::path::PathBuf::from(path))
}

fn show_already_running() {
    let _ = std::process::Command::new("osascript")
        .args([
            "-e",
            "display alert \"BiMyScribe 已在运行\" message \"请使用已经打开的窗口。\" as informational",
        ])
        .status();
}

// ---- Worker loop (background thread; one job at a time) ----

fn worker_loop(
    rx: mpsc::Receiver<WorkMsg>,
    scheduler: Arc<Scheduler>,
    config: Arc<Mutex<Config>>,
    app: Weak<App>,
) {
    while let Ok(msg) = rx.recv() {
        match msg {
            WorkMsg::Run => {
                // Drain the queue until idle or waiting. Each call runs one
                // job to completion.
                loop {
                    let cfg_snap = config.lock().unwrap().clone();
                    match scheduler.drain_next(&cfg_snap, &app) {
                        DrainOutcome::Idle | DrainOutcome::Waiting => break,
                        DrainOutcome::Ran { job_id, .. } => {
                            refresh_selected_detail(&app, &scheduler.queue, job_id);
                            // Continue to the next job.
                        }
                    }
                }
            }
            WorkMsg::Cancel(job_id) => {
                // The token was already set by the Controller (UI thread,
                // immediate). Here we persist the Cancelling state and let
                // the drain loop handle the aftermath.
                let mut cancelled = false;
                let mut persistence_error = None;
                let mut q = scheduler.queue.lock().unwrap();
                if let Some(job) = q.get_mut(job_id) {
                    if job.status == JobStatus::Running {
                        job.status = JobStatus::Cancelling;
                        cancelled = true;
                        if let Err(error) = jobs::save_job_state(job) {
                            log::warn!("failed to persist cancelling state: {error}");
                            persistence_error = Some(error.to_string());
                        }
                    }
                }
                if let Err(error) = q.save() {
                    log::warn!("failed to persist cancellation: {error}");
                    persistence_error = Some(error.to_string());
                }
                let feedback_error = persistence_error.is_some();
                let message = if let Some(error) = persistence_error {
                    format!("取消请求已发出，但状态保存失败：{error}")
                } else if cancelled {
                    "已发出取消请求".to_string()
                } else {
                    "取消请求已确认".to_string()
                };
                let app = app.clone();
                let _ = app.upgrade_in_event_loop(move |app| {
                    app.set_action_pending(false);
                    app.set_action_feedback(message.into());
                    app.set_action_feedback_error(feedback_error);
                });
            }
            WorkMsg::Retry(job_id) => {
                let mut requeued = false;
                let mut persistence_error = None;
                {
                    let mut q = scheduler.queue.lock().unwrap();
                    if let Some(job) = q.get_mut(job_id) {
                        // Reset the failed stage to Pending and re-queue.
                        job.status = JobStatus::Queued;
                        job.error = None;
                        if let Some(failed) = failed_stage(job) {
                            job.set_stage_state(failed, StageState::Pending);
                            job.stage = failed;
                        }
                        requeued = true;
                        if let Err(error) = jobs::save_job_state(job) {
                            log::warn!("failed to persist retry state: {error}");
                            persistence_error = Some(error.to_string());
                        }
                    }
                    if let Err(error) = q.save() {
                        log::warn!("failed to persist retry queue: {error}");
                        persistence_error = Some(error.to_string());
                    }
                }
                if !requeued {
                    let app = app.clone();
                    let _ = app.upgrade_in_event_loop(move |app| {
                        app.set_action_pending(false);
                        app.set_action_feedback("重试失败：任务不存在".into());
                        app.set_action_feedback_error(true);
                    });
                    continue;
                }
                if let Some(error) = persistence_error {
                    let app = app.clone();
                    let _ = app.upgrade_in_event_loop(move |app| {
                        app.set_action_pending(false);
                        app.set_action_feedback(
                            format!("重试状态保存失败，请重试：{error}").into(),
                        );
                        app.set_action_feedback_error(true);
                    });
                    continue;
                }
                // Drain the next runnable job.
                let cfg_snap = config.lock().unwrap().clone();
                let outcome = scheduler.drain_next(&cfg_snap, &app);
                if let DrainOutcome::Ran { job_id, .. } = &outcome {
                    refresh_selected_detail(&app, &scheduler.queue, *job_id);
                }
                let app = app.clone();
                let _ = app.upgrade_in_event_loop(move |app| {
                    app.set_action_pending(false);
                    app.set_action_feedback(
                        match outcome {
                            DrainOutcome::Ran { .. } => "重试已完成",
                            DrainOutcome::Waiting => "已重新排队，等待运行条件",
                            DrainOutcome::Idle => "已重新排队",
                        }
                        .into(),
                    );
                    app.set_action_feedback_error(false);
                });
            }
            WorkMsg::SpeakerChanged(job_id) => {
                // Changing speaker names regenerates documents without
                // transcribing the audio again.
                let mut q = scheduler.queue.lock().unwrap();
                if let Some(job) = q.get_mut(job_id) {
                    rebuild_speaker_documents(job);
                    let _ = jobs::save_job_state(job);
                }
                let _ = q.save();
            }
            WorkMsg::Shutdown => break,
        }
    }
}

/// Rebuild all markdown documents after a speaker name change.
/// Does NOT re-transcribe or call the LLM. Uses the stored `final_output_dir`
/// if available; otherwise infers from the current config (best-effort).
fn rebuild_speaker_documents(job: &mut Job) {
    use bimyscribe::document;

    let Some(dir) = job.work_dir.clone() else {
        return;
    };
    let raw_json = dir.join("transcript.raw.json");
    if !raw_json.exists() {
        return;
    }
    let utts = match pipeline::load_utterances(&raw_json) {
        Ok(u) => u,
        Err(_) => return,
    };

    // 1. Rebuild transcript.raw.md from original utterances + new speaker_map.
    let raw_md = document::render_raw_markdown(job, &utts);
    let _ = jobs::atomic_write(&dir.join("transcript.raw.md"), raw_md.as_bytes());

    // 2. Load existing readable content (reuse, no LLM call).
    let readable = std::fs::read_to_string(dir.join("transcript.readable.md")).ok();

    // 3. Rebuild full.md in the job directory.
    let raw_md_path = dir.join("transcript.raw.md");
    let meta = document::DocMeta {
        title: job.title.clone(),
        up_name: job.up_name.clone().unwrap_or_default(),
        duration_ms: job.duration_ms.unwrap_or(0),
        bvid: job.bvid.clone(),
        page: job.page,
        source_url: job.source_url.clone(),
    };
    let md = document::render_full_markdown(job, &meta, &utts, readable.as_deref(), &raw_md_path);
    let _ = jobs::atomic_write(&dir.join("full.md"), md.as_bytes());

    // 4. Rebuild full.md in the final output directory (if known).
    if let Some(out_dir) = job.final_output_dir.clone() {
        if out_dir.exists() {
            let _ = jobs::atomic_write(&out_dir.join("full.md"), md.as_bytes());
        }
    }
}

/// Find the currently-failed stage, if any.
fn failed_stage(job: &Job) -> Option<Stage> {
    jobs::pipeline_stages()
        .into_iter()
        .find(|s| job.stage_state(*s) == StageState::Failed)
}

/// Refresh file-backed task-information metrics after a worker run completes.
///
/// Stage snapshots are intentionally streamed by `pipeline.rs`, but file
/// statistics only become truthful after the terminal job state and retention
/// policy are known.  Keep this small refresh on the UI event loop boundary so
/// a selected job never keeps the previous task's media/document values.
fn refresh_selected_detail(app: &Weak<App>, queue: &Arc<Mutex<Queue>>, job_id: Uuid) {
    let queue = queue.clone();
    let _ = app.upgrade_in_event_loop(move |app| {
        let model = app.get_jobs();
        let mut selected = None;
        for index in 0..model.row_count() {
            if let Some(row) = model.row_data(index) {
                if row.selected {
                    selected = Uuid::parse_str(row.id.as_ref()).ok();
                    break;
                }
            }
        }
        if selected != Some(job_id) {
            return;
        }

        let Some(job) = queue
            .lock()
            .ok()
            .and_then(|queue| queue.get(job_id).cloned())
        else {
            return;
        };
        let now = chrono::Utc::now();
        let (elapsed_secs, elapsed_label) = fmt_job_elapsed(&job, now);
        let document_words = document_words_placeholder(&job);
        let mut detail = app.get_detail();
        detail.elapsed_secs = elapsed_secs as i32;
        detail.elapsed = elapsed_label.into();
        detail.created_at = fmt_timestamp(job.created_at.as_ref()).into();
        detail.started_at = fmt_timestamp(job.started_at.as_ref()).into();
        detail.finished_at = fmt_timestamp(job.finished_at.as_ref()).into();
        detail.video_duration = job
            .duration_ms
            .filter(|milliseconds| *milliseconds > 0)
            .map(|milliseconds| fmt_elapsed(milliseconds / 1000))
            .unwrap_or_else(|| "—".to_string())
            .into();
        detail.media_size = fmt_media_size(&job).into();
        detail.document_words = document_words.into();
        app.set_detail(detail);
        if document_words == "统计中…" {
            refresh_document_words_async(app.as_weak(), job);
        }
    });
}

// ---- Controller (lives on the UI thread) ----
struct Controller {
    app: Weak<App>,
    jobs_model: Rc<VecModel<JobRow>>,
    stages_model: Rc<VecModel<StageView>>,
    speakers_model: Rc<VecModel<SpeakerEntry>>,
    config: Arc<Mutex<Config>>,
    queue: Arc<Mutex<Queue>>,
    scheduler: Arc<Scheduler>,
    tx: Sender<WorkMsg>,
    /// Debounce timer for speaker name edits.
    speaker_timer: Mutex<Option<slint::Timer>>,
}

impl Controller {
    fn install_runtime(&self, settings: SettingsView) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_settings_saving() {
            return;
        }
        let candidate = {
            let current = self.config.lock().unwrap();
            let mut candidate = current.clone();
            if let Err(error) = candidate.apply_view(&settings) {
                app.set_settings_feedback(format!("无法安装：{error}").into());
                return;
            }
            if let Err(error) = candidate.save() {
                app.set_settings_feedback(format!("配置保存失败：{error}").into());
                return;
            }
            candidate
        };
        let Some(project_dir) = candidate.effective_runtime_project() else {
            app.set_settings_feedback("请先选择 Runtime 项目目录".into());
            return;
        };
        let runtime_data = candidate.runtime_data_dir.clone();
        *self.config.lock().unwrap() = candidate;
        app.set_settings_saving(true);
        app.set_settings_feedback("正在安装并验证 Runtime…".into());

        let app_weak = self.app.clone();
        let config = self.config.clone();
        std::thread::spawn(move || {
            let result = bimyscribe::funasr::install_runtime(&project_dir, &runtime_data);
            let _ = app_weak.upgrade_in_event_loop(move |app| {
                let current = config.lock().unwrap().clone();
                sync_settings(&app, &current);
                app.set_settings_saving(false);
                match result {
                    Ok(ready) => {
                        app.set_settings_feedback(
                            format!("Runtime 已就绪：{}", ready.device).into(),
                        );
                    }
                    Err(error) => {
                        app.set_settings_feedback(format!("Runtime 安装失败：{error}").into());
                    }
                }
            });
        });
    }

    fn add_current_job(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_add_busy() {
            return;
        }
        let url = app.get_url().to_string();
        if url.trim().is_empty() {
            return;
        }
        let runtime_ready = {
            let config = self.config.lock().unwrap();
            config.effective_runtime_project().is_some_and(|project| {
                bimyscribe::funasr::runtime_is_ready(&project, &config.runtime_data_dir)
            })
        };
        if !runtime_ready {
            app.set_add_state("error".into());
            app.set_add_feedback("请先在设置中选择并安装或验证 FunASR Runtime".into());
            return;
        }

        app.set_add_busy(true);
        app.set_add_state("submitting".into());
        app.set_add_feedback("正在添加任务…".into());
        app.set_hint("".into());

        let parsed = match bilibili::parse_url(&url) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("URL parse failed: {e}");
                app.set_add_busy(false);
                app.set_add_state("error".into());
                app.set_add_feedback(format!("无法添加：{e}").into());
                return;
            }
        };

        let id = Uuid::new_v4();
        let page = parsed.page.unwrap_or(1);
        let mut job = Job::new(id, parsed.bvid.clone(), page);
        job.source_url = Some(url.clone());
        // Copy the global default retention policy into the job so later config
        // changes don't affect already-created jobs.
        job.retention = self.config.lock().unwrap().default_retention;
        // Initialize all happy-path stages to Pending.
        for s in jobs::pipeline_stages() {
            job.set_stage_state(s, StageState::Pending);
        }

        // Persist and add to the shared queue.
        let persist_result = {
            let mut q = self.queue.lock().unwrap();
            q.jobs.push(job.clone());
            match q.save() {
                Ok(()) => Ok(()),
                Err(error) => {
                    // Do not leave an in-memory-only task behind when the
                    // queue file cannot be written.
                    q.jobs.pop();
                    Err(error)
                }
            }
        };
        if let Err(error) = persist_result {
            app.set_add_busy(false);
            app.set_add_state("error".into());
            app.set_add_feedback(format!("无法保存任务：{error}").into());
            return;
        }

        let row = job_to_row(&job, true);

        // Deselect all existing rows, then append the new (selected) one.
        let len = self.jobs_model.row_count();
        for i in 0..len {
            if let Some(mut r) = self.jobs_model.row_data(i) {
                r.selected = false;
                self.jobs_model.set_row_data(i, r);
            }
        }
        self.jobs_model.push(row);

        // Show detail for the new job.
        self.set_detail_for(id.to_string());

        // Clear the input.
        app.set_url("".into());
        app.set_add_busy(false);
        app.set_add_state("success".into());
        app.set_add_feedback("已添加任务".into());

        // Tell the worker there's work.
        let _ = self.tx.send(WorkMsg::Run);
    }

    fn select_job(&self, idx: i32) {
        if idx < 0 {
            return;
        }
        let idx = idx as usize;
        let len = self.jobs_model.row_count();
        if let Some(app) = self.app.upgrade() {
            app.set_action_pending(false);
            app.set_action_feedback("".into());
            app.set_action_feedback_error(false);
            app.set_speaker_feedback("".into());
            app.set_speaker_feedback_error(false);
        }
        for i in 0..len {
            if let Some(mut r) = self.jobs_model.row_data(i) {
                r.selected = i == idx;
                self.jobs_model.set_row_data(i, r);
            }
        }
        if let Some(row) = self.jobs_model.row_data(idx) {
            let id = row.id.to_string();
            self.show_detail(&id);
        }
        self.update_reorder_capabilities();
    }

    fn select_relative(&self, idx: i32, delta: i32) {
        let len = self.jobs_model.row_count();
        if len == 0 {
            return;
        }
        let target = (idx + delta).clamp(0, len.saturating_sub(1) as i32);
        self.select_job(target);
    }

    fn move_selected(&self, up: bool) {
        let idx = self.selected_index();
        let Some(idx) = idx else { return };
        let other = if up {
            idx.checked_sub(1)
        } else {
            Some(idx + 1)
        };
        let Some(other) = other else { return };
        if other >= self.jobs_model.row_count() {
            return;
        }
        self.jobs_model.swap(idx, other);
        // Mirror the reorder into the persistent queue.
        {
            let mut q = self.queue.lock().unwrap();
            if idx < q.jobs.len() && other < q.jobs.len() {
                q.jobs.swap(idx, other);
                let _ = q.save();
            }
        }
        self.update_reorder_capabilities();
    }

    fn selected_index(&self) -> Option<usize> {
        for i in 0..self.jobs_model.row_count() {
            if let Some(r) = self.jobs_model.row_data(i) {
                if r.selected {
                    return Some(i);
                }
            }
        }
        None
    }

    fn update_reorder_capabilities(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let len = self.jobs_model.row_count();
        let selected = self.selected_index();
        let (can_move_up, can_move_down) = reorder_capabilities(selected, len);
        app.set_can_move_up(can_move_up);
        app.set_can_move_down(can_move_down);
    }

    fn selected_job_id(&self) -> Option<Uuid> {
        let idx = self.selected_index()?;
        let row = self.jobs_model.row_data(idx)?;
        Uuid::parse_str(row.id.as_ref()).ok()
    }

    fn cancel_selected(&self) {
        if let Some(id) = self.selected_job_id() {
            if let Some(app) = self.app.upgrade() {
                if app.get_action_pending() {
                    return;
                }
                app.set_action_pending(true);
                app.set_action_feedback("正在取消任务…".into());
                app.set_action_feedback_error(false);
            }
            // Set the cancellation token directly (immediate, not queued
            // behind the blocked worker). The pipeline checks it between
            // stages and during long blocking calls.
            self.scheduler.cancel_registry.cancel(id);
            if self.tx.send(WorkMsg::Cancel(id)).is_err() {
                if let Some(app) = self.app.upgrade() {
                    app.set_action_pending(false);
                    app.set_action_feedback("取消失败：后台任务已退出".into());
                    app.set_action_feedback_error(true);
                }
            }
        }
    }

    fn retry_selected(&self) {
        if let Some(id) = self.selected_job_id() {
            if let Some(app) = self.app.upgrade() {
                if app.get_action_pending() {
                    return;
                }
                app.set_action_pending(true);
                app.set_action_feedback("正在重新排队…".into());
                app.set_action_feedback_error(false);
            }
            if self.tx.send(WorkMsg::Retry(id)).is_err() {
                if let Some(app) = self.app.upgrade() {
                    app.set_action_pending(false);
                    app.set_action_feedback("重试失败：后台任务已退出".into());
                    app.set_action_feedback_error(true);
                }
                return;
            }
            // The worker re-queues; nudge it to run.
            if self.tx.send(WorkMsg::Run).is_err() {
                if let Some(app) = self.app.upgrade() {
                    app.set_action_pending(false);
                    app.set_action_feedback("重试失败：无法唤醒后台任务".into());
                    app.set_action_feedback_error(true);
                }
            }
        }
    }

    fn open_selected_md(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_action_pending() {
            return;
        }
        let Some(id) = self.selected_job_id() else {
            return;
        };
        let q = self.queue.lock().unwrap();
        let Some(job) = q.get(id) else { return };
        let candidates = [
            job.final_output_dir.as_ref().map(|dir| dir.join("full.md")),
            job.work_dir.as_ref().map(|dir| dir.join("full.md")),
        ];
        let Some(full) = candidates.into_iter().flatten().find(|path| path.exists()) else {
            app.set_action_feedback("打开失败：Markdown 尚未生成".into());
            app.set_action_feedback_error(true);
            return;
        };
        app.set_action_pending(true);
        app.set_action_feedback("正在打开 Markdown…".into());
        app.set_action_feedback_error(false);
        match std::process::Command::new("open").arg(&full).spawn() {
            Ok(_) => {
                app.set_action_pending(false);
                app.set_action_feedback("已请求打开 Markdown".into());
            }
            Err(error) => {
                app.set_action_pending(false);
                app.set_action_feedback(format!("打开失败：{error}").into());
                app.set_action_feedback_error(true);
            }
        }
    }

    fn reveal_selected(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_action_pending() {
            return;
        }
        let Some(id) = self.selected_job_id() else {
            return;
        };
        let q = self.queue.lock().unwrap();
        let Some(job) = q.get(id) else { return };
        let Some(dir) = &job.work_dir else {
            app.set_action_feedback("显示失败：任务目录尚未生成".into());
            app.set_action_feedback_error(true);
            return;
        };
        if !dir.exists() {
            app.set_action_feedback("显示失败：任务目录不存在".into());
            app.set_action_feedback_error(true);
            return;
        }
        app.set_action_pending(true);
        app.set_action_feedback("正在定位任务目录…".into());
        app.set_action_feedback_error(false);
        match std::process::Command::new("open")
            .arg("-R")
            .arg(dir)
            .spawn()
        {
            Ok(_) => {
                app.set_action_pending(false);
                app.set_action_feedback("已请求在访达中显示".into());
            }
            Err(error) => {
                app.set_action_pending(false);
                app.set_action_feedback(format!("显示失败：{error}").into());
                app.set_action_feedback_error(true);
            }
        }
    }

    fn set_speaker_name(&self, id: usize, name: String) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        app.set_speaker_feedback("保存中…".into());
        app.set_speaker_feedback_error(false);
        let name_clone = name.clone();
        {
            let len = self.speakers_model.row_count();
            for i in 0..len {
                if let Some(mut s) = self.speakers_model.row_data(i) {
                    if s.speaker_id as usize == id {
                        s.name = name_clone.clone().into();
                        self.speakers_model.set_row_data(i, s);
                        break;
                    }
                }
            }
        }
        // Persist into the selected job's speaker_map immediately so the
        // data is never lost, but debounce the document rebuild.
        let job_id = self.selected_job_id();
        if let Some(job_id) = job_id {
            let persist_result = {
                let mut q = self.queue.lock().unwrap();
                match q.get_mut(job_id) {
                    Some(job) => {
                        job.speaker_map.insert(id as u32, name);
                        jobs::save_job_state(job)
                            .map_err(|error| format!("保存说话人失败：{error}"))
                            .and_then(|_| {
                                q.save().map_err(|error| format!("保存队列失败：{error}"))
                            })
                    }
                    None => Err("任务不存在".to_string()),
                }
            };

            if let Err(error) = persist_result {
                app.set_speaker_feedback(format!("保存失败：{error}").into());
                app.set_speaker_feedback_error(true);
                return;
            }
            app.set_speaker_feedback("已保存".into());
            app.set_speaker_feedback_error(false);

            // Debounce: reset the timer on each edit. After 200ms of no
            // further edits, send a single SpeakerChanged message.
            let tx = self.tx.clone();
            let mut timer_guard = self.speaker_timer.lock().unwrap();
            let timer = timer_guard.get_or_insert_with(|| {
                let t = slint::Timer::default();
                t.start(
                    slint::TimerMode::SingleShot,
                    std::time::Duration::from_millis(200),
                    move || {
                        // The closure captures `tx` but we need the job_id at fire
                        // time, so we use a shared cell. The timer is restarted
                        // below with the current job_id before it fires.
                    },
                );
                t
            });
            // Restart with the current job_id. We use a stop + start cycle
            // because Slint Timer doesn't allow changing the callback.
            timer.stop();
            timer.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(200),
                move || {
                    let _ = tx.send(WorkMsg::SpeakerChanged(job_id));
                },
            );
        } else {
            app.set_speaker_feedback("保存失败：未选择任务".into());
            app.set_speaker_feedback_error(true);
        }
    }

    fn save_settings(&self, s: SettingsView) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_settings_saving() {
            return;
        }
        app.set_settings_saving(true);
        app.set_settings_feedback("正在保存…".into());
        app.set_settings_error_field("".into());
        app.set_settings_error_message("".into());
        {
            let mut cfg = self.config.lock().unwrap();
            let mut candidate = cfg.clone();
            if let Err(e) = candidate.apply_view(&s) {
                log::error!("settings validation failed: {e}");
                let field = if e.starts_with("工作目录") {
                    "working"
                } else if e.starts_with("Markdown 输出目录") {
                    "output"
                } else if e.starts_with("FunASR Runtime") {
                    "funasr"
                } else if e.starts_with("Runtime 数据目录") {
                    "runtime-data"
                } else {
                    ""
                };
                app.set_settings_error_field(field.into());
                app.set_settings_error_message(e.clone().into());
                app.set_settings_saving(false);
                app.set_settings_feedback(format!("保存失败：{e}").into());
                return;
            }
            if let Err(e) = candidate.save() {
                log::error!("failed to save config: {e}");
                app.set_settings_saving(false);
                app.set_settings_feedback(format!("保存失败：{e}").into());
                return;
            }
            *cfg = candidate;
        }
        let cfg = self.config.lock().unwrap().clone();
        sync_settings(&app, &cfg);
        app.set_settings_saving(false);
        app.set_settings_feedback("已保存".into());
        app.set_settings_visible(false);
    }

    fn show_detail(&self, id: &str) {
        self.set_detail_for(id.to_string());
    }

    /// Set the detail panel + stages + speakers from the job with the given id.
    /// Reads the persisted `Job` (single source of truth) and builds a snapshot
    /// rather than inferring from the row.
    fn set_detail_for(&self, id: String) {
        let Some(app) = self.app.upgrade() else {
            return;
        };

        // Look up the job in the shared queue.
        let job_opt: Option<Job> = {
            let q = self.queue.lock().unwrap();
            Uuid::parse_str(&id)
                .ok()
                .and_then(|uid| q.get(uid))
                .cloned()
        };

        let Some(job) = job_opt else {
            self.stages_model.set_vec(Vec::new());
            self.speakers_model.set_vec(Vec::new());
            app.set_detail(empty_detail());
            self.update_reorder_capabilities();
            return;
        };

        // Build the snapshot from the job (reads StageState directly).
        let now = chrono::Utc::now();
        let snap = jobs::JobViewSnapshot::from_job(&job, now);
        let (elapsed_secs, elapsed_label) = fmt_job_elapsed(&job, now);

        // Stages view: from the snapshot's StageSnapshot list (reads persisted
        // StageState rather than inferring it from sequence position.
        let stage_views = pipeline::stage_views_from_snapshot(&snap);
        self.stages_model.set_vec(stage_views);

        // Speakers: from the job's speaker_map if known, else default 3 slots.
        let spk: Vec<SpeakerEntry> = {
            let ids: Vec<u32> = {
                let raw: Option<Vec<funasr::Utterance>> = job
                    .work_dir
                    .as_ref()
                    .and_then(|d| pipeline::load_utterances(&d.join("transcript.raw.json")).ok());
                match raw {
                    Some(utts) => {
                        let mut v: Vec<u32> = utts.iter().map(|u| u.speaker_id).collect();
                        v.sort();
                        v.dedup();
                        v
                    }
                    None => (0..3).collect(),
                }
            };
            ids.iter()
                .map(|i| SpeakerEntry {
                    speaker_id: *i as i32,
                    raw_label: format!("Speaker {}", i).into(),
                    name: job.speaker_map.get(i).cloned().unwrap_or_default().into(),
                })
                .collect()
        };
        self.speakers_model.set_vec(spk);

        let caps = &snap.capabilities;
        let document_words = document_words_placeholder(&job);
        app.set_detail(JobDetailData {
            has_job: true,
            title: snap.title.clone().into(),
            bvid: snap.bvid.clone().into(),
            page: snap.page as i32,
            status_label: snap.status.label().into(),
            total_progress: snap.total_progress as i32,
            elapsed_secs: elapsed_secs as i32,
            elapsed: elapsed_label.into(),
            error_text: snap.error.clone().unwrap_or_default().into(),
            has_error: snap.error.is_some(),
            warning_text: snap
                .warning
                .as_ref()
                .map(|w| w.label())
                .unwrap_or_default()
                .into(),
            can_cancel: caps.can_cancel,
            can_retry: caps.can_retry,
            can_open: caps.can_open_document,
            can_reveal: caps.can_reveal,
            can_edit_speakers: caps.can_edit_speakers,
            screenshots_enabled: false,
            retention_label: snap.retention_label.clone().into(),
            created_at: fmt_timestamp(job.created_at.as_ref()).into(),
            started_at: fmt_timestamp(job.started_at.as_ref()).into(),
            finished_at: fmt_timestamp(job.finished_at.as_ref()).into(),
            video_duration: job
                .duration_ms
                .filter(|milliseconds| *milliseconds > 0)
                .map(|milliseconds| fmt_elapsed(milliseconds / 1000))
                .unwrap_or_else(|| "—".to_string())
                .into(),
            media_size: fmt_media_size(&job).into(),
            document_words: document_words.into(),
        });
        if document_words == "统计中…" {
            refresh_document_words_async(self.app.clone(), job);
        }
        self.update_reorder_capabilities();
    }
}

fn reorder_capabilities(selected: Option<usize>, len: usize) -> (bool, bool) {
    let can_move_up = selected.is_some_and(|index| index > 0 && index < len);
    let can_move_down = selected.is_some_and(|index| index + 1 < len);
    (can_move_up, can_move_down)
}

/// Convert a `Job` into the Slint `JobRow` view model. Uses the job's
/// `started_at`/`finished_at` to compute elapsed time.
fn job_to_row(job: &Job, selected: bool) -> JobRow {
    let stage = job.stage;
    let indeterminate = stage == Stage::Transcribe;
    let now = chrono::Utc::now();
    let (elapsed_secs, elapsed_label) = fmt_job_elapsed(job, now);
    JobRow {
        id: job.id.to_string().into(),
        title: job.title.clone().into(),
        bvid: job.bvid.clone().into(),
        page: job.page as i32,
        stage_name: stage.name().into(),
        stage_label: stage.label().into(),
        stage_progress: job.stage_progress as i32,
        total_progress: job.total_progress() as i32,
        indeterminate,
        elapsed_secs: elapsed_secs as i32,
        elapsed: elapsed_label.into(),
        selected,
        has_error: job.error.is_some(),
        error_text: job.error.clone().unwrap_or_default().into(),
        status_label: job.status.label().into(),
    }
}

fn empty_detail() -> JobDetailData {
    JobDetailData {
        has_job: false,
        title: "".into(),
        bvid: "".into(),
        page: 1,
        status_label: "".into(),
        total_progress: 0,
        elapsed_secs: 0,
        elapsed: "—".into(),
        error_text: "".into(),
        has_error: false,
        warning_text: "".into(),
        can_cancel: false,
        can_retry: false,
        can_open: false,
        can_reveal: false,
        can_edit_speakers: false,
        screenshots_enabled: false,
        retention_label: jobs::RetentionPolicy::default().label().into(),
        created_at: "—".into(),
        started_at: "—".into(),
        finished_at: "—".into(),
        video_duration: "—".into(),
        media_size: "尚未生成".into(),
        document_words: "尚未生成".into(),
    }
}

/// Format a number of seconds as MM:SS (or H:MM:SS for >= 1h).
fn fmt_elapsed(secs: u64) -> String {
    pipeline::fmt_elapsed(secs)
}

/// Return elapsed time together with a truthful display value for a job.
///
/// A queued job has no `started_at`; exposing `00:00` in that state looks like
/// measured work and is especially confusing when a user compares jobs.  Keep
/// the numeric value at zero for progress calculations, but render an em dash
/// until the pipeline has recorded a start time.
fn fmt_job_elapsed(job: &Job, now: chrono::DateTime<chrono::Utc>) -> (u64, String) {
    let Some(start) = job.started_at else {
        return (0, "—".to_string());
    };
    let secs = match job.finished_at {
        Some(end) => (end - start).num_seconds().max(0) as u64,
        None => (now - start).num_seconds().max(0) as u64,
    };
    (secs, fmt_elapsed(secs))
}

fn fmt_timestamp(timestamp: Option<&chrono::DateTime<chrono::Utc>>) -> String {
    timestamp
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "—".to_string())
}

fn fmt_media_size(job: &Job) -> String {
    let Some(work_dir) = &job.work_dir else {
        return "尚未生成".to_string();
    };

    let video_bytes = std::fs::metadata(work_dir.join("source.video"))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let audio_bytes = std::fs::metadata(work_dir.join("source.audio"))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let total_bytes = video_bytes.saturating_add(audio_bytes);

    if total_bytes > 0 {
        let kind = match (video_bytes > 0, audio_bytes > 0) {
            (true, true) => "音视频",
            (true, false) => "视频",
            (false, true) => "音频",
            (false, false) => unreachable!(),
        };
        return format!("{}（{}）", fmt_bytes(total_bytes), kind);
    }

    if job.status == JobStatus::Completed && job.retention != jobs::RetentionPolicy::KeepAll {
        "已按保留策略清理".to_string()
    } else {
        "尚未生成".to_string()
    }
}

fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn fmt_document_words(job: &Job) -> String {
    let final_path = job
        .final_output_dir
        .as_ref()
        .map(|directory| directory.join("full.md"));
    let work_full_path = job
        .work_dir
        .as_ref()
        .map(|directory| directory.join("full.md"));
    let raw_path = job
        .work_dir
        .as_ref()
        .map(|directory| directory.join("transcript.raw.md"));

    for path in [final_path.as_ref(), work_full_path.as_ref()]
        .into_iter()
        .flatten()
    {
        if let Ok(count) = count_document_chars(path) {
            return format!("{count} 字");
        }
    }
    if let Some(path) = raw_path {
        if let Ok(count) = count_document_chars(&path) {
            return format!("{count} 字（原始稿）");
        }
    }
    "尚未生成".to_string()
}

fn document_words_placeholder(job: &Job) -> &'static str {
    let has_document = job
        .final_output_dir
        .as_ref()
        .is_some_and(|directory| directory.join("full.md").is_file())
        || job.work_dir.as_ref().is_some_and(|directory| {
            directory.join("full.md").is_file() || directory.join("transcript.raw.md").is_file()
        });
    if has_document {
        "统计中…"
    } else {
        "尚未生成"
    }
}

fn refresh_document_words_async(app: Weak<App>, job: Job) {
    std::thread::spawn(move || {
        let job_id = job.id;
        let label = fmt_document_words(&job);
        let _ = app.upgrade_in_event_loop(move |app| {
            let selected_job_id = (0..app.get_jobs().row_count()).find_map(|index| {
                let row = app.get_jobs().row_data(index)?;
                row.selected
                    .then(|| Uuid::parse_str(row.id.as_ref()).ok())
                    .flatten()
            });
            if selected_job_id != Some(job_id) {
                return;
            }
            let mut detail = app.get_detail();
            detail.document_words = label.into();
            app.set_detail(detail);
        });
    });
}

fn count_document_chars(path: &std::path::Path) -> std::io::Result<usize> {
    let content = std::fs::read_to_string(path)?;
    Ok(count_non_whitespace_chars(&content))
}

fn count_non_whitespace_chars(content: &str) -> usize {
    content
        .chars()
        .filter(|character| !character.is_whitespace())
        .count()
}

fn sync_settings(app: &App, cfg: &Config) {
    app.set_settings(cfg.to_view());
}

#[cfg(test)]
mod tests {
    use super::{
        count_document_chars, job_to_row, parse_ui_fixture, parse_ui_fixture_position,
        reorder_capabilities, JobRow,
    };
    use bimyscribe::jobs::Job;
    use slint::{Model, VecModel};
    use std::rc::Rc;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    #[test]
    fn ui_fixture_parser_supports_three_states_and_legacy_switches() {
        assert_eq!(parse_ui_fixture("running"), Some("running"));
        assert_eq!(parse_ui_fixture("FAILED"), Some("failed"));
        assert_eq!(parse_ui_fixture(" completed "), Some("completed"));
        assert_eq!(parse_ui_fixture("1"), Some("completed"));
        assert_eq!(parse_ui_fixture(""), Some("completed"));
        assert_eq!(parse_ui_fixture("unknown"), None);
    }

    #[test]
    fn ui_fixture_window_position_is_fixed_and_optionally_overridable() {
        assert_eq!(parse_ui_fixture_position(None), (40, 40));
        assert_eq!(parse_ui_fixture_position(Some(" 120, 80 ")), (120, 80));
        assert_eq!(parse_ui_fixture_position(Some("invalid")), (40, 40));
    }

    #[test]
    fn reorder_capabilities_follow_queue_boundaries() {
        assert_eq!(reorder_capabilities(None, 0), (false, false));
        assert_eq!(reorder_capabilities(None, 3), (false, false));
        assert_eq!(reorder_capabilities(Some(0), 1), (false, false));
        assert_eq!(reorder_capabilities(Some(0), 3), (false, true));
        assert_eq!(reorder_capabilities(Some(1), 3), (true, true));
        assert_eq!(reorder_capabilities(Some(2), 3), (true, false));
        assert_eq!(reorder_capabilities(Some(3), 3), (false, false));
    }

    #[test]
    fn queue_model_handles_500_simulated_tasks_without_blocking_updates() {
        // This exercises the model operations used by the Slint ListView.  It
        // intentionally avoids a window or fixture so the regression check is
        // deterministic and safe to run in CI/headless environments.
        let model = Rc::new(VecModel::<JobRow>::default());
        let started = std::time::Instant::now();
        for index in 0..500 {
            let mut job = Job::new(Uuid::new_v4(), format!("BV{index:06}"), 1);
            job.title = format!("模拟任务 {index}");
            model.push(job_to_row(&job, index == 0));
        }
        for index in 0..500 {
            let mut row = model
                .row_data(index)
                .expect("every simulated task should be readable");
            row.selected = index == 250;
            model.set_row_data(index, row);
        }
        assert_eq!(model.row_count(), 500);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn large_markdown_statistics_benchmark() {
        const SIZE: usize = 20 * 1024 * 1024;
        let path =
            std::env::temp_dir().join(format!("bimyscribe-large-markdown-{}.md", Uuid::new_v4()));
        std::fs::write(&path, vec![b'a'; SIZE]).expect("write large markdown fixture");

        let started = Instant::now();
        let count = count_document_chars(&path).expect("count large markdown fixture");
        let elapsed = started.elapsed();
        let _ = std::fs::remove_file(&path);

        eprintln!(
            "20 MiB markdown statistics: {:.2} ms",
            elapsed.as_secs_f64() * 1000.0
        );
        assert_eq!(count, SIZE);
        assert!(
            elapsed < Duration::from_secs(1),
            "20 MiB markdown statistics took {elapsed:?}"
        );
    }

    #[test]
    fn missing_job_start_is_rendered_as_an_em_dash() {
        let job = Job::new(Uuid::new_v4(), "BVTEST".to_string(), 1);
        let (_, elapsed) = super::fmt_job_elapsed(&job, chrono::Utc::now());
        assert_eq!(elapsed, "—");
        assert_eq!(super::empty_detail().elapsed.to_string(), "—");
    }
}
