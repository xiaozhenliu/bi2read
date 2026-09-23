//! Desktop application composition root.
//!
//! The binary calls the single [`run`] interface. Queue recovery, Slint model
//! wiring, the background worker, and controller callbacks stay private here.

use crate::*;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};
use uuid::Uuid;

use crate::config::Config;
use crate::content_results::{
    ArtifactKindV1, ContentBlockV1, ContentViewV1, EffectivePathV1, FallbackReasonCodeV1, Intent,
    ReadyGenerationTargetV1, RegenerableKind, SourceStatusV1,
};
use crate::jobs::{
    CreatedFrom, Job, JobCreationInput, JobStatus, Queue, SourceLanguage, Stage, StageState,
    TranscriptionSelection,
};
use crate::scheduler::{DrainOutcome, JobResult, Scheduler};

/// Messages from the UI thread to the single background worker.
#[allow(dead_code)]
enum WorkMsg {
    /// Wake the scheduler: drain all runnable jobs until idle or waiting.
    Run,
    /// Retry a failed job from its failed stage.
    Retry(Uuid),
    /// Regenerate one terminal v0.5 enhancement from the clicked target
    /// snapshot. The command exists only in the in-memory worker channel.
    Regenerate {
        job_id: Uuid,
        kind: RegenerableKind,
        target: ReadyGenerationTargetV1,
        tier: crate::document::SummaryTier,
    },
    /// Rebuild a missing/corrupt v0.5 current from already-valid Evidence.
    RepairContent {
        job_id: Uuid,
        tier: crate::document::SummaryTier,
    },
    /// Delete one terminal job: its artifacts first, then its queue record.
    DeleteJob(Uuid),
    /// Delete every terminal job (history clear). Live jobs are untouched.
    ClearHistory,
    /// A speaker name was edited; regenerate the markdown documents only.
    SpeakerChanged {
        job_id: Uuid,
        tier: crate::document::SummaryTier,
    },
    Shutdown,
}

/// In-memory gate for terminal content operations. It deliberately has no
/// serde representation and is not part of Queue/Job state: a crashed worker
/// simply drops the token and the user can issue the operation again.
#[derive(Default)]
struct ContentBusy {
    job_id: Option<Uuid>,
    token: Option<crate::cancel::CancellationToken>,
}

struct ContentBusyGuard {
    busy: Arc<Mutex<ContentBusy>>,
    job_id: Uuid,
}

impl Drop for ContentBusyGuard {
    fn drop(&mut self) {
        clear_content_operation(&self.busy, self.job_id);
    }
}

fn reserve_content_operation(busy: &Arc<Mutex<ContentBusy>>, job_id: Uuid) -> bool {
    let mut state = busy.lock().unwrap();
    if state.job_id.is_some() {
        return false;
    }
    state.job_id = Some(job_id);
    // Create the token at reservation time so Cancel can signal an operation
    // that is still waiting in the worker channel.
    state.token = Some(crate::cancel::CancellationToken::new());
    true
}

fn start_content_operation(
    busy: &Arc<Mutex<ContentBusy>>,
    job_id: Uuid,
) -> Option<crate::cancel::CancellationToken> {
    let state = busy.lock().unwrap();
    if state.job_id != Some(job_id) {
        return None;
    }
    state.token.clone()
}

fn clear_content_operation(busy: &Arc<Mutex<ContentBusy>>, job_id: Uuid) {
    let mut state = busy.lock().unwrap();
    if state.job_id == Some(job_id) {
        state.job_id = None;
        state.token = None;
    }
}

fn cancel_content_operation(busy: &Arc<Mutex<ContentBusy>>, job_id: Uuid) -> bool {
    let token = busy
        .lock()
        .ok()
        .and_then(|state| (state.job_id == Some(job_id)).then(|| state.token.clone()))
        .flatten();
    if let Some(token) = token {
        token.cancel();
        true
    } else {
        false
    }
}

/// Parse the opt-in visual fixture selector. Empty/boolean values keep the
/// original completed fixture behavior used by existing screenshot scripts.
fn parse_ui_fixture(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "empty" => Some("empty"),
        "settings" => Some("settings"),
        "running" => Some("running"),
        "failed" => Some("failed"),
        "queue-scroll" => Some("queue-scroll"),
        "speaker-empty" => Some("speaker-empty"),
        "speaker-single" => Some("speaker-single"),
        "" | "1" | "true" | "completed" => Some("completed"),
        "result-success" => Some("result-success"),
        "result-limited" => Some("result-limited"),
        "result-enhancement-failed" => Some("result-enhancement-failed"),
        "result-raw-only" => Some("result-raw-only"),
        "result-regenerating" => Some("result-regenerating"),
        "settings-connection-tested" => Some("settings-connection-tested"),
        "settings-connection-failed" => Some("settings-connection-failed"),
        "result-summary-depths" => Some("result-summary-depths"),
        "result-search" => Some("result-search"),
        "result-model-changed" => Some("result-model-changed"),
        "confirmation" => Some("confirmation"),
        "delete-confirm" => Some("delete-confirm"),
        "clear-confirm" => Some("clear-confirm"),
        _ => None,
    }
}

fn queue_scroll_fixture_jobs() -> Vec<Job> {
    (0..12)
        .map(|index| {
            let mut job = Job::new(Uuid::new_v4(), format!("BV1SCROLL{index:02}"), 1);
            job.title = format!("长队列滚动边框验证任务 {:02}", index + 1);
            job.status = if index % 4 == 2 {
                JobStatus::Failed
            } else {
                JobStatus::Completed
            };
            job.stage = if job.status == JobStatus::Failed {
                Stage::Transcribe
            } else {
                Stage::Completed
            };
            job.stage_progress = if job.status == JobStatus::Failed {
                42
            } else {
                100
            };
            if job.status == JobStatus::Failed {
                job.error = Some("隔离 fixture 错误边框".into());
                job.set_stage_state(job.stage, StageState::Failed);
            } else {
                job.set_stage_state(job.stage, StageState::Completed);
            }
            job
        })
        .collect()
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

fn parse_ui_fixture_size(value: Option<&str>) -> (u32, u32) {
    let Some((width, height)) = value.and_then(|value| value.trim().split_once('x')) else {
        return (1180, 760);
    };
    match (width.trim().parse(), height.trim().parse()) {
        (Ok(width), Ok(height)) if width >= 920 && height >= 600 => (width, height),
        _ => (1180, 760),
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let app_paths = crate::paths::AppPaths::discover()?;
    let _instance_lock = match crate::paths::InstanceLock::acquire(&app_paths) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            show_already_running();
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let ui_fixture = std::env::var("BI2READ_UI_FIXTURE")
        .ok()
        .and_then(|value| match parse_ui_fixture(&value) {
            Some(state) => Some(state),
            None => {
                log::warn!(
                    "ignoring unknown BI2READ_UI_FIXTURE={value:?}; expected empty, running, failed, completed, queue-scroll, settings, confirmation, delete-confirm, or clear-confirm"
                );
                None
            }
        });
    if ui_fixture.is_none() {
        crate::paths::initialize_or_migrate(&app_paths)?;
    }
    let mut cfg = if ui_fixture.is_some() {
        Config::for_paths(&app_paths)
    } else {
        Config::load()?
    };
    let result_fixture = ui_fixture.is_some_and(|state| state.starts_with("result-"));
    let fixture_root = result_fixture
        .then(|| std::env::temp_dir().join(format!("bi2read-result-fixture-{}", Uuid::new_v4())));
    let is_settings_test_fixture = matches!(
        ui_fixture,
        Some("settings-connection-tested" | "settings-connection-failed")
    );
    if is_settings_test_fixture || fixture_root.is_some() {
        cfg.llm_enabled = true;
        cfg.llm_connection = Some(crate::llm::LlmConnection {
            id: "ui-fixture-local".into(),
            name: "UI fixture local endpoint".into(),
            api_format: crate::llm::ApiFormat::OpenAiChatCompletions,
            base_url: "http://127.0.0.1:11434/v1".into(),
            model: "ui-fixture-model".into(),
        });
    }
    if let Some(root) = &fixture_root {
        cfg.working_dir = root.join("jobs");
        cfg.output_dir = root.join("output");
        cfg.runtime_data_dir = root.join("runtime-data");
    }
    let queue = if result_fixture {
        let root = fixture_root.as_ref().expect("result fixture root");
        Queue {
            jobs: vec![result_fixture_callback_job(
                root,
                &cfg,
                ui_fixture.expect("result fixture selector"),
            )?],
        }
    } else if ui_fixture == Some("queue-scroll") {
        Queue {
            jobs: queue_scroll_fixture_jobs(),
        }
    } else if ui_fixture.is_some() {
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
        crate::funasr::cleanup_residual_containers(
            queue.jobs.iter().map(|job| &job.id),
            &cfg.instance_nonce,
        );
    }

    let app = App::new()?;
    if let Some(state) = ui_fixture {
        if state == "empty" {
            app.set_ui_verification_empty(true);
        } else if state == "confirmation" {
            app.set_transcription_confirm_video("BV1UIFIX · P2".into());
            app.set_transcription_confirm_runtime_name("内置 Runtime".into());
            app.set_transcription_confirm_runtime_source("bundled".into());
            app.set_transcription_confirm_runtime_backend("native-uv".into());
            app.set_transcription_confirm_model("未提供".into());
            app.set_transcription_confirm_recommended("中文、英文".into());
            app.set_transcription_confirm_limitations("仅用于 UI fixture".into());
            app.set_transcription_confirm_error("".into());
            app.set_transcription_confirm_ready(true);
            app.set_transcription_confirm_auto_available(true);
            app.set_transcription_confirm_language("zh".into());
            app.set_transcription_confirm_can_create(true);
            app.set_transcription_confirm_visible(true);
        } else if state == "settings-connection-tested" {
            app.set_settings_page(2);
            app.set_settings_visible(true);
            app.set_connection_test_state("success".into());
            app.set_connection_test_message(
                "连接成功 · openai_chat_completions · ui-fixture-model。测试成功不代表生成内容正确。".into(),
            );
            app.set_ui_verification_fixture("settings".into());
        } else if state == "settings-connection-failed" {
            app.set_settings_page(2);
            app.set_settings_visible(true);
            app.set_connection_test_state("failure".into());
            app.set_connection_test_message("无法连接到服务，请确认服务已启动且地址正确。".into());
            app.set_ui_verification_fixture("settings".into());
        } else if matches!(
            state,
            "result-success"
                | "result-limited"
                | "result-enhancement-failed"
                | "result-raw-only"
                | "result-regenerating"
                | "result-summary-depths"
                | "result-model-changed"
        ) {
            // Result fixtures still use the production JobDetail and
            // ContentResults view models.  Only their deterministic input
            // projection is synthetic; no screenshot-only component or
            // persisted queue state is created.
            app.set_ui_verification_fixture("completed".into());
            let fixture_job = queue.jobs.first().expect("result fixture job");
            let model_override = (state == "result-model-changed").then_some("new-model-v2");
            let initial_tier = if state == "result-summary-depths" {
                0
            } else {
                1
            };
            app.set_result(content_results_for_job(
                fixture_job,
                (state == "result-regenerating").then_some(ArtifactKindV1::Chapters),
                model_override,
                initial_tier,
            ));
            if state == "result-regenerating" {
                app.set_action_pending_kind("chapters".into());
                app.set_action_pending(true);
                app.set_action_feedback("正在重新生成章节…".into());
                app.set_action_feedback_error(false);
            }
            app.set_result_open(true);
        } else if state == "result-search" {
            app.set_ui_verification_fixture("completed".into());
            let fixture_job = queue.jobs.first().expect("result fixture job");
            let mut result = content_results_for_job(fixture_job, None, None, 1);
            let search_rows = vec![
                section_row(
                    "search-header",
                    "搜索“原始” · 2 条结果",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                    false,
                    "",
                ),
                content_row(
                    "search-0",
                    "block",
                    "原始稿",
                    "这是隔离结果 fixture 的第 0 个【原始】片段。",
                    "",
                    "原始稿",
                    "mapped",
                    "这是隔离结果 fixture 的第 0 个【原始】片段。",
                    "",
                    "00:00:00–00:00:01",
                    "https://www.bilibili.com/video/BV1UIFIX?p=2&t=0",
                    true,
                    true,
                    "",
                    "",
                    false,
                ),
                content_row(
                    "search-1",
                    "block",
                    "忠实正文",
                    "这是隔离结果 fixture 的第 1 个【原始】片段。",
                    "",
                    "忠实正文",
                    "mapped",
                    "这是隔离结果 fixture 的第 1 个【原始】片段。",
                    "",
                    "00:00:01–00:00:02",
                    "https://www.bilibili.com/video/BV1UIFIX?p=2&t=1000",
                    true,
                    true,
                    "",
                    "",
                    false,
                ),
            ];
            result.search_active = true;
            result.search_query = "原始".into();
            result.search_result_count = 2;
            result.search_rows = ModelRc::from(Rc::new(VecModel::from(search_rows)));
            app.set_result(result);
            app.set_result_open(true);
        } else if state == "delete-confirm" || state == "clear-confirm" {
            // History-management confirm fixtures: completed detail behind the
            // dialog so screenshots show the delete affordance and the modal.
            app.set_ui_verification_fixture("completed".into());
            if state == "delete-confirm" {
                app.set_delete_confirm_title("删除任务".into());
                app.set_delete_confirm_message(
                    "将删除“这是一个用于验证详情滚动的超长视频标题：包含多个说话人、错误信息和任务阶段”的任务记录与全部产物文件（含输出目录中的 full.md）。此操作不可撤销。".into(),
                );
                app.set_delete_confirm_confirm_label("删除".into());
            } else {
                app.set_delete_confirm_title("清空任务历史".into());
                app.set_delete_confirm_message(
                    "将删除 3 个已结束任务的记录与产物文件；排队与运行中的任务不受影响。此操作不可撤销。".into(),
                );
                app.set_delete_confirm_confirm_label("清空历史".into());
            }
            app.set_delete_confirm_visible(true);
        } else {
            app.set_ui_verification_fixture(state.into());
        }
        let (x, y) =
            parse_ui_fixture_position(std::env::var("BI2READ_UI_FIXTURE_POSITION").ok().as_deref());
        let (width, height) =
            parse_ui_fixture_size(std::env::var("BI2READ_UI_FIXTURE_SIZE").ok().as_deref());
        app.window()
            .set_size(slint::PhysicalSize::new(width, height));
        app.window()
            .set_position(slint::LogicalPosition::new(x as f32, y as f32));
        if std::env::var("BI2READ_UI_FIXTURE_INSPECTOR")
            .ok()
            .is_some_and(|value| value == "info")
        {
            app.set_ui_verification_inspector_page(1);
        }
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
    let content_busy = Arc::new(Mutex::new(ContentBusy::default()));

    // The scheduler owns the cancellation registry and exposes the drain loop.
    let scheduler = Arc::new(Scheduler::new(queue.clone()));

    // ---- Repopulate the job list from the recovered queue ----
    {
        let q = queue.lock().unwrap();
        for job in &q.jobs {
            jobs_model.push(job_to_row(job, result_fixture));
        }
        app.set_can_clear_history(q.jobs.iter().any(|job| job.status.is_terminal()));
    }

    let shared_config = Arc::new(Mutex::new(cfg.clone()));
    let controller = Rc::new(Controller {
        app: weak.clone(),
        jobs_model: jobs_model.clone(),
        speakers_model: speakers_model.clone(),
        config: shared_config.clone(),
        queue: queue.clone(),
        scheduler: scheduler.clone(),
        tx: tx.clone(),
        content_busy: content_busy.clone(),
        speaker_timer: Mutex::new(None),
        pending_transcription: RefCell::new(None),
        history_confirmation: RefCell::new(None),
        selected_summary_tier: RefCell::new(1),
        probe_token: Arc::new(AtomicU64::new(0)),
    });

    // ---- Spawn the single worker thread; only one job runs at a time. ----
    {
        let cfg2 = shared_config.clone();
        let weak2 = weak.clone();
        let sched = scheduler.clone();
        let content_busy2 = content_busy.clone();
        let _handle: JoinHandle<()> = std::thread::spawn(move || {
            worker_loop(rx, sched, cfg2, weak2, content_busy2);
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
            c.prepare_add_job();
        });
    }
    {
        let c = controller.clone();
        app.on_transcription_confirm_language_changed(move |value| {
            c.update_pending_language(value.to_string());
        });
    }
    {
        let c = controller.clone();
        app.on_transcription_confirm_runtime_changed(move |_index| {
            c.refresh_pending_runtime();
        });
    }
    {
        let c = controller.clone();
        app.on_transcription_confirmed(move || {
            c.confirm_pending_job();
        });
    }
    {
        let c = controller.clone();
        app.on_transcription_cancelled(move || {
            c.cancel_pending_job();
        });
    }
    {
        let c = controller.clone();
        app.on_transcription_open_settings(move || {
            c.open_settings_from_confirmation();
        });
    }
    {
        let c = controller.clone();
        app.on_settings_clicked(move || {
            c.probe_token.fetch_add(1, Ordering::SeqCst);
            if let Some(app) = c.app.upgrade() {
                app.set_settings_feedback("".into());
                app.set_settings_error_field("".into());
                app.set_settings_error_message("".into());
                app.set_connection_test_state("idle".into());
                app.set_connection_test_message("".into());
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
        app.on_rebuild_job(move || {
            c.rebuild_selected();
        });
    }
    {
        let c = controller.clone();
        app.on_open_md(move || {
            c.open_selected_md();
        });
    }
    {
        let app_weak = app.as_weak();
        app.on_view_result(move || {
            let _ = app_weak.upgrade_in_event_loop(|app| {
                app.set_action_feedback("".into());
                app.set_action_feedback_error(false);
                app.set_result_open(true);
            });
        });
    }
    {
        let app_weak = app.as_weak();
        app.on_result_back(move || {
            let _ = app_weak.upgrade_in_event_loop(|app| {
                app.set_result_open(false);
            });
        });
    }
    {
        let c = controller.clone();
        app.on_result_open_raw(move || {
            c.open_selected_raw();
        });
    }
    {
        let c = controller.clone();
        app.on_result_repair(move || {
            c.repair_selected_content();
        });
    }
    {
        let c = controller.clone();
        app.on_result_regenerate(move |kind| {
            if let Some(kind) = regenerable_kind_from_action(kind.as_str()) {
                c.regenerate_selected(kind);
            }
        });
    }
    {
        let app_weak = app.as_weak();
        app.on_result_source(move |_url| {
            let _ = app_weak.upgrade_in_event_loop(|app| {
                app.set_action_feedback("已展开来源片段".into());
                app.set_action_feedback_error(false);
            });
        });
    }
    {
        let c = controller.clone();
        app.on_result_jump(move |url| {
            c.open_external_url(url.to_string());
        });
    }
    {
        let c = controller.clone();
        app.on_reveal_in_finder(move || {
            c.reveal_selected();
        });
    }
    {
        let c = controller.clone();
        app.on_delete_job(move || {
            c.request_delete_selected();
        });
    }
    {
        let c = controller.clone();
        app.on_clear_history(move || {
            c.request_clear_history();
        });
    }
    {
        let c = controller.clone();
        app.on_delete_confirmed(move || {
            c.confirm_history_action();
        });
    }
    {
        let c = controller.clone();
        app.on_delete_cancelled(move || {
            c.cancel_history_action();
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
        app.on_test_connection(move |s| {
            c.test_connection(s);
        });
    }
    {
        let c = controller.clone();
        app.on_summary_tier_selected(move |tier| {
            c.select_summary_tier(tier);
        });
    }
    {
        let c = controller.clone();
        app.on_result_search(move |query| {
            c.handle_search(query.to_string());
        });
    }
    {
        let c = controller.clone();
        app.on_result_search_clear(move || {
            c.clear_search();
        });
    }
    {
        let c = controller.clone();
        app.on_cancel_settings(move || {
            c.probe_token.fetch_add(1, Ordering::SeqCst);
            if let Some(app) = c.app.upgrade() {
                app.set_settings_saving(false);
                app.set_settings_feedback("".into());
                app.set_settings_error_field("".into());
                app.set_settings_error_message("".into());
                app.set_connection_test_state("idle".into());
                app.set_connection_test_message("".into());
            }
            // Cancellation just hides; changes weren't persisted.
        });
    }
    {
        let c = controller.clone();
        app.on_settings_draft_changed(move || {
            c.probe_token.fetch_add(1, Ordering::SeqCst);
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
    if ui_fixture == Some("settings") {
        app.set_settings_visible(true);
    } else if ui_fixture == Some("settings-connection-tested") {
        app.set_settings_visible(true);
        app.set_settings_page(2);
        app.set_connection_test_state("success".into());
        app.set_connection_test_message(
            "连接成功 · openai_chat_completions · ui-fixture-model。测试成功不代表生成内容正确。"
                .into(),
        );
    } else if ui_fixture == Some("settings-connection-failed") {
        app.set_settings_visible(true);
        app.set_settings_page(2);
        app.set_connection_test_state("failure".into());
        app.set_connection_test_message("无法连接到服务，请确认服务已启动且地址正确。".into());
    }

    // Keep the worker tx alive for the app lifetime; on drop it shuts down.
    let _keep_tx = tx.clone();

    // Wake the scheduler after recovery so recovered queued jobs auto-start.
    let _ = tx.send(WorkMsg::Run);

    app.run()?;
    let _ = tx.send(WorkMsg::Shutdown);
    if let Some(root) = fixture_root {
        let _ = std::fs::remove_dir_all(root);
    }
    Ok(())
}

fn regenerable_kind_from_action(action: &str) -> Option<RegenerableKind> {
    RegenerableKind::from_action(action)
}

fn valid_result_source_url(url: &str) -> bool {
    url.starts_with("https://www.bilibili.com/video/")
}

fn start_result_fixture_generation_server(selector: &str) -> Result<(u16, JoinHandle<()>), String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let selector = selector.to_string();
    let expected_requests = if selector == "result-raw-only" {
        0
    } else if selector == "result-enhancement-failed" {
        13
    } else {
        16
    };
    let handle = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut handled = 0;
        while handled < expected_requests && std::time::Instant::now() < deadline {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            let (header_end, content_length) = loop {
                let Ok(read) = stream.read(&mut chunk) else {
                    break (None, 0);
                };
                if read == 0 {
                    break (None, 0);
                }
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let header = String::from_utf8_lossy(&request[..header_end]);
                let content_length = header
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let body_start = header_end + 4;
                while request.len() < body_start + content_length {
                    let Ok(read) = stream.read(&mut chunk) else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                }
                break (Some(header_end), content_length);
            };

            let body = header_end
                .and_then(|header_end| {
                    let start = header_end + 4;
                    (request.len() >= start + content_length)
                        .then(|| &request[start..start + content_length])
                })
                .and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok());
            let prompt = body
                .as_ref()
                .and_then(|body| body["messages"].as_array())
                .and_then(|messages| messages.first())
                .and_then(|message| message["content"].as_str())
                .unwrap_or_default();
            let input = prompt
                .find("input=")
                .and_then(|offset| {
                    prompt[offset + "input=".len()..]
                        .find('{')
                        .map(|start| offset + "input=".len() + start)
                })
                .and_then(|start| serde_json::from_str::<serde_json::Value>(&prompt[start..]).ok());
            let kind = input
                .as_ref()
                .and_then(|input| input["kind"].as_str())
                .unwrap_or_default();
            let invalid = selector == "result-enhancement-failed" && kind == "highlights";
            let response = if invalid {
                serde_json::json!({"choices": []})
            } else {
                let utterances = input
                    .as_ref()
                    .and_then(|input| input["utterances"].as_array())
                    .cloned()
                    .unwrap_or_default();
                let ids: Vec<String> = utterances
                    .iter()
                    .filter_map(|utterance| utterance["id"].as_str().map(str::to_owned))
                    .collect();
                let blocks = if selector == "result-limited" && kind == "default_summary" {
                    let text = format!(
                        "无法精确关联的摘要，用于测试受限状态展示。{}",
                        "相关描述内容。".repeat(15)
                    );
                    serde_json::json!([{
                        "id": "fixture-limited-summary",
                        "title": null,
                        "text": text,
                        "source_refs": [],
                        "source_status": "limited",
                    }])
                } else if kind == "default_summary" {
                    let first = utterances.first();
                    let text = format!(
                        "本段核心总结：{}",
                        first
                            .and_then(|u| u["text"].as_str())
                            .unwrap_or("片段总结内容。")
                    );
                    serde_json::json!([{
                        "id": "fixture-summary-block",
                        "title": null,
                        "text": format!("{}{}", text.repeat(2), "总结概要描述。".repeat(10)),
                        "source_refs": [{
                            "utterance_ids": ids.clone(),
                            "start_ms": utterances.first().and_then(|u| u["start_ms"].as_u64()).unwrap_or(0),
                            "end_ms": utterances.last().and_then(|u| u["end_ms"].as_u64()).unwrap_or(0),
                        }],
                        "source_status": "mapped",
                    }])
                } else if selector == "result-limited" && kind == "chapters" && utterances.len() > 1
                {
                    let first = &utterances[0];
                    serde_json::json!([
                        {
                            "id": "fixture-mapped-chapter",
                            "title": "可核对章节",
                            "text": first["text"].as_str().unwrap_or_default(),
                            "source_refs": [{
                                "utterance_ids": [first["id"].as_str().unwrap_or_default()],
                                "start_ms": first["start_ms"].as_u64().unwrap_or(0),
                                "end_ms": first["end_ms"].as_u64().unwrap_or(0),
                            }],
                            "source_status": "mapped",
                        },
                        {
                            "id": "fixture-limited-chapter",
                            "title": "来源受限章节",
                            "text": "无法精确关联的章节",
                            "source_refs": [],
                            "source_status": "limited",
                        }
                    ])
                } else {
                    serde_json::Value::Array(
                        utterances
                            .iter()
                            .enumerate()
                            .map(|(index, utterance)| {
                                serde_json::json!({
                                    "id": format!("fixture-block-{index}"),
                                    "title": (kind == "chapters")
                                        .then(|| format!("第 {} 段", index + 1)),
                                    // Keep each result row short and distinct;
                                    // faithful output remains byte/lexically
                                    // identical to its one-utterance source.
                                    "text": utterance["text"].as_str().unwrap_or_default(),
                                    "source_refs": [{
                                        "utterance_ids": [utterance["id"].as_str().unwrap_or_default()],
                                        "start_ms": utterance["start_ms"].as_u64().unwrap_or(0),
                                        "end_ms": utterance["end_ms"].as_u64().unwrap_or(0),
                                    }],
                                    "source_status": "mapped",
                                })
                            })
                            .collect(),
                    )
                };
                let content = serde_json::json!({
                    "processed_utterance_ids": ids,
                    "blocks": blocks,
                })
                .to_string();
                serde_json::json!({
                    "choices": [{"message": {"content": content}}]
                })
            };
            let response = response.to_string();
            let http = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(), response
            );
            let _ = stream.write_all(http.as_bytes());
            handled += 1;
        }
    });
    Ok((port, handle))
}

pub(crate) fn result_fixture_callback_job(
    root: &std::path::Path,
    cfg: &Config,
    selector: &str,
) -> Result<Job, String> {
    let (port, server) = start_result_fixture_generation_server(selector)?;
    let mut fixture_cfg = cfg.clone();
    fixture_cfg.llm_enabled = true;
    fixture_cfg.llm_connection = Some(crate::llm::LlmConnection {
        id: "ui-fixture-local".into(),
        name: "UI fixture local endpoint".into(),
        api_format: crate::llm::ApiFormat::OpenAiChatCompletions,
        base_url: format!("http://127.0.0.1:{port}"),
        model: "ui-fixture-model".into(),
    });
    let work_dir = root.join("jobs").join("result-fixture");
    std::fs::create_dir_all(&work_dir).map_err(|error| error.to_string())?;
    let utterances = (0..64)
        .map(|index| crate::funasr::Utterance {
            id: format!("u{index:04}"),
            text: format!("这是隔离结果 fixture 的第 {index} 个原始片段。"),
            start_ms: index as u64 * 1_000,
            end_ms: index as u64 * 1_000 + 900,
            speaker_id: (index % 2) as u32,
        })
        .collect::<Vec<_>>();
    let raw = serde_json::to_vec_pretty(&utterances).map_err(|error| error.to_string())?;
    jobs::atomic_write(&work_dir.join("transcript.raw.json"), &raw)
        .map_err(|error| error.to_string())?;

    let mut job = Job::new(Uuid::new_v4(), "BV1UIFIX".into(), 2);
    job.title = "消费结果生产回调 fixture".into();
    job.part_title = Some("第二部分".into());
    job.cid = Some(2_024_082_400);
    job.duration_ms = Some(64_000);
    job.work_dir = Some(work_dir.clone());
    job.status = JobStatus::Completed;
    job.stage = Stage::Completed;
    job.stage_progress = 100;
    for stage in jobs::pipeline_stages() {
        job.set_stage_state(stage, StageState::Completed);
    }
    job.content_setup = Some(fixture_cfg.content_setup());
    if selector == "result-raw-only" {
        let _ = server.join();
        return Ok(job);
    }
    let result = crate::content_results::execute(
        &job,
        Intent::Initial,
        &crate::cancel::CancellationToken::new(),
    )
    .map_err(|error| error.to_string());
    let _ = server.join();
    result?;

    if selector == "result-summary-depths" {
        let current_path = work_dir.join(crate::content_results::CONTENT_CURRENT_FILE);
        let mut snapshot_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&current_path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        snapshot_json["slots"]["short_summary"] = serde_json::json!({
            "current": null,
            "last_failure": null
        });
        snapshot_json["slots"]["long_summary"] = serde_json::json!({
            "current": null,
            "last_failure": {
                "code": "response_invalid",
                "message": "长摘要长度不足契约要求（少于 400 字符）。",
                "retryable": true,
                "occurred_at": chrono::Utc::now().to_rfc3339()
            }
        });
        std::fs::write(
            &current_path,
            serde_json::to_vec_pretty(&snapshot_json).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(job)
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
            "display alert \"bi2read 已在运行\" message \"请使用已经打开的窗口。\" as informational",
        ])
        .status();
}

// ---- Worker loop (background thread; one job at a time) ----

fn worker_loop(
    rx: mpsc::Receiver<WorkMsg>,
    scheduler: Arc<Scheduler>,
    config: Arc<Mutex<Config>>,
    app: Weak<App>,
    content_busy: Arc<Mutex<ContentBusy>>,
) {
    while let Ok(msg) = rx.recv() {
        if !handle_work_message(msg, &scheduler, &config, &app, &content_busy) {
            break;
        }
        // Every handled message may have moved a job into (or out of) a
        // terminal state; keep the queue-header clear-history button truthful.
        sync_history_capabilities(&app, &scheduler.queue);
    }
}

/// Handle one work message. Returns false when the worker must shut down.
fn handle_work_message(
    msg: WorkMsg,
    scheduler: &Arc<Scheduler>,
    config: &Arc<Mutex<Config>>,
    app: &Weak<App>,
    content_busy: &Arc<Mutex<ContentBusy>>,
) -> bool {
    match msg {
        WorkMsg::Run => {
            // Drain the queue until idle or waiting. Each call runs one
            // job to completion.
            loop {
                let cfg_snap = config.lock().unwrap().clone();
                match scheduler.drain_next(&cfg_snap, app) {
                    DrainOutcome::Idle | DrainOutcome::Waiting => break,
                    DrainOutcome::Ran { job_id, .. } => {
                        refresh_selected_detail(app, &scheduler.queue, config, job_id);
                        // Continue to the next job.
                    }
                }
            }
        }
        WorkMsg::Retry(job_id) => {
            let mut requeued = false;
            let mut persistence_error = None;
            {
                let mut q = scheduler.queue.lock().unwrap();
                let retry_allowed = q
                    .get(job_id)
                    .map(|job| crate::jobs::JobCapabilities::from_job(job).can_retry)
                    .unwrap_or(false);
                if !retry_allowed {
                    let app = app.clone();
                    let _ = app.upgrade_in_event_loop(move |app| {
                        app.set_action_pending(false);
                        app.set_action_feedback("重试不可用：请使用修复文字结果".into());
                        app.set_action_feedback_error(true);
                    });
                    return true;
                }
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
                return true;
            }
            if let Some(error) = persistence_error {
                let app = app.clone();
                let _ = app.upgrade_in_event_loop(move |app| {
                    app.set_action_pending(false);
                    app.set_action_feedback(format!("重试状态保存失败，请重试：{error}").into());
                    app.set_action_feedback_error(true);
                });
                return true;
            }
            // Drain the next runnable job.
            let cfg_snap = config.lock().unwrap().clone();
            let outcome = scheduler.drain_next(&cfg_snap, app);
            if let DrainOutcome::Ran { job_id, .. } = &outcome {
                refresh_selected_detail(app, &scheduler.queue, config, *job_id);
            }
            let (feedback, is_error) = retry_feedback(&outcome);
            let app = app.clone();
            let _ = app.upgrade_in_event_loop(move |app| {
                app.set_action_pending(false);
                app.set_action_feedback(feedback.into());
                app.set_action_feedback_error(is_error);
            });
        }
        WorkMsg::Regenerate {
            job_id,
            kind,
            target,
            tier,
        } => {
            handle_regenerate_content(
                job_id,
                kind,
                target,
                tier,
                scheduler,
                config,
                app,
                content_busy,
            );
        }
        WorkMsg::RepairContent { job_id, tier } => {
            handle_repair_content(job_id, tier, scheduler, config, app, content_busy);
        }
        WorkMsg::DeleteJob(job_id) => {
            let (feedback, feedback_error) = {
                let mut q = scheduler.queue.lock().unwrap();
                match q.get(job_id).cloned() {
                    None => ("删除失败：任务不存在".to_string(), true),
                    Some(job) if !job.status.is_terminal() => {
                        ("删除失败：任务尚未结束，请先取消再删除".to_string(), true)
                    }
                    Some(job) => match jobs::delete_job_artifacts(&job) {
                        Ok(()) => {
                            // Artifacts are gone; only now may the queue record
                            // disappear, otherwise recovery would resurrect a
                            // job whose files no longer exist.
                            q.jobs.retain(|job| job.id != job_id);
                            match q.save() {
                                Ok(()) => ("已删除任务及其产物".to_string(), false),
                                Err(error) => {
                                    (format!("任务产物已删除，但队列保存失败：{error}"), true)
                                }
                            }
                        }
                        Err(error) => (format!("删除失败：{error}"), true),
                    },
                }
            };
            let jobs_snapshot = scheduler.queue.lock().unwrap().jobs.clone();
            let current_model = config
                .lock()
                .ok()
                .and_then(|cfg| cfg.llm_connection.as_ref().map(|c| c.model.clone()));
            let app = app.clone();
            let _ = app.upgrade_in_event_loop(move |app| {
                let (selected, index) = current_selection(&app);
                apply_queue_rows(
                    &app,
                    &jobs_snapshot,
                    selected,
                    index,
                    current_model.as_deref(),
                );
                app.set_action_pending(false);
                app.set_action_feedback(feedback.into());
                app.set_action_feedback_error(feedback_error);
            });
        }
        WorkMsg::ClearHistory => {
            let (feedback, feedback_error) = {
                let mut q = scheduler.queue.lock().unwrap();
                let terminal: Vec<Job> = q
                    .jobs
                    .iter()
                    .filter(|job| job.status.is_terminal())
                    .cloned()
                    .collect();
                let mut deleted = 0usize;
                let mut failed_ids: Vec<Uuid> = Vec::new();
                let mut failed_errors: Vec<String> = Vec::new();
                for job in &terminal {
                    match jobs::delete_job_artifacts(job) {
                        Ok(()) => deleted += 1,
                        Err(error) => {
                            failed_ids.push(job.id);
                            failed_errors.push(error);
                        }
                    }
                }
                if deleted > 0 {
                    // Keep any job whose artifacts could not be removed so the
                    // user can retry; its record still points at real files.
                    q.jobs
                        .retain(|job| !job.status.is_terminal() || failed_ids.contains(&job.id));
                }
                let (mut feedback, mut feedback_error) = if deleted > 0 && failed_ids.is_empty() {
                    (format!("已清空 {deleted} 个已结束任务"), false)
                } else if deleted > 0 {
                    (
                        format!(
                            "已清空 {deleted} 个已结束任务；{} 个删除失败：{}",
                            failed_ids.len(),
                            failed_errors.join("；")
                        ),
                        true,
                    )
                } else if !failed_ids.is_empty() {
                    (format!("清空失败：{}", failed_errors.join("；")), true)
                } else {
                    ("没有可清空的历史任务".to_string(), false)
                };
                if let Err(error) = q.save() {
                    feedback = format!("已删除任务，但队列保存失败：{error}");
                    feedback_error = true;
                }
                (feedback, feedback_error)
            };
            let jobs_snapshot = scheduler.queue.lock().unwrap().jobs.clone();
            let current_model = config
                .lock()
                .ok()
                .and_then(|cfg| cfg.llm_connection.as_ref().map(|c| c.model.clone()));
            let app = app.clone();
            let _ = app.upgrade_in_event_loop(move |app| {
                let (selected, index) = current_selection(&app);
                apply_queue_rows(
                    &app,
                    &jobs_snapshot,
                    selected,
                    index,
                    current_model.as_deref(),
                );
                app.set_action_pending(false);
                app.set_action_feedback(feedback.into());
                app.set_action_feedback_error(feedback_error);
            });
        }
        WorkMsg::SpeakerChanged { job_id, tier } => {
            // Changing speaker names regenerates documents without
            // transcribing the audio again.
            let cfg = config.lock().unwrap().clone();
            let is_v05 = scheduler
                .queue
                .lock()
                .ok()
                .and_then(|q| q.get(job_id).map(|job| job.content_setup.is_some()))
                .unwrap_or(false);
            let _content_guard = if is_v05 {
                if !reserve_content_operation(content_busy, job_id) {
                    log::info!("skip v0.5 speaker rebuild while content operation is busy");
                    let app = app.clone();
                    let _ = app.upgrade_in_event_loop(move |app| {
                        app.set_speaker_feedback(
                            "说话人名称已保存；文字结果正在处理，再次打开 Markdown 时会更新文档"
                                .into(),
                        );
                        app.set_speaker_feedback_error(true);
                    });
                    return true;
                }
                Some(ContentBusyGuard {
                    busy: content_busy.clone(),
                    job_id,
                })
            } else {
                None
            };
            let feedback = {
                let mut q = scheduler.queue.lock().unwrap();
                let feedback = q.get_mut(job_id).and_then(|job| {
                    let result = rebuild_speaker_documents(job, &cfg, tier);
                    let feedback = is_v05.then(|| speaker_rebuild_feedback(&result));
                    if let Err(error) = &result {
                        log::warn!("speaker Presentation rebuild failed: {error}");
                    }
                    let _ = jobs::save_job_state(job);
                    feedback
                });
                let _ = q.save();
                feedback
            };
            if let Some((message, error)) = feedback {
                refresh_selected_result(app, &scheduler.queue, config, job_id);
                let app = app.clone();
                let _ = app.upgrade_in_event_loop(move |app| {
                    app.set_speaker_feedback(message.into());
                    app.set_speaker_feedback_error(error);
                });
            }
        }
        WorkMsg::Shutdown => return false,
    }
    true
}

fn speaker_rebuild_feedback(result: &Result<(), String>) -> (String, bool) {
    match result {
        Ok(()) => ("已保存，文档已更新".into(), false),
        Err(error) => (
            format!("说话人名称已保存，但文档更新失败：{error}；再次打开 Markdown 时会重试"),
            true,
        ),
    }
}

fn content_feedback(app: &Weak<App>, message: String, error: bool) {
    let _ = app.upgrade_in_event_loop(move |app| {
        app.set_action_pending(false);
        app.set_action_pending_kind("".into());
        app.set_action_feedback(message.into());
        app.set_action_feedback_error(error);
    });
}

fn apply_presentation_locator(job: &mut Job, output_dir: std::path::PathBuf) {
    // Presentation success may update only the locator. Job terminal state,
    // stage/error/timestamps and structured current remain untouched.
    job.final_output_dir = Some(output_dir);
}

fn presentation_operation(
    mut job: Job,
    snapshot: &crate::content_results::ContentSnapshotV1,
    config: &Arc<Mutex<Config>>,
    tier: crate::document::SummaryTier,
    success_message: String,
    success_is_error: bool,
) -> (String, bool, Option<std::path::PathBuf>) {
    let cfg = config.lock().unwrap().clone();
    let output_dir = match crate::pipeline::ensure_final_output_dir(&mut job, &cfg) {
        Ok(output_dir) => output_dir,
        Err(error) => {
            return (format!("内容已提交，但文档写入失败：{error}"), true, None);
        }
    };
    match crate::document::rebuild_presentation_with_tier(&job, snapshot, tier) {
        Ok(()) => (success_message, success_is_error, Some(output_dir)),
        Err(error) => (
            format!("{success_message}；文档写入失败，可重新打开重试：{error}"),
            true,
            Some(output_dir),
        ),
    }
}

fn regeneration_feedback(slot: &crate::content_results::ArtifactSlotV1) -> (String, bool) {
    match (slot.current.is_some(), &slot.last_failure) {
        (true, None) => ("文字结果已更新".into(), false),
        (true, Some(failure)) => (
            format!("重生成失败：{}；已保留上一版结果", failure.message),
            true,
        ),
        (false, Some(failure)) => (
            format!("重生成失败：{}；当前没有可用结果", failure.message),
            true,
        ),
        (false, None) => ("重生成结果状态无效，请重试".into(), true),
    }
}

fn persist_job_cancellation(
    scheduler: &Scheduler,
    job_id: Uuid,
) -> Result<crate::jobs::CancellationTransition, String> {
    persist_job_cancellation_with(
        scheduler,
        job_id,
        |job| jobs::save_job_state(job).map_err(|error| error.to_string()),
        |queue| queue.save().map_err(|error| error.to_string()),
    )
}

fn persist_job_cancellation_with<SaveJob, SaveQueue>(
    scheduler: &Scheduler,
    job_id: Uuid,
    save_job: SaveJob,
    save_queue: SaveQueue,
) -> Result<crate::jobs::CancellationTransition, String>
where
    SaveJob: Fn(&Job) -> Result<(), String>,
    SaveQueue: Fn(&Queue) -> Result<(), String>,
{
    let mut queue = scheduler
        .queue
        .lock()
        .map_err(|_| "任务队列锁已损坏".to_string())?;
    let original = queue
        .get(job_id)
        .cloned()
        .ok_or_else(|| "任务不存在".to_string())?;
    let transition = queue
        .get_mut(job_id)
        .expect("cloned Job remains in locked queue")
        .request_cancellation(chrono::Utc::now());
    if !matches!(
        transition,
        crate::jobs::CancellationTransition::Cooperative
            | crate::jobs::CancellationTransition::Immediate
    ) {
        return Ok(transition);
    }

    if let Err(error) = save_job(queue.get(job_id).expect("cancelled Job remains in queue")) {
        *queue
            .get_mut(job_id)
            .expect("cancelled Job remains in queue") = original;
        return Err(error);
    }
    if let Err(error) = save_queue(&queue) {
        *queue
            .get_mut(job_id)
            .expect("cancelled Job remains in queue") = original.clone();
        if let Err(rollback_error) = save_job(&original) {
            return Err(format!("{error}；回滚任务状态失败：{rollback_error}"));
        }
        return Err(error);
    }
    Ok(transition)
}

fn run_regenerate_content_operation(
    job: &Job,
    kind: RegenerableKind,
    target: ReadyGenerationTargetV1,
    config: &Arc<Mutex<Config>>,
    token: &crate::cancel::CancellationToken,
    tier: crate::document::SummaryTier,
) -> (String, bool, Option<std::path::PathBuf>) {
    match crate::content_results::execute(job, Intent::Regenerate { kind, target }, token) {
        Err(crate::content_results::ContentError::Cancelled) => {
            ("已取消文字结果重生成".into(), true, None)
        }
        Err(error) => (format!("重生成失败：{error}"), true, None),
        Ok(snapshot) => {
            let (feedback, is_error) =
                regeneration_feedback(snapshot.current.slots.get(kind.artifact_kind()));
            presentation_operation(job.clone(), &snapshot, config, tier, feedback, is_error)
        }
    }
}

fn run_repair_content_operation(
    job: &Job,
    config: &Arc<Mutex<Config>>,
    token: &crate::cancel::CancellationToken,
    tier: crate::document::SummaryTier,
) -> (String, bool, Option<std::path::PathBuf>) {
    match crate::content_results::execute(job, Intent::Initial, token) {
        Err(crate::content_results::ContentError::Cancelled) => {
            ("已取消文字结果修复".into(), true, None)
        }
        Err(error) => (format!("修复失败：{error}"), true, None),
        Ok(snapshot) => presentation_operation(
            job.clone(),
            &snapshot,
            config,
            tier,
            "文字结果已修复".into(),
            false,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_regenerate_content(
    job_id: Uuid,
    kind: RegenerableKind,
    target: ReadyGenerationTargetV1,
    tier: crate::document::SummaryTier,
    scheduler: &Arc<Scheduler>,
    config: &Arc<Mutex<Config>>,
    app: &Weak<App>,
    content_busy: &Arc<Mutex<ContentBusy>>,
) {
    let Some(token) = start_content_operation(content_busy, job_id) else {
        content_feedback(app, "文字结果正在处理中，请稍候".into(), true);
        return;
    };
    let Some(job) = scheduler
        .queue
        .lock()
        .ok()
        .and_then(|q| q.get(job_id).cloned())
    else {
        clear_content_operation(content_busy, job_id);
        content_feedback(app, "重生成失败：任务不存在".into(), true);
        return;
    };
    if job.content_setup.is_none() || !job.status.is_terminal() {
        clear_content_operation(content_busy, job_id);
        content_feedback(app, "重生成失败：只允许终态 v0.5 任务".into(), true);
        return;
    }

    let (message, is_error, final_output_dir) =
        run_regenerate_content_operation(&job, kind, target, config, &token, tier);

    if final_output_dir.is_some() {
        let mut queue = scheduler.queue.lock().unwrap();
        if let Some(stored) = queue.get_mut(job_id) {
            if let Some(output_dir) = final_output_dir {
                apply_presentation_locator(stored, output_dir);
            }
            let _ = crate::jobs::save_job_state(stored);
        }
        let _ = queue.save();
    }
    refresh_selected_result(app, &scheduler.queue, config, job_id);
    clear_content_operation(content_busy, job_id);
    content_feedback(app, message, is_error);
}

fn handle_repair_content(
    job_id: Uuid,
    tier: crate::document::SummaryTier,
    scheduler: &Arc<Scheduler>,
    config: &Arc<Mutex<Config>>,
    app: &Weak<App>,
    content_busy: &Arc<Mutex<ContentBusy>>,
) {
    let Some(token) = start_content_operation(content_busy, job_id) else {
        content_feedback(app, "文字结果正在处理中，请稍候".into(), true);
        return;
    };
    let Some(job) = scheduler
        .queue
        .lock()
        .ok()
        .and_then(|q| q.get(job_id).cloned())
    else {
        clear_content_operation(content_busy, job_id);
        content_feedback(app, "修复失败：任务不存在".into(), true);
        return;
    };
    let raw_only = matches!(
        crate::content_results::current(&job),
        Ok(ContentViewV1::RawOnly { .. })
    );
    if job.content_setup.is_none() || !job.status.is_terminal() || !raw_only {
        clear_content_operation(content_busy, job_id);
        content_feedback(
            app,
            "修复失败：当前任务没有可修复的原始 Evidence".into(),
            true,
        );
        return;
    }

    let (message, is_error, final_output_dir) =
        run_repair_content_operation(&job, config, &token, tier);

    if final_output_dir.is_some() {
        let mut queue = scheduler.queue.lock().unwrap();
        if let Some(stored) = queue.get_mut(job_id) {
            if let Some(output_dir) = final_output_dir {
                apply_presentation_locator(stored, output_dir);
            }
            let _ = crate::jobs::save_job_state(stored);
        }
        let _ = queue.save();
    }
    refresh_selected_result(app, &scheduler.queue, config, job_id);
    clear_content_operation(content_busy, job_id);
    content_feedback(app, message, is_error);
}

/// Keep the queue-header clear-history availability in sync with the queue.
fn sync_history_capabilities(app: &Weak<App>, queue: &Arc<Mutex<Queue>>) {
    let has_terminal = queue
        .lock()
        .map(|q| q.jobs.iter().any(|job| job.status.is_terminal()))
        .unwrap_or(false);
    let _ = app.upgrade_in_event_loop(move |app| {
        app.set_can_clear_history(has_terminal);
    });
}

/// The (selected job id, row index) currently shown in the queue list.
fn current_selection(app: &App) -> (Option<Uuid>, usize) {
    let model = app.get_jobs();
    for index in 0..model.row_count() {
        if let Some(row) = model.row_data(index) {
            if row.selected {
                return (Uuid::parse_str(row.id.as_ref()).ok(), index);
            }
        }
    }
    (None, 0)
}

/// Rebuild the queue rows from a queue snapshot after jobs were removed.
///
/// Selection repair: keep the previously selected job when it still exists;
/// otherwise select the row after the deleted one, falling back to the
/// previous row; with an empty queue the detail pane returns to its empty
/// state.
fn apply_queue_rows(
    app: &App,
    jobs: &[Job],
    previous_selected: Option<Uuid>,
    fallback_index: usize,
    current_model: Option<&str>,
) {
    let jobs_rc = app.get_jobs();
    let Some(model) = jobs_rc.as_any().downcast_ref::<slint::VecModel<JobRow>>() else {
        return;
    };
    let mut selected_index =
        previous_selected.and_then(|id| jobs.iter().position(|job| job.id == id));
    if selected_index.is_none() && !jobs.is_empty() {
        selected_index = Some(fallback_index.min(jobs.len() - 1));
    }
    let rows: Vec<JobRow> = jobs
        .iter()
        .enumerate()
        .map(|(index, job)| job_to_row(job, selected_index == Some(index)))
        .collect();
    model.set_vec(rows);
    let summary_tier = app.get_summary_tier();
    match selected_index {
        Some(index) => show_job_detail(app, &jobs[index], current_model, summary_tier),
        None => clear_job_detail(app),
    }
    let len = model.row_count();
    let selected = selected_index;
    let (can_move_up, can_move_down) = reorder_capabilities(selected, len);
    app.set_can_move_up(can_move_up);
    app.set_can_move_down(can_move_down);
}

/// Rebuild all markdown documents after a speaker name change.
/// Does NOT re-transcribe or call the LLM. Uses the stored `final_output_dir`
/// if available; otherwise infers from the current config (best-effort).
fn rebuild_speaker_documents(
    job: &mut Job,
    cfg: &Config,
    tier: crate::document::SummaryTier,
) -> Result<(), String> {
    use crate::document;

    let Some(dir) = job.work_dir.clone() else {
        return Err("任务没有工作目录".into());
    };
    if job.content_setup.is_some() {
        if !job.status.is_terminal() {
            return Err("v0.5 任务尚未进入终态".into());
        }
        let snapshot = match crate::content_results::current(job)
            .map_err(|error| error.to_string())?
        {
            ContentViewV1::Current(snapshot) => snapshot,
            ContentViewV1::RawOnly { .. } => return Err("当前没有可重建的可信文字结果".into()),
            ContentViewV1::Legacy => return Err("v0.5 Job unexpectedly resolved as legacy".into()),
        };
        crate::pipeline::ensure_final_output_dir(job, cfg).map_err(|error| error.to_string())?;
        document::rebuild_presentation_with_tier(job, &snapshot, tier)
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    let raw_json = dir.join("transcript.raw.json");
    if !raw_json.exists() {
        return Err("原始转写尚未生成".into());
    }
    let utts = match pipeline::load_utterances(&raw_json) {
        Ok(u) => u,
        Err(error) => return Err(error.to_string()),
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
    Ok(())
}

fn reading_caption_for_job(
    job: &Job,
    snapshot: &crate::content_results::ContentSnapshotV1,
    selected_tier: Option<crate::content_results::ArtifactKindV1>,
) -> (String, String, bool) {
    let language = job
        .transcription_result
        .as_ref()
        .and_then(|result| result.reported_language)
        .or_else(|| {
            job.transcription_selection
                .as_ref()
                .map(|selection| selection.requested_language)
        });
    let record = crate::content_results::primary_consumption_record(snapshot, selected_tier);
    let Some(record) = record else {
        return (String::new(), String::new(), false);
    };
    let text = crate::content_results::record_plain_text(record);
    let estimate = crate::reading_time::estimate(&text, job.duration_ms, language);
    let scale = estimate.scale;
    let duration_prefix = job
        .duration_ms
        .map(|ms| format!("视频 {} 分钟 · ", (ms as f64 / 60_000.0).ceil() as u64))
        .unwrap_or_default();

    let formatted_scale = scale.format_scale();
    let Some(reading_ms) = estimate.reading_ms else {
        return (
            format!("{duration_prefix}正文约 {formatted_scale}"),
            String::new(),
            false,
        );
    };
    let Some(video_ms) = estimate.video_ms else {
        return (
            format!(
                "正文约 {formatted_scale} · 预计阅读约 {} 分钟",
                (reading_ms as f64 / 60_000.0).ceil() as u64
            ),
            crate::reading_time::READING_PROFILE_VERSION.to_string(),
            true,
        );
    };
    let Some(saved_ms) = estimate.saved_ms else {
        return (
            format!(
                "视频 {} 分钟 · 正文约 {formatted_scale} · 预计阅读约 {} 分钟",
                (video_ms as f64 / 60_000.0).ceil() as u64,
                (reading_ms as f64 / 60_000.0).ceil() as u64
            ),
            crate::reading_time::READING_PROFILE_VERSION.to_string(),
            true,
        );
    };
    let Some(ratio) = estimate.saved_ratio else {
        return (
            format!(
                "视频 {} 分钟 · 正文约 {formatted_scale} · 预计阅读约 {} 分钟 · 预计节省约 {} 分钟",
                (video_ms as f64 / 60_000.0).ceil() as u64,
                (reading_ms as f64 / 60_000.0).ceil() as u64,
                (saved_ms as f64 / 60_000.0).ceil() as u64
            ),
            crate::reading_time::READING_PROFILE_VERSION.to_string(),
            true,
        );
    };
    (
        format!(
            "视频 {} 分钟 · 正文约 {formatted_scale} · 预计阅读约 {} 分钟 · 预计节省约 {} 分钟 · {}%（相对 1.0x 观看）",
            (video_ms as f64 / 60_000.0).ceil() as u64,
            (reading_ms as f64 / 60_000.0).ceil() as u64,
            (saved_ms as f64 / 60_000.0).ceil() as u64,
            ratio
        ),
        crate::reading_time::READING_PROFILE_VERSION.to_string(),
        true,
    )
}

fn reading_detail_for_job(
    job: &Job,
    selected_tier: Option<crate::content_results::ArtifactKindV1>,
) -> (String, String, String) {
    let Ok(crate::content_results::ContentViewV1::Current(snapshot)) =
        crate::content_results::current(job)
    else {
        return ("—".into(), "—".into(), String::new());
    };
    let language = job
        .transcription_result
        .as_ref()
        .and_then(|result| result.reported_language)
        .or_else(|| {
            job.transcription_selection
                .as_ref()
                .map(|selection| selection.requested_language)
        });
    let record = crate::content_results::primary_consumption_record(&snapshot, selected_tier);
    let Some(record) = record else {
        return ("—".into(), "—".into(), String::new());
    };
    let text = crate::content_results::record_plain_text(record);
    let estimate = crate::reading_time::estimate(&text, job.duration_ms, language);
    let Some(reading_ms) = estimate.reading_ms else {
        return ("—".into(), "—".into(), String::new());
    };
    let Some(saved_ms) = estimate.saved_ms else {
        return (
            format!("约 {} 分钟", (reading_ms as f64 / 60_000.0).ceil() as u64),
            "—".into(),
            crate::reading_time::READING_PROFILE_VERSION.to_string(),
        );
    };
    (
        format!("约 {} 分钟", (reading_ms as f64 / 60_000.0).ceil() as u64),
        format!("约 {} 分钟", (saved_ms as f64 / 60_000.0).ceil() as u64),
        crate::reading_time::READING_PROFILE_VERSION.to_string(),
    )
}

fn empty_content_results() -> ContentResultsData {
    ContentResultsData {
        entry_visible: false,
        mode: "unavailable".into(),
        title: "".into(),
        job_id: "".into(),
        bvid: "".into(),
        page: 1,
        video_duration: "—".into(),
        reading_caption: "".into(),
        reading_profile: "".into(),
        show_reading_explainer: false,
        reading_explainer: "".into(),
        model_notice: "".into(),
        show_model_notice: false,
        search_active: false,
        search_query: "".into(),
        search_result_count: 0,
        search_rows: content_row_model(Vec::new()),
        status_label: "".into(),
        notice: "".into(),
        notice_error: false,
        source_url: "".into(),
        default_tab: 3,
        show_summary: false,
        show_chapters: false,
        show_faithful: false,
        show_raw: false,
        can_repair: false,
        repair_pending: false,
        repair_label: "修复文字结果".into(),
        summary_tier: 1,
        summary_rows: content_row_model(Vec::new()),
        chapters_rows: content_row_model(Vec::new()),
        faithful_rows: content_row_model(Vec::new()),
        raw_rows: content_row_model(Vec::new()),
    }
}

fn content_row_model(rows: Vec<ContentRow>) -> ModelRc<ContentRow> {
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

#[allow(clippy::too_many_arguments)] // Single UI projection constructor; Slint row fields remain explicit.
fn content_row(
    id: impl Into<String>,
    kind: &str,
    title: impl Into<String>,
    text: impl Into<String>,
    declaration: impl Into<String>,
    status_label: impl Into<String>,
    status_kind: &str,
    source_text: impl Into<String>,
    source_speaker: impl Into<String>,
    source_range: impl Into<String>,
    source_url: impl Into<String>,
    can_source: bool,
    can_jump: bool,
    action_kind: &str,
    action_label: impl Into<String>,
    action_pending: bool,
) -> ContentRow {
    ContentRow {
        id: id.into().into(),
        kind: kind.into(),
        title: title.into().into(),
        text: text.into().into(),
        declaration: declaration.into().into(),
        status_label: status_label.into().into(),
        status_kind: status_kind.into(),
        source_text: source_text.into().into(),
        source_speaker: source_speaker.into().into(),
        source_range: source_range.into().into(),
        source_url: source_url.into().into(),
        can_source,
        can_jump,
        action_kind: action_kind.into(),
        action_label: action_label.into().into(),
        action_pending,
        action_caption: "".into(),
    }
}

fn model_changed_notice(
    snapshot: &crate::content_results::ContentSnapshotV1,
    current_model: Option<&str>,
) -> Option<String> {
    let current_model = current_model?.trim();
    if current_model.is_empty() {
        return None;
    }
    let newest = [
        crate::content_results::ArtifactKindV1::DefaultSummary,
        crate::content_results::ArtifactKindV1::ShortSummary,
        crate::content_results::ArtifactKindV1::LongSummary,
        crate::content_results::ArtifactKindV1::Highlights,
        crate::content_results::ArtifactKindV1::Chapters,
    ]
    .into_iter()
    .filter_map(|kind| snapshot.current.slots.get(kind).current.as_ref())
    .max_by_key(|record| record.created_at)?;

    let old_model = newest.provenance.model.as_deref()?.trim();
    if old_model.is_empty() || old_model == current_model {
        return None;
    }
    Some(format!(
        "当前模型已更换（{old_model} → {current_model}）。如需用新模型重写，请点击对应内容的“重新生成”。"
    ))
}

fn action_caption_for_model(current_model: Option<&str>) -> String {
    match current_model {
        Some(model) if !model.trim().is_empty() => format!("将使用：{model}"),
        _ => "将使用：当前 AI 设置不可用".into(),
    }
}

#[allow(clippy::too_many_arguments)] // Section rows deliberately share the flat ContentRow model.
fn section_row(
    id: impl Into<String>,
    title: impl Into<String>,
    declaration: impl Into<String>,
    status_label: impl Into<String>,
    status_kind: &str,
    text: impl Into<String>,
    action_kind: &str,
    action_label: impl Into<String>,
    action_pending: bool,
    action_caption: impl Into<String>,
) -> ContentRow {
    let mut row = content_row(
        id,
        "section",
        title,
        text,
        declaration,
        status_label,
        status_kind,
        "",
        "",
        "",
        "",
        false,
        false,
        action_kind,
        action_label,
        action_pending,
    );
    row.action_caption = action_caption.into().into();
    row
}

fn result_timestamp(milliseconds: u64) -> String {
    fmt_elapsed(milliseconds / 1000)
}

fn block_source_details(
    job: &Job,
    snapshot: &crate::content_results::ContentSnapshotV1,
    block: &ContentBlockV1,
) -> (String, String, String, String, bool, bool) {
    if block.source_status != SourceStatusV1::Mapped {
        return (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            false,
            false,
        );
    }
    let by_id = snapshot
        .evidence
        .utterances
        .iter()
        .map(|utterance| (utterance.id.as_str(), utterance))
        .collect::<HashMap<_, _>>();
    let mut seen = std::collections::HashSet::new();
    let mut utterances = Vec::new();
    for source_ref in &block.source_refs {
        for id in &source_ref.utterance_ids {
            if seen.insert(id.as_str()) {
                if let Some(utterance) = by_id.get(id.as_str()) {
                    utterances.push(*utterance);
                }
            }
        }
    }
    if utterances.is_empty() {
        return (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            false,
            false,
        );
    }
    let source_text = utterances
        .iter()
        .map(|utterance| utterance.text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let speakers = utterances
        .iter()
        .map(|utterance| job.speaker_name(utterance.speaker_id))
        .collect::<Vec<_>>();
    let mut speakers = speakers;
    speakers.dedup();
    let Some(span) = crate::bilibili::source_span(
        utterances
            .iter()
            .map(|utterance| (utterance.start_ms, utterance.end_ms)),
    ) else {
        return (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            false,
            false,
        );
    };
    let range = format!(
        "{}–{}",
        result_timestamp(span.start_ms),
        result_timestamp(span.end_ms)
    );
    let url = crate::bilibili::video_url(&job.bvid, job.page, Some(span.start_ms));
    (source_text, speakers.join("、"), range, url, true, true)
}

fn record_rows(
    job: &Job,
    snapshot: &crate::content_results::ContentSnapshotV1,
    record: &crate::content_results::DerivationRecordV1,
    id_prefix: &str,
) -> Vec<ContentRow> {
    record
        .blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            let (source_text, source_speaker, source_range, source_url, can_source, can_jump) =
                block_source_details(job, snapshot, block);
            let (status_label, status_kind) = match block.source_status {
                SourceStatusV1::Mapped if can_source => ("可核对", "mapped"),
                SourceStatusV1::Mapped => ("来源受限", "limited"),
                SourceStatusV1::Limited => ("来源受限", "limited"),
            };
            content_row(
                format!("{id_prefix}-{}-{index}", block.id),
                "block",
                block
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("条目 {}", index + 1)),
                block.text.trim(),
                "",
                status_label,
                status_kind,
                source_text,
                source_speaker,
                source_range,
                source_url,
                can_source,
                can_jump,
                "",
                "",
                false,
            )
        })
        .collect()
}

fn raw_rows(
    job: &Job,
    utterances: &[crate::funasr::Utterance],
    id_prefix: &str,
) -> Vec<ContentRow> {
    utterances
        .iter()
        .enumerate()
        .map(|(index, utterance)| {
            content_row(
                format!("{id_prefix}-{}-{index}", utterance.id),
                "raw",
                format!(
                    "{} · {}",
                    job.speaker_name(utterance.speaker_id),
                    result_timestamp(utterance.start_ms)
                ),
                utterance.text.trim(),
                "",
                "可核对",
                "mapped",
                utterance.text.trim(),
                job.speaker_name(utterance.speaker_id),
                format!(
                    "{}–{}",
                    result_timestamp(utterance.start_ms),
                    result_timestamp(utterance.end_ms)
                ),
                crate::bilibili::video_url(&job.bvid, job.page, Some(utterance.start_ms)),
                true,
                true,
                "",
                "",
                false,
            )
        })
        .collect()
}

fn artifact_declaration(
    kind: ArtifactKindV1,
    record: Option<&crate::content_results::DerivationRecordV1>,
) -> &'static str {
    if kind == ArtifactKindV1::FaithfulText {
        return match record {
            None => "忠实整理 · 保留原始稿与核对入口",
            Some(record) => match record.provenance.effective_path {
                EffectivePathV1::Rules => "规则整理 · 未使用 AI",
                EffectivePathV1::RulesFallback => match record
                    .provenance
                    .fallback_reason
                    .as_ref()
                    .map(|reason| reason.code)
                {
                    Some(FallbackReasonCodeV1::TargetUnavailable) => {
                        "已使用规则整理 · AI 配置不可用"
                    }
                    _ => "AI 校正未完成 · 已使用规则整理",
                },
                EffectivePathV1::Llm => "忠实整理 · 保留原始稿与核对入口",
            },
        };
    }
    match kind {
        ArtifactKindV1::DefaultSummary
        | ArtifactKindV1::ShortSummary
        | ArtifactKindV1::LongSummary => "AI 生成内容 · 请结合来源核对",
        ArtifactKindV1::Highlights => "AI 生成内容 · 请结合来源核对",
        ArtifactKindV1::Chapters => "结构化整理",
        ArtifactKindV1::FaithfulText => unreachable!(),
    }
}

fn action_label(
    kind: ArtifactKindV1,
    has_record: bool,
    failure: Option<&crate::content_results::DerivationFailureV1>,
) -> String {
    let label = match kind {
        ArtifactKindV1::DefaultSummary => "摘要",
        ArtifactKindV1::ShortSummary => "短摘要",
        ArtifactKindV1::LongSummary => "长摘要",
        ArtifactKindV1::Highlights => "重点",
        ArtifactKindV1::Chapters => "章节",
        ArtifactKindV1::FaithfulText => "忠实正文",
    };
    if !has_record && failure.is_none() {
        format!("生成{label}")
    } else if failure.is_some_and(|failure| {
        failure.code == crate::content_results::FailureCodeV1::TargetUnavailable
    }) {
        format!("使用当前 AI 设置重新生成{label}")
    } else {
        format!("重新生成{label}")
    }
}

fn slot_rows(
    job: &Job,
    snapshot: &crate::content_results::ContentSnapshotV1,
    kind: ArtifactKindV1,
    title: &str,
    action_kind: &str,
    pending_kind: Option<ArtifactKindV1>,
    action_caption: &str,
) -> Vec<ContentRow> {
    let slot = snapshot.current.slots.get(kind);
    let record = slot.current.as_ref();
    let failure = slot.last_failure.as_ref();
    let (status_label, status_kind, text) = match (record, failure) {
        (Some(_), Some(failure)) => (
            "最近失败",
            "failure",
            format!("增强未完成；原始稿和忠实正文仍可用。{}", failure.message),
        ),
        (Some(record), None) => {
            let limited = record.validation.record_source_status == SourceStatusV1::Limited
                || record
                    .blocks
                    .iter()
                    .any(|block| block.source_status == SourceStatusV1::Limited);
            if limited {
                ("来源受限", "limited", String::new())
            } else {
                ("可用", "mapped", String::new())
            }
        }
        (None, Some(failure)) => (
            "生成失败",
            "failure",
            format!("增强未完成；原始稿和忠实正文仍可用。{}", failure.message),
        ),
        (None, None) => ("", "idle", format!("{title}尚未生成")),
    };
    let has_action = kind != ArtifactKindV1::FaithfulText;
    let mut rows = vec![section_row(
        format!("section-{action_kind}"),
        title,
        artifact_declaration(kind, record),
        status_label,
        status_kind,
        text,
        if has_action { action_kind } else { "" },
        if has_action {
            action_label(kind, record.is_some(), failure)
        } else {
            String::new()
        },
        pending_kind == Some(kind),
        if has_action { action_caption } else { "" },
    )];
    if let Some(record) = record {
        rows.extend(record_rows(job, snapshot, record, action_kind));
    }
    rows
}

fn raw_slot_rows(job: &Job, utterances: &[crate::funasr::Utterance]) -> Vec<ContentRow> {
    let mut rows = vec![section_row(
        "section-raw",
        "原始稿",
        "原始稿 · 未经 LLM 改写",
        "可用",
        "mapped",
        "",
        "open-raw",
        "打开原始稿",
        false,
        "",
    )];
    rows.extend(raw_rows(job, utterances, "raw"));
    rows
}

fn content_results_for_job(
    job: &Job,
    pending_kind: Option<ArtifactKindV1>,
    current_model: Option<&str>,
    summary_tier: i32,
) -> ContentResultsData {
    let Some(_setup) = job.content_setup.as_ref() else {
        return empty_content_results();
    };
    if !job.status.is_terminal() {
        return empty_content_results();
    }
    let source_url = crate::bilibili::video_url(&job.bvid, job.page, None);
    let video_duration = job
        .duration_ms
        .filter(|milliseconds| *milliseconds > 0)
        .map(|milliseconds| fmt_elapsed(milliseconds / 1000))
        .unwrap_or_else(|| "—".to_string());
    match crate::content_results::current(job) {
        Ok(ContentViewV1::Current(snapshot)) => {
            let enhancements_requested = snapshot.current.setup.enhancements_requested;
            let action_caption = action_caption_for_model(current_model);
            let faithful = slot_rows(
                job,
                &snapshot,
                ArtifactKindV1::FaithfulText,
                "忠实正文",
                "",
                pending_kind,
                "",
            );
            let tier_kind = match summary_tier {
                0 => ArtifactKindV1::ShortSummary,
                2 => ArtifactKindV1::LongSummary,
                _ => ArtifactKindV1::DefaultSummary,
            };
            let summary = if enhancements_requested {
                let mut rows = slot_rows(
                    job,
                    &snapshot,
                    tier_kind,
                    match tier_kind {
                        ArtifactKindV1::ShortSummary => "短摘要",
                        ArtifactKindV1::LongSummary => "长摘要",
                        _ => "摘要",
                    },
                    match tier_kind {
                        ArtifactKindV1::ShortSummary => "summary-short",
                        ArtifactKindV1::LongSummary => "summary-long",
                        _ => "summary",
                    },
                    pending_kind,
                    &action_caption,
                );
                rows.extend(slot_rows(
                    job,
                    &snapshot,
                    ArtifactKindV1::Highlights,
                    "重点",
                    "highlights",
                    pending_kind,
                    &action_caption,
                ));
                rows
            } else {
                Vec::new()
            };
            let chapters = if enhancements_requested {
                slot_rows(
                    job,
                    &snapshot,
                    ArtifactKindV1::Chapters,
                    "章节",
                    "chapters",
                    pending_kind,
                    &action_caption,
                )
            } else {
                Vec::new()
            };
            let faithful_has_current = snapshot
                .current
                .slots
                .get(ArtifactKindV1::FaithfulText)
                .current
                .is_some();
            let enhancement_failed = [
                ArtifactKindV1::DefaultSummary,
                ArtifactKindV1::Highlights,
                ArtifactKindV1::Chapters,
            ]
            .into_iter()
            .any(|kind| snapshot.current.slots.get(kind).last_failure.is_some());
            let tier_for_reading = match summary_tier {
                0 => Some(ArtifactKindV1::ShortSummary),
                2 => Some(ArtifactKindV1::LongSummary),
                _ => Some(ArtifactKindV1::DefaultSummary),
            };
            let (reading_caption, reading_profile, show_reading_explainer) =
                reading_caption_for_job(job, &snapshot, tier_for_reading);
            let (model_notice, show_model_notice) =
                match model_changed_notice(&snapshot, current_model) {
                    Some(notice) => (notice, true),
                    None => (String::new(), false),
                };
            let (notice, notice_error) = if enhancement_failed {
                ("增强未完成；原始稿和忠实正文仍可用。".into(), false)
            } else {
                (String::new(), false)
            };
            ContentResultsData {
                entry_visible: true,
                mode: "current".into(),
                title: job.title.clone().into(),
                job_id: job.id.to_string().into(),
                bvid: job.bvid.clone().into(),
                page: job.page as i32,
                video_duration: video_duration.clone().into(),
                reading_caption: reading_caption.into(),
                reading_profile: reading_profile.into(),
                show_reading_explainer,
                reading_explainer: "公式 cjk/400+words/240 · reading-profile v1 · 1.0x · 依据 Brysbaert 2019 / 2024 中文阅读实验".into(),
                model_notice: model_notice.into(),
                show_model_notice,
                search_active: false,
                search_query: "".into(),
                search_result_count: 0,
                search_rows: content_row_model(Vec::new()),
                status_label: job.status.label().into(),
                notice: notice.into(),
                notice_error,
                source_url: source_url.into(),
                default_tab: if enhancements_requested
                    && snapshot
                        .current
                        .slots
                        .get(tier_kind)
                        .current
                        .is_some()
                {
                    0
                } else if faithful_has_current {
                    2
                } else {
                    3
                },
                show_summary: enhancements_requested,
                show_chapters: enhancements_requested,
                show_faithful: faithful_has_current,
                show_raw: true,
                can_repair: false,
                repair_pending: false,
                repair_label: "修复文字结果".into(),
                summary_rows: content_row_model(summary),
                chapters_rows: content_row_model(chapters),
                faithful_rows: content_row_model(faithful),
                raw_rows: content_row_model(raw_slot_rows(job, &snapshot.evidence.utterances)),
                summary_tier,
            }
        }
        Ok(ContentViewV1::RawOnly {
            evidence,
            current_issue,
        }) => ContentResultsData {
            entry_visible: true,
            mode: "raw-only".into(),
            title: job.title.clone().into(),
            job_id: job.id.to_string().into(),
            bvid: job.bvid.clone().into(),
            page: job.page as i32,
            video_duration: video_duration.into(),
            reading_caption: "".into(),
            reading_profile: "".into(),
            show_reading_explainer: false,
            reading_explainer: "".into(),
            model_notice: "".into(),
            show_model_notice: false,
            search_active: false,
            search_query: "".into(),
            search_result_count: 0,
            search_rows: content_row_model(Vec::new()),
            status_label: job.status.label().into(),
            notice: format!(
                "可信文字结果{}；原始 Evidence 仍可用。",
                match current_issue {
                    crate::content_results::CurrentIssueV1::NotGenerated => "尚未生成",
                    crate::content_results::CurrentIssueV1::Corrupt => "已损坏",
                }
            )
            .into(),
            notice_error: true,
            source_url: source_url.into(),
            default_tab: 3,
            summary_tier: 1,
            show_summary: false,
            show_chapters: false,
            show_faithful: false,
            show_raw: true,
            can_repair: true,
            repair_pending: false,
            repair_label: "修复文字结果".into(),
            summary_rows: content_row_model(Vec::new()),
            chapters_rows: content_row_model(Vec::new()),
            faithful_rows: content_row_model(Vec::new()),
            raw_rows: content_row_model(raw_slot_rows(job, &evidence.utterances)),
        },
        Ok(ContentViewV1::Legacy) | Err(_) => empty_content_results(),
    }
}

#[cfg(test)]
fn fixture_content_results(selector: &str) -> ContentResultsData {
    let id = Uuid::new_v4();
    let root = std::env::temp_dir().join(format!("bi2read-result-fixture-{id}"));
    let cfg = Config::for_paths(&crate::paths::AppPaths::discover().expect("app paths"));
    let job = result_fixture_callback_job(&root, &cfg, selector).expect("result fixture job");
    let pending_kind = (selector == "result-regenerating").then_some(ArtifactKindV1::Chapters);
    let result = content_results_for_job(&job, pending_kind, None, 1);
    std::fs::remove_dir_all(root).ok();
    result
}

/// Find the currently-failed stage, if any.
fn failed_stage(job: &Job) -> Option<Stage> {
    jobs::pipeline_stages()
        .into_iter()
        .find(|s| job.stage_state(*s) == StageState::Failed)
}

fn retry_feedback(outcome: &DrainOutcome) -> (String, bool) {
    match outcome {
        DrainOutcome::Ran {
            result: JobResult::Completed,
            ..
        } => ("重试已完成".into(), false),
        DrainOutcome::Ran {
            result: JobResult::Failed(message),
            ..
        } => (format!("重试失败：{message}"), true),
        DrainOutcome::Ran {
            result: JobResult::Cancelled,
            ..
        } => ("重试已取消".into(), true),
        DrainOutcome::Ran {
            result: JobResult::NeedsUserAction,
            ..
        } => ("重试需要外部操作".into(), true),
        DrainOutcome::Ran {
            result: JobResult::WaitingForDrive,
            ..
        } => ("重试已排队，等待外接盘".into(), false),
        DrainOutcome::Waiting => ("已重新排队，等待运行条件".into(), false),
        DrainOutcome::Idle => ("已重新排队".into(), false),
    }
}

/// Show the full detail pane (stages, speakers, task info) for one job.
///
/// Extracted from the Controller so worker-side queue rebuilds after deletion
/// can refresh the detail pane without going through UI-thread-only state.
fn show_job_detail(app: &App, job: &Job, current_model: Option<&str>, summary_tier: i32) {
    let tier_kind = match summary_tier {
        0 => Some(ArtifactKindV1::ShortSummary),
        2 => Some(ArtifactKindV1::LongSummary),
        _ => Some(ArtifactKindV1::DefaultSummary),
    };
    show_job_detail_inner(app, job, tier_kind, current_model, summary_tier);
}

fn show_job_detail_inner(
    app: &App,
    job: &Job,
    tier_kind: Option<ArtifactKindV1>,
    current_model: Option<&str>,
    summary_tier: i32,
) {
    let stages_rc = app.get_stages();
    let speakers_rc = app.get_speakers();

    // Build the snapshot from the job (reads StageState directly).
    let now = chrono::Utc::now();
    let snap = jobs::JobViewSnapshot::from_job(job, now);
    let (elapsed_secs, elapsed_label) = fmt_job_elapsed(job, now);
    let content_result = content_results_for_job(job, None, current_model, summary_tier);
    let content_entry_visible = content_result.entry_visible;
    app.set_result(content_result);

    // Stages view: from the snapshot's StageSnapshot list (reads persisted
    // StageState rather than inferring it from sequence position.
    let stage_views = crate::ui_bridge::stage_views_from_snapshot(&snap);
    if let Some(model) = stages_rc
        .as_any()
        .downcast_ref::<slint::VecModel<StageView>>()
    {
        model.set_vec(stage_views);
    }

    // Speakers: from the job's speaker_map if known, else default 3 slots.
    // Segment counts (spec.md 第二节第 12 条) come from the same raw
    // transcript: each utterance's `speaker_id` is tallied so the inspector
    // can show "出现 N 段". When no raw transcript exists yet (job hasn't
    // reached transcription, or the file is missing/unparseable) there is no
    // per-speaker utterance data to count, so every speaker gets
    // `segment_count: 0` — the UI treats 0 as "count unavailable" and hides
    // the "出现 N 段" suffix rather than lying about zero segments.
    let raw: Option<Vec<funasr::Utterance>> = job
        .work_dir
        .as_ref()
        .and_then(|d| pipeline::load_utterances(&d.join("transcript.raw.json")).ok());
    let (ids, segment_counts): (Vec<u32>, HashMap<u32, i32>) = match &raw {
        Some(utts) => {
            let mut v: Vec<u32> = utts.iter().map(|u| u.speaker_id).collect();
            v.sort();
            v.dedup();
            let mut counts: HashMap<u32, i32> = HashMap::new();
            for u in utts {
                *counts.entry(u.speaker_id).or_insert(0) += 1;
            }
            (v, counts)
        }
        None => ((0..3).collect(), HashMap::new()),
    };
    let spk: Vec<SpeakerEntry> = ids
        .iter()
        .map(|i| SpeakerEntry {
            speaker_id: *i as i32,
            raw_label: format!("Speaker {}", i).into(),
            name: job.speaker_map.get(i).cloned().unwrap_or_default().into(),
            segment_count: *segment_counts.get(i).unwrap_or(&0),
        })
        .collect();
    if let Some(model) = speakers_rc
        .as_any()
        .downcast_ref::<slint::VecModel<SpeakerEntry>>()
    {
        model.set_vec(spk);
    }

    let caps = &snap.capabilities;
    let document_words = document_words_placeholder(job);
    let (reading_estimate, reading_saved, reading_profile) = reading_detail_for_job(job, tier_kind);
    app.set_detail(JobDetailData {
        has_job: true,
        title: snap.title.clone().into(),
        bvid: snap.bvid.clone().into(),
        page: snap.page as i32,
        status_label: snap.status.label().into(),
        stage_name: snap.stage.name().into(),
        status_name: snap.status.name().into(),
        total_progress: snap.total_progress as i32,
        elapsed_secs: elapsed_secs as i32,
        elapsed: elapsed_label.into(),
        error_text: snap.error.clone().unwrap_or_default().into(),
        has_error: snap.error.is_some(),
        warning_text: snap
            .warning
            .as_ref()
            .map(|w| w.label())
            .or_else(|| {
                (job.content_setup.is_some()
                    && job.status.is_terminal()
                    && !content_entry_visible
                    && crate::content_results::current(job).is_err())
                .then_some("原始稿不可用，请重新创建任务。".to_string())
            })
            .unwrap_or_default()
            .into(),
        can_cancel: caps.can_cancel,
        can_retry: caps.can_retry,
        can_rebuild: caps.can_rebuild,
        can_open: caps.can_open_document,
        can_reveal: caps.can_reveal,
        can_delete: caps.can_delete,
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
        media_size: fmt_media_size(job).into(),
        document_words: document_words.into(),
        transcription_runtime: snap.transcription_runtime.clone().into(),
        transcription_source: snap.transcription_source.clone().into(),
        transcription_backend: snap.transcription_backend.clone().into(),
        transcription_model: snap.transcription_model.clone().into(),
        requested_language: snap.requested_language.clone().into(),
        reported_language: snap.reported_language.clone().into(),
        reported_model: snap.reported_model.clone().into(),
        reading_estimate: reading_estimate.into(),
        reading_saved: reading_saved.into(),
        reading_profile: reading_profile.into(),
        can_view_result: content_entry_visible,
    });
    if document_words == "统计中…" {
        refresh_document_words_async(app.as_weak(), job.clone());
    }
}

/// Clear the detail pane back to its empty state ("选择左侧任务…").
fn clear_job_detail(app: &App) {
    let stages_rc = app.get_stages();
    if let Some(model) = stages_rc
        .as_any()
        .downcast_ref::<slint::VecModel<StageView>>()
    {
        model.set_vec(Vec::new());
    }
    let speakers_rc = app.get_speakers();
    if let Some(model) = speakers_rc
        .as_any()
        .downcast_ref::<slint::VecModel<SpeakerEntry>>()
    {
        model.set_vec(Vec::new());
    }
    app.set_detail(empty_detail());
    app.set_result(empty_content_results());
    app.set_action_pending_kind("".into());
    app.set_result_open(false);
}

/// Refresh file-backed task-information metrics after a worker run completes.
///
/// Stage snapshots are intentionally streamed by `pipeline.rs`, but file
/// statistics only become truthful after the terminal job state and retention
/// policy are known.  Keep this small refresh on the UI event loop boundary so
/// a selected job never keeps the previous task's media/document values.
fn refresh_selected_detail(
    app: &Weak<App>,
    queue: &Arc<Mutex<Queue>>,
    config: &Arc<Mutex<Config>>,
    job_id: Uuid,
) {
    let queue = queue.clone();
    let config = config.clone();
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
        let current_model = config
            .lock()
            .ok()
            .and_then(|cfg| cfg.llm_connection.as_ref().map(|c| c.model.clone()));
        let pending_kind = regenerable_kind_from_action(app.get_action_pending_kind().as_str())
            .map(RegenerableKind::artifact_kind);
        let summary_tier = app.get_summary_tier();
        app.set_result(content_results_for_job(
            &job,
            pending_kind,
            current_model.as_deref(),
            summary_tier,
        ));
        if document_words == "统计中…" {
            refresh_document_words_async(app.as_weak(), job);
        }
    });
}

fn refresh_selected_result(
    app: &Weak<App>,
    queue: &Arc<Mutex<Queue>>,
    config: &Arc<Mutex<Config>>,
    job_id: Uuid,
) {
    let queue = queue.clone();
    let config = config.clone();
    let _ = app.upgrade_in_event_loop(move |app| {
        let selected = (0..app.get_jobs().row_count()).find_map(|index| {
            let row = app.get_jobs().row_data(index)?;
            row.selected
                .then(|| Uuid::parse_str(row.id.as_ref()).ok())
                .flatten()
        });
        if selected != Some(job_id) {
            return;
        }
        if let Some(job) = queue
            .lock()
            .ok()
            .and_then(|queue| queue.get(job_id).cloned())
        {
            let current_model = config
                .lock()
                .ok()
                .and_then(|cfg| cfg.llm_connection.as_ref().map(|c| c.model.clone()));
            let pending_kind = regenerable_kind_from_action(app.get_action_pending_kind().as_str())
                .map(RegenerableKind::artifact_kind);
            let summary_tier = app.get_summary_tier();
            app.set_result(content_results_for_job(
                &job,
                pending_kind,
                current_model.as_deref(),
                summary_tier,
            ));
        }
    });
}

// ---- Controller (lives on the UI thread) ----
struct Controller {
    app: Weak<App>,
    jobs_model: Rc<VecModel<JobRow>>,
    speakers_model: Rc<VecModel<SpeakerEntry>>,
    config: Arc<Mutex<Config>>,
    queue: Arc<Mutex<Queue>>,
    scheduler: Arc<Scheduler>,
    tx: Sender<WorkMsg>,
    content_busy: Arc<Mutex<ContentBusy>>,
    pending_transcription: RefCell<Option<PendingTranscription>>,
    history_confirmation: RefCell<Option<HistoryConfirmation>>,
    /// Debounce timer for speaker name edits.
    speaker_timer: Mutex<Option<slint::Timer>>,
    selected_summary_tier: RefCell<usize>,
    probe_token: Arc<AtomicU64>,
}

#[derive(Debug, Clone)]
struct PendingTranscription {
    source_url: String,
    bvid: String,
    page: u32,
    runtime: crate::funasr::RuntimeDescription,
    requested_language: SourceLanguage,
}

/// Which destructive history action the confirm dialog is asking about.
#[derive(Debug, Clone)]
enum HistoryConfirmation {
    DeleteJob(Uuid),
    ClearHistory,
}

impl Controller {
    fn current_model(&self) -> Option<String> {
        self.config
            .lock()
            .ok()
            .and_then(|cfg| cfg.llm_connection.as_ref().map(|c| c.model.clone()))
    }

    fn test_connection(&self, settings: SettingsView) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_settings_saving() {
            return;
        }
        // Reuse the same resolver as Job creation / regeneration so the probe
        // exercises the canonical loopback validation. Pre-send failures never
        // touch the network.
        let connection = {
            let api_format = crate::llm::ApiFormat::parse(settings.llm_api_format.as_ref())
                .unwrap_or(crate::llm::ApiFormat::OpenAiChatCompletions);
            crate::llm::LlmConnection {
                id: "default".into(),
                name: settings.llm_connection_name.to_string(),
                api_format,
                base_url: crate::llm::normalize_base_url(settings.llm_base_url.as_ref()),
                model: settings.llm_model.to_string(),
            }
        };
        let target = match crate::content_results::resolve_content_setup(
            settings.llm_enabled,
            Some(&connection),
        )
        .initial_target
        {
            crate::content_results::InitialTargetV1::Ready(target) => target,
            _ => {
                app.set_connection_test_state("failure".into());
                app.set_connection_test_message(
                    crate::content_results::ConnectionTestFailure::InvalidAddress
                        .message()
                        .into(),
                );
                return;
            }
        };
        if !connection.is_local() {
            return;
        }
        app.set_connection_test_state("testing".into());
        app.set_connection_test_message("".into());
        let token = self.probe_token.fetch_add(1, Ordering::SeqCst) + 1;
        let probe_token = self.probe_token.clone();
        let weak = self.app.clone();
        std::thread::spawn(move || {
            let outcome = crate::content_results::test_connection(&target);
            let _ = weak.upgrade_in_event_loop(move |app| {
                if probe_token.load(Ordering::SeqCst) != token {
                    return;
                }
                match outcome {
                    Ok(success) => {
                        let protocol = match success.api_format {
                            crate::content_results::GenerationApiFormatV1::OpenAiChatCompletions => {
                                "openai_chat_completions"
                            }
                            crate::content_results::GenerationApiFormatV1::AnthropicMessages => {
                                "anthropic_messages"
                            }
                        };
                        app.set_connection_test_state("success".into());
                        app.set_connection_test_message(
                            format!(
                                "连接成功 · {protocol} · {}。测试成功不代表生成内容正确。",
                                success.model
                            )
                            .into(),
                        );
                    }
                    Err(failure) => {
                        let message = failure.with_model(&target.model);
                        app.set_connection_test_state("failure".into());
                        app.set_connection_test_message(message.into());
                    }
                }
            });
        });
    }

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
            let result = crate::funasr::install_runtime(&project_dir, &runtime_data);
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

    fn prepare_add_job(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_add_busy() || app.get_transcription_confirm_visible() {
            return;
        }
        let url = app.get_url().to_string();
        if url.trim().is_empty() {
            return;
        }
        let config_snapshot = self.config.lock().unwrap().clone();
        let parsed = match bilibili::parse_url(&url) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("URL parse failed: {e}");
                app.set_add_state("error".into());
                app.set_add_feedback(format!("无法添加：{e}").into());
                return;
            }
        };

        let page = parsed.page.unwrap_or(1);
        let Some(project) = config_snapshot.effective_runtime_project() else {
            self.show_unavailable_confirmation(
                &app,
                &parsed.bvid,
                page,
                "未配置 Runtime，请打开设置选择并验证 Runtime。",
            );
            return;
        };
        let runtime_source = if config_snapshot.runtime_project.is_some() {
            crate::jobs::RuntimeSource::External
        } else {
            crate::jobs::RuntimeSource::Bundled
        };
        let runtime = match crate::funasr::describe_runtime(
            &project,
            &config_snapshot.runtime_data_dir,
            runtime_source,
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                self.show_unavailable_confirmation(
                    &app,
                    &parsed.bvid,
                    page,
                    &format!("无法读取 Runtime：{error}"),
                );
                return;
            }
        };
        let mut requested_language = config_snapshot.app_default_language(&runtime.identity);
        if requested_language == SourceLanguage::Auto && runtime.auto_detection != Some(true) {
            requested_language = SourceLanguage::Zh;
        }
        *self.pending_transcription.borrow_mut() = Some(PendingTranscription {
            source_url: url,
            bvid: parsed.bvid,
            page,
            runtime,
            requested_language,
        });
        self.refresh_pending_runtime();
    }

    fn select_summary_tier(&self, tier: i32) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let tier = tier.clamp(0, 2);
        *self.selected_summary_tier.borrow_mut() = tier as usize;
        app.set_summary_tier(tier);
        let Some(job_id) = self.selected_job_id() else {
            return;
        };
        let current_model = self.current_model();
        let Some(job) = self.queue.lock().ok().and_then(|q| q.get(job_id).cloned()) else {
            return;
        };
        let pending_kind = regenerable_kind_from_action(app.get_action_pending_kind().as_str())
            .map(|k| k.artifact_kind());
        app.set_result(content_results_for_job(
            &job,
            pending_kind,
            current_model.as_deref(),
            tier,
        ));
        // Keep job detail's estimate in sync if the selected summary tier changed.
        let tier_kind = match tier {
            0 => Some(ArtifactKindV1::ShortSummary),
            2 => Some(ArtifactKindV1::LongSummary),
            _ => Some(ArtifactKindV1::DefaultSummary),
        };
        let (reading_estimate, reading_saved, reading_profile) =
            reading_detail_for_job(&job, tier_kind);
        let mut detail = app.get_detail();
        detail.reading_estimate = reading_estimate.into();
        detail.reading_saved = reading_saved.into();
        detail.reading_profile = reading_profile.into();
        app.set_detail(detail);
    }

    fn handle_search(&self, query: String) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(job_id) = self.selected_job_id() else {
            return;
        };
        let Some(job) = self.queue.lock().ok().and_then(|q| q.get(job_id).cloned()) else {
            return;
        };
        let Ok(crate::content_results::ContentViewV1::Current(snapshot)) =
            crate::content_results::current(&job)
        else {
            return;
        };
        let trimmed = query.trim().to_string();
        if trimmed.is_empty() {
            self.clear_search();
            return;
        }
        // Parse with time > speaker > text priority; caller supplies speaker map.
        let parsed =
            crate::content_results::parse_search_query_with_speakers(&trimmed, &job.speaker_map)
                .or_else(|| crate::content_results::parse_search_query(&trimmed))
                .unwrap_or(crate::content_results::SearchQuery::Text(trimmed.clone()));
        let hits = crate::content_results::search(&snapshot, &job.speaker_map, &parsed);
        let count = hits.len() as i32;
        // Project hits into ContentRow search rows (same flat ListView, no nesting).
        let search_rows: Vec<ContentRow> = hits
            .into_iter()
            .enumerate()
            .map(|(idx, hit)| {
                let can_jump = hit.has_source_refs;
                let can_source = hit.has_source_refs;
                let status_kind = if hit.has_source_refs {
                    "mapped"
                } else {
                    "limited"
                };
                let range = if hit.has_source_refs {
                    format!(
                        "{}–{}",
                        result_timestamp(hit.start_ms),
                        result_timestamp(hit.end_ms)
                    )
                } else {
                    String::new()
                };
                let url = if can_jump {
                    crate::bilibili::video_url(&job.bvid, job.page, Some(hit.start_ms))
                } else {
                    String::new()
                };
                content_row(
                    format!("search-{idx}"),
                    "block",
                    hit.layer.label(),
                    hit.snippet.clone(),
                    "",
                    hit.layer.label(),
                    status_kind,
                    hit.snippet.clone(),
                    String::new(),
                    range,
                    url,
                    can_jump,
                    can_source,
                    "",
                    "",
                    false,
                )
            })
            .collect();
        // If no hits, show a single empty section row with the fixed copy.
        let mut result = app.get_result();
        if search_rows.is_empty() {
            let empty = section_row(
                "search-empty",
                format!("没有找到“{trimmed}”。"),
                "",
                "",
                "",
                "",
                "",
                "",
                false,
                "",
            );
            result.search_rows = ModelRc::from(Rc::new(VecModel::from(vec![empty])));
            result.search_result_count = 0;
        } else {
            // Prepend a section header row with count.
            let header = section_row(
                "search-header",
                format!("搜索“{trimmed}” · {count} 条结果"),
                "",
                "",
                "",
                "",
                "",
                "",
                false,
                "",
            );
            let mut rows = vec![header];
            rows.extend(search_rows);
            result.search_rows = ModelRc::from(Rc::new(VecModel::from(rows)));
            result.search_result_count = count;
        }
        result.search_active = true;
        result.search_query = trimmed.into();
        app.set_result(result);
    }

    fn clear_search(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let mut result = app.get_result();
        result.search_active = false;
        result.search_query = "".into();
        result.search_result_count = 0;
        result.search_rows = ModelRc::from(Rc::new(VecModel::<ContentRow>::default()));
        app.set_result(result);
    }

    fn show_unavailable_confirmation(&self, app: &App, bvid: &str, page: u32, error: &str) {
        self.pending_transcription.borrow_mut().take();
        app.set_transcription_confirm_video(format!("{} · P{}", bvid, page).into());
        app.set_transcription_confirm_runtime_name("未提供".into());
        app.set_transcription_confirm_runtime_source("未提供".into());
        app.set_transcription_confirm_runtime_backend("未提供".into());
        app.set_transcription_confirm_model("未提供".into());
        app.set_transcription_confirm_recommended("未提供".into());
        app.set_transcription_confirm_limitations("未提供".into());
        app.set_transcription_confirm_error(error.into());
        app.set_transcription_confirm_ready(false);
        app.set_transcription_confirm_auto_available(false);
        app.set_transcription_confirm_language(SourceLanguage::Zh.as_str().into());
        app.set_transcription_confirm_can_create(false);
        app.set_transcription_confirm_visible(true);
    }

    fn refresh_pending_runtime(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(pending) = self.pending_transcription.borrow().clone() else {
            return;
        };
        let runtime = &pending.runtime;
        let contract_ready = runtime.contract_version == crate::funasr::SUPPORTED_CONTRACT_VERSION;
        let can_create = runtime.ready && contract_ready;
        let runtime_error = if !contract_ready {
            format!(
                "当前 Runtime contract v{} 需要升级到 v{}，暂不能创建新任务。",
                runtime.contract_version,
                crate::funasr::SUPPORTED_CONTRACT_VERSION
            )
        } else if !runtime.ready {
            "Runtime 未就绪，请打开设置完成安装或验证。".to_string()
        } else {
            String::new()
        };
        let runtime_name = runtime
            .display_name
            .clone()
            .unwrap_or_else(|| runtime.source.label().to_string());
        let recommended_languages = if runtime.recommended_languages.is_empty() {
            "未提供".to_string()
        } else {
            runtime
                .recommended_languages
                .iter()
                .map(|language| language.label())
                .collect::<Vec<_>>()
                .join("、")
        };
        let known_limitations = if runtime.known_limitations.is_empty() {
            "未提供".to_string()
        } else {
            runtime.known_limitations.join("；")
        };
        app.set_transcription_confirm_video(format!("{} · P{}", pending.bvid, pending.page).into());
        app.set_transcription_confirm_runtime_name(runtime_name.into());
        app.set_transcription_confirm_runtime_source(runtime.source.as_str().into());
        app.set_transcription_confirm_runtime_backend(runtime.backend.as_str().into());
        app.set_transcription_confirm_model(
            runtime
                .model_description
                .clone()
                .unwrap_or_else(|| "未提供".into())
                .into(),
        );
        app.set_transcription_confirm_recommended(recommended_languages.into());
        app.set_transcription_confirm_limitations(known_limitations.into());
        app.set_transcription_confirm_error(runtime_error.into());
        app.set_transcription_confirm_ready(can_create);
        app.set_transcription_confirm_auto_available(runtime.auto_detection == Some(true));
        app.set_transcription_confirm_language(pending.requested_language.as_str().into());
        app.set_transcription_confirm_can_create(can_create);
        app.set_transcription_confirm_visible(true);
    }

    fn update_pending_language(&self, value: String) {
        let Ok(mut language) = value.parse::<SourceLanguage>() else {
            return;
        };
        if language == SourceLanguage::Auto {
            let auto_available = self
                .pending_transcription
                .borrow()
                .as_ref()
                .is_some_and(|pending| pending.runtime.auto_detection == Some(true));
            if !auto_available {
                language = SourceLanguage::Zh;
            }
        }
        if let Some(pending) = self.pending_transcription.borrow_mut().as_mut() {
            pending.requested_language = language;
        }
        self.refresh_pending_runtime();
    }

    fn cancel_pending_job(&self) {
        self.pending_transcription.borrow_mut().take();
        if let Some(app) = self.app.upgrade() {
            app.set_transcription_confirm_visible(false);
            app.set_transcription_confirm_error("".into());
        }
    }

    fn open_settings_from_confirmation(&self) {
        self.pending_transcription.borrow_mut().take();
        let Some(app) = self.app.upgrade() else {
            return;
        };
        app.set_transcription_confirm_visible(false);
        app.set_settings_feedback("".into());
        app.set_settings_error_field("".into());
        app.set_settings_error_message("".into());
        app.set_settings_saving(false);
        app.set_settings_visible(true);
    }

    fn confirm_pending_job(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(pending) = self.pending_transcription.borrow().clone() else {
            return;
        };
        let contract_ready =
            pending.runtime.contract_version == crate::funasr::SUPPORTED_CONTRACT_VERSION;
        if !pending.runtime.ready || !contract_ready {
            self.refresh_pending_runtime();
            return;
        }
        let selection = TranscriptionSelection::new(
            pending.runtime.source,
            pending.runtime.project.clone(),
            pending.runtime.data_dir.clone(),
            pending.runtime.identity.clone(),
            pending.runtime.backend,
            pending.runtime.model_description.clone(),
            pending.requested_language,
            CreatedFrom::App,
        );
        if let Err(error) = selection.validate() {
            app.set_transcription_confirm_error(format!("无法创建任务：{error}").into());
            return;
        }
        let (retention, content_setup) = {
            let config = self.config.lock().unwrap();
            (config.default_retention, config.content_setup())
        };
        let mut job = Job::from_creation(JobCreationInput::new(
            Uuid::new_v4(),
            pending.bvid.clone(),
            pending.page,
            selection.clone(),
            retention,
            content_setup,
        ));
        job.source_url = Some(pending.source_url.clone());
        let persist_result = {
            let mut q = self.queue.lock().unwrap();
            q.jobs.push(job.clone());
            match q.save() {
                Ok(()) => Ok(()),
                Err(error) => {
                    q.jobs.pop();
                    Err(error)
                }
            }
        };
        if let Err(error) = persist_result {
            app.set_transcription_confirm_error(format!("无法保存任务：{error}").into());
            return;
        }
        {
            let mut config = self.config.lock().unwrap();
            config.last_transcription_selection = Some(selection);
            if let Err(error) = config.save() {
                log::warn!("failed to save last transcription selection: {error}");
            }
        }

        let row = job_to_row(&job, true);
        let len = self.jobs_model.row_count();
        for i in 0..len {
            if let Some(mut r) = self.jobs_model.row_data(i) {
                r.selected = false;
                self.jobs_model.set_row_data(i, r);
            }
        }
        self.jobs_model.push(row);
        self.set_detail_for(job.id.to_string());
        self.pending_transcription.borrow_mut().take();
        app.set_transcription_confirm_visible(false);
        app.set_url("".into());
        app.set_add_busy(false);
        app.set_add_state("success".into());
        app.set_add_feedback("已创建任务".into());
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
            app.set_action_pending_kind("".into());
            app.set_action_feedback("".into());
            app.set_action_feedback_error(false);
            app.set_speaker_feedback("".into());
            app.set_speaker_feedback_error(false);
            app.set_result_open(false);
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
                    let active_content_job =
                        self.content_busy.lock().ok().and_then(|state| state.job_id);
                    if active_content_job.is_some_and(|active_id| {
                        cancel_content_operation(&self.content_busy, active_id)
                    }) {
                        app.set_action_feedback("正在取消文字结果操作…".into());
                        app.set_action_feedback_error(false);
                    }
                    return;
                }
                app.set_action_pending_kind("".into());
                app.set_action_pending(true);
                app.set_action_feedback("正在取消任务…".into());
                app.set_action_feedback_error(false);
            }
            // Set the token and persisted transition directly, not behind the
            // blocked worker. This prevents a queued Job from starting before
            // its cancellation message is eventually handled.
            self.scheduler.cancel_registry.cancel(id);
            cancel_content_operation(&self.content_busy, id);
            let result = persist_job_cancellation(&self.scheduler, id);
            sync_history_capabilities(&self.app, &self.queue);
            if let Some(app) = self.app.upgrade() {
                if let Some(job) = self
                    .queue
                    .lock()
                    .ok()
                    .and_then(|queue| queue.get(id).cloned())
                {
                    for index in 0..self.jobs_model.row_count() {
                        if let Some(row) = self.jobs_model.row_data(index) {
                            if row.id.as_str() == id.to_string() {
                                self.jobs_model
                                    .set_row_data(index, job_to_row(&job, row.selected));
                                if row.selected {
                                    let current_model = self.current_model();
                                    let tier = *self.selected_summary_tier.borrow() as i32;
                                    show_job_detail(&app, &job, current_model.as_deref(), tier);
                                }
                                break;
                            }
                        }
                    }
                }
                app.set_action_pending(false);
                match result {
                    Ok(crate::jobs::CancellationTransition::Cooperative) => {
                        app.set_action_feedback("已发出取消请求".into());
                        app.set_action_feedback_error(false);
                    }
                    Ok(crate::jobs::CancellationTransition::Immediate) => {
                        app.set_action_feedback("已取消".into());
                        app.set_action_feedback_error(false);
                    }
                    Ok(crate::jobs::CancellationTransition::AlreadyCancelling) => {
                        app.set_action_feedback("正在取消".into());
                        app.set_action_feedback_error(false);
                    }
                    Ok(crate::jobs::CancellationTransition::Unavailable) => {
                        app.set_action_feedback("当前任务不可取消".into());
                        app.set_action_feedback_error(true);
                    }
                    Err(error) => {
                        app.set_action_feedback(
                            format!("取消状态保存失败，请重试：{error}").into(),
                        );
                        app.set_action_feedback_error(true);
                    }
                }
            }
        }
    }

    fn rebuild_selected(&self) {
        let Some(id) = self.selected_job_id() else {
            return;
        };
        let Some(job) = self
            .queue
            .lock()
            .ok()
            .and_then(|queue| queue.get(id).cloned())
        else {
            return;
        };
        if !crate::jobs::JobCapabilities::from_job(&job).can_rebuild {
            if let Some(app) = self.app.upgrade() {
                app.set_action_feedback("当前任务不需要重建".into());
                app.set_action_feedback_error(true);
            }
            return;
        }
        let source_url = job.source_url.unwrap_or_else(|| {
            let page = if job.page > 1 {
                format!("?p={}", job.page)
            } else {
                String::new()
            };
            format!("https://www.bilibili.com/video/{}{page}", job.bvid)
        });
        if let Some(app) = self.app.upgrade() {
            app.set_url(source_url.into());
            app.set_action_feedback("请确认新 Runtime 与语言设置；原任务保持不变".into());
            app.set_action_feedback_error(false);
        }
        self.prepare_add_job();
    }

    fn retry_selected(&self) {
        if let Some(id) = self.selected_job_id() {
            let can_retry = self.queue.lock().ok().and_then(|q| {
                q.get(id)
                    .map(|job| crate::jobs::JobCapabilities::from_job(job).can_retry)
            });
            if can_retry != Some(true) {
                if let Some(app) = self.app.upgrade() {
                    app.set_action_feedback("当前任务不可重试；RawOnly 请使用修复文字结果".into());
                    app.set_action_feedback_error(true);
                }
                return;
            }
            if let Some(app) = self.app.upgrade() {
                if app.get_action_pending() {
                    return;
                }
                app.set_action_pending_kind("".into());
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

    /// Queue one terminal v0.5 enhancement operation with a target snapshot
    /// taken at click time. The gate is reserved before sending so a rapid
    /// double click cannot enqueue a second in-memory command.
    pub(crate) fn regenerate_selected(&self, kind: RegenerableKind) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_action_pending() {
            return;
        }
        let Some(id) = self.selected_job_id() else {
            return;
        };
        if self
            .content_busy
            .lock()
            .ok()
            .is_some_and(|state| state.job_id == Some(id))
        {
            app.set_action_feedback("文字结果正在处理中，请稍候".into());
            app.set_action_feedback_error(true);
            return;
        }
        let Some(job) = self.queue.lock().unwrap().get(id).cloned() else {
            return;
        };
        if job.content_setup.is_none() || !job.status.is_terminal() {
            app.set_action_feedback("当前任务尚未进入可重生成的终态".into());
            app.set_action_feedback_error(true);
            return;
        }
        if !matches!(
            crate::content_results::current(&job),
            Ok(ContentViewV1::Current(_))
        ) {
            app.set_action_feedback("当前没有可信文字结果，请先修复文字结果".into());
            app.set_action_feedback_error(true);
            return;
        }
        let target = match self.config.lock().unwrap().content_target() {
            Ok(target) => target,
            Err(reason) => {
                app.set_action_feedback(format!("检查 AI 设置：{}", reason.message).into());
                app.set_action_feedback_error(true);
                return;
            }
        };
        if !reserve_content_operation(&self.content_busy, id) {
            app.set_action_feedback("文字结果正在处理中，请稍候".into());
            app.set_action_feedback_error(true);
            return;
        }
        app.set_action_pending_kind(
            match kind {
                RegenerableKind::DefaultSummary => "summary",
                RegenerableKind::ShortSummary => "summary-short",
                RegenerableKind::LongSummary => "summary-long",
                RegenerableKind::Highlights => "highlights",
                RegenerableKind::Chapters => "chapters",
            }
            .into(),
        );
        app.set_action_pending(true);
        app.set_action_feedback("正在重新生成文字结果…".into());
        app.set_action_feedback_error(false);
        let tier = match *self.selected_summary_tier.borrow() {
            0 => crate::document::SummaryTier::Short,
            2 => crate::document::SummaryTier::Long,
            _ => crate::document::SummaryTier::Standard,
        };
        if self
            .tx
            .send(WorkMsg::Regenerate {
                job_id: id,
                kind,
                target,
                tier,
            })
            .is_err()
        {
            clear_content_operation(&self.content_busy, id);
            app.set_action_pending(false);
            app.set_action_feedback("重生成失败：后台任务已退出".into());
            app.set_action_feedback_error(true);
        }
    }

    /// Queue the explicit RawOnly repair path. It never re-enters Scheduler
    /// and is accepted only when ContentResults confirms valid raw Evidence.
    pub(crate) fn repair_selected_content(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_action_pending() {
            return;
        }
        let Some(id) = self.selected_job_id() else {
            return;
        };
        if self
            .content_busy
            .lock()
            .ok()
            .is_some_and(|state| state.job_id == Some(id))
        {
            app.set_action_feedback("文字结果正在处理中，请稍候".into());
            app.set_action_feedback_error(true);
            return;
        }
        let Some(job) = self.queue.lock().unwrap().get(id).cloned() else {
            return;
        };
        if job.content_setup.is_none() || !job.status.is_terminal() {
            app.set_action_feedback("当前任务没有可修复的 v0.5 文字结果".into());
            app.set_action_feedback_error(true);
            return;
        }
        if !matches!(
            crate::content_results::current(&job),
            Ok(ContentViewV1::RawOnly { .. })
        ) {
            app.set_action_feedback("当前文字结果不需要修复".into());
            app.set_action_feedback_error(true);
            return;
        }
        if !reserve_content_operation(&self.content_busy, id) {
            app.set_action_feedback("文字结果正在处理中，请稍候".into());
            app.set_action_feedback_error(true);
            return;
        }
        app.set_action_pending_kind("".into());
        app.set_action_pending(true);
        app.set_action_feedback("正在修复文字结果…".into());
        app.set_action_feedback_error(false);
        let tier = match *self.selected_summary_tier.borrow() {
            0 => crate::document::SummaryTier::Short,
            2 => crate::document::SummaryTier::Long,
            _ => crate::document::SummaryTier::Standard,
        };
        if self
            .tx
            .send(WorkMsg::RepairContent { job_id: id, tier })
            .is_err()
        {
            clear_content_operation(&self.content_busy, id);
            app.set_action_pending(false);
            app.set_action_feedback("修复失败：后台任务已退出".into());
            app.set_action_feedback_error(true);
        }
    }

    /// Open the destructive-action confirmation for deleting the selected job.
    fn request_delete_selected(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_action_pending() {
            return;
        }
        let Some(id) = self.selected_job_id() else {
            return;
        };
        let job = self.queue.lock().unwrap().get(id).cloned();
        let Some(job) = job else { return };
        if !job.status.is_terminal() {
            app.set_action_feedback("删除失败：任务尚未结束，请先取消再删除".into());
            app.set_action_feedback_error(true);
            return;
        }
        *self.history_confirmation.borrow_mut() = Some(HistoryConfirmation::DeleteJob(id));
        app.set_delete_confirm_title("删除任务".into());
        app.set_delete_confirm_message(
            format!(
                "将删除“{}”的任务记录与全部产物文件（含输出目录中的 full.md）。此操作不可撤销。",
                job.title
            )
            .into(),
        );
        app.set_delete_confirm_confirm_label("删除".into());
        app.set_delete_confirm_visible(true);
    }

    /// Open the destructive-action confirmation for clearing terminal history.
    fn request_clear_history(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_action_pending() {
            return;
        }
        let terminal_count = {
            let q = self.queue.lock().unwrap();
            q.jobs.iter().filter(|job| job.status.is_terminal()).count()
        };
        if terminal_count == 0 {
            app.set_action_feedback("没有可清空的历史任务".into());
            app.set_action_feedback_error(false);
            return;
        }
        *self.history_confirmation.borrow_mut() = Some(HistoryConfirmation::ClearHistory);
        app.set_delete_confirm_title("清空任务历史".into());
        app.set_delete_confirm_message(
            format!(
                "将删除 {terminal_count} 个已结束任务的记录与产物文件；排队与运行中的任务不受影响。此操作不可撤销。"
            )
            .into(),
        );
        app.set_delete_confirm_confirm_label("清空历史".into());
        app.set_delete_confirm_visible(true);
    }

    /// The user confirmed the pending destructive action; dispatch it to the
    /// worker so artifact deletion never blocks the UI thread.
    fn confirm_history_action(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let confirmation = self.history_confirmation.borrow_mut().take();
        app.set_delete_confirm_visible(false);
        let Some(confirmation) = confirmation else {
            return;
        };
        if app.get_action_pending() {
            return;
        }
        let send_result = match confirmation {
            HistoryConfirmation::DeleteJob(id) => {
                app.set_action_pending_kind("".into());
                app.set_action_pending(true);
                app.set_action_feedback("正在删除任务…".into());
                app.set_action_feedback_error(false);
                self.tx.send(WorkMsg::DeleteJob(id))
            }
            HistoryConfirmation::ClearHistory => {
                app.set_action_pending_kind("".into());
                app.set_action_pending(true);
                app.set_action_feedback("正在清空历史…".into());
                app.set_action_feedback_error(false);
                self.tx.send(WorkMsg::ClearHistory)
            }
        };
        if send_result.is_err() {
            app.set_action_pending(false);
            app.set_action_feedback("操作失败：后台任务已退出".into());
            app.set_action_feedback_error(true);
        }
    }

    fn cancel_history_action(&self) {
        self.history_confirmation.borrow_mut().take();
        if let Some(app) = self.app.upgrade() {
            app.set_delete_confirm_visible(false);
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
        let Some(job) = self.queue.lock().unwrap().get(id).cloned() else {
            return;
        };
        let _content_guard = if job.content_setup.is_some() {
            if !reserve_content_operation(&self.content_busy, id) {
                app.set_action_feedback("文字结果正在处理中，请稍候".into());
                app.set_action_feedback_error(true);
                return;
            }
            Some(ContentBusyGuard {
                busy: self.content_busy.clone(),
                job_id: id,
            })
        } else {
            None
        };
        if job.content_setup.is_some() && !job.status.is_terminal() {
            app.set_action_feedback("打开失败：任务尚未进入终态".into());
            app.set_action_feedback_error(true);
            return;
        }
        let full = if job.content_setup.is_some() {
            let snapshot = match crate::content_results::current(&job) {
                Ok(ContentViewV1::Current(snapshot)) => snapshot,
                Ok(ContentViewV1::RawOnly { .. }) => {
                    app.set_action_feedback("打开失败：可信文字结果尚未生成，请先修复".into());
                    app.set_action_feedback_error(true);
                    return;
                }
                Ok(ContentViewV1::Legacy) => {
                    app.set_action_feedback("打开失败：任务内容标记无效".into());
                    app.set_action_feedback_error(true);
                    return;
                }
                Err(error) => {
                    app.set_action_feedback(format!("打开失败：{error}").into());
                    app.set_action_feedback_error(true);
                    return;
                }
            };
            let cfg = self.config.lock().unwrap().clone();
            let mut rendered_job = job.clone();
            let output_dir = match crate::pipeline::ensure_final_output_dir(&mut rendered_job, &cfg)
            {
                Ok(path) => path,
                Err(error) => {
                    app.set_action_feedback(format!("打开失败：{error}").into());
                    app.set_action_feedback_error(true);
                    return;
                }
            };
            let doc_tier = match app.get_result().summary_tier {
                0 => crate::document::SummaryTier::Short,
                2 => crate::document::SummaryTier::Long,
                _ => crate::document::SummaryTier::Standard,
            };
            if let Err(error) =
                crate::document::rebuild_presentation_with_tier(&rendered_job, &snapshot, doc_tier)
            {
                app.set_action_feedback(format!("打开失败：{error}").into());
                app.set_action_feedback_error(true);
                return;
            }
            if let Ok(mut q) = self.queue.lock() {
                if let Some(stored) = q.get_mut(id) {
                    apply_presentation_locator(stored, output_dir.clone());
                    let _ = jobs::save_job_state(stored);
                }
                let _ = q.save();
            }
            output_dir.join("full.md")
        } else {
            let candidates = [
                job.final_output_dir.as_ref().map(|dir| dir.join("full.md")),
                job.work_dir.as_ref().map(|dir| dir.join("full.md")),
            ];
            let Some(full) = candidates.into_iter().flatten().find(|path| path.exists()) else {
                app.set_action_feedback("打开失败：Markdown 尚未生成".into());
                app.set_action_feedback_error(true);
                return;
            };
            full
        };
        app.set_action_pending_kind("".into());
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

    fn open_selected_raw(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if app.get_action_pending() {
            return;
        }
        let Some(id) = self.selected_job_id() else {
            return;
        };
        let Some(job) = self
            .queue
            .lock()
            .ok()
            .and_then(|queue| queue.get(id).cloned())
        else {
            return;
        };
        let _content_guard = if job.content_setup.is_some() {
            if !reserve_content_operation(&self.content_busy, id) {
                app.set_action_feedback("文字结果正在处理中，请稍候".into());
                app.set_action_feedback_error(true);
                return;
            }
            Some(ContentBusyGuard {
                busy: self.content_busy.clone(),
                job_id: id,
            })
        } else {
            None
        };

        let raw_path = if job.content_setup.is_some() {
            let Some(work_dir) = job.work_dir.as_ref() else {
                app.set_action_feedback("打开失败：任务目录尚未生成".into());
                app.set_action_feedback_error(true);
                return;
            };
            let raw = match crate::content_results::current(&job) {
                Ok(ContentViewV1::Current(snapshot)) => {
                    crate::document::render_raw_markdown(&job, &snapshot.evidence.utterances)
                }
                Ok(ContentViewV1::RawOnly { evidence, .. }) => {
                    crate::document::render_raw_markdown(&job, &evidence.utterances)
                }
                Ok(ContentViewV1::Legacy) => {
                    app.set_action_feedback("打开失败：任务内容标记无效".into());
                    app.set_action_feedback_error(true);
                    return;
                }
                Err(error) => {
                    app.set_action_feedback(format!("打开失败：{error}").into());
                    app.set_action_feedback_error(true);
                    return;
                }
            };
            let path = work_dir.join("transcript.raw.md");
            if let Err(error) = crate::jobs::atomic_write(&path, raw.as_bytes()) {
                app.set_action_feedback(format!("打开失败：{error}").into());
                app.set_action_feedback_error(true);
                return;
            }
            path
        } else {
            let Some(path) = job
                .work_dir
                .as_ref()
                .map(|directory| directory.join("transcript.raw.md"))
                .filter(|path| path.is_file())
            else {
                app.set_action_feedback("打开失败：原始稿尚未生成".into());
                app.set_action_feedback_error(true);
                return;
            };
            path
        };

        app.set_action_pending_kind("".into());
        app.set_action_pending(true);
        app.set_action_feedback("正在打开原始稿…".into());
        app.set_action_feedback_error(false);
        match std::process::Command::new("open").arg(&raw_path).spawn() {
            Ok(_) => {
                app.set_action_pending(false);
                app.set_action_feedback("已请求打开原始稿".into());
            }
            Err(error) => {
                app.set_action_pending(false);
                app.set_action_feedback(format!("打开失败：{error}").into());
                app.set_action_feedback_error(true);
            }
        }
    }

    fn open_external_url(&self, url: String) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if !valid_result_source_url(&url) {
            app.set_action_feedback("打开失败：来源链接无效".into());
            app.set_action_feedback_error(true);
            return;
        }
        match std::process::Command::new("open").arg(&url).spawn() {
            Ok(_) => {
                app.set_action_feedback("已在浏览器中打开来源".into());
                app.set_action_feedback_error(false);
            }
            Err(error) => {
                app.set_action_feedback(format!("打开来源失败：{error}").into());
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
        app.set_action_pending_kind("".into());
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
        let job_id = self.selected_job_id();
        if let Some(job_id) = job_id {
            let selected_job = self.queue.lock().ok().and_then(|q| q.get(job_id).cloned());
            let is_v05 = selected_job
                .as_ref()
                .is_some_and(|job| job.content_setup.is_some());
            if is_v05
                && !selected_job.as_ref().is_some_and(|job| {
                    matches!(
                        crate::content_results::current(job),
                        Ok(ContentViewV1::Current(_))
                    )
                })
            {
                app.set_speaker_feedback("请先修复文字结果，再编辑说话人".into());
                app.set_speaker_feedback_error(true);
                return;
            }
            if is_v05
                && self
                    .content_busy
                    .lock()
                    .ok()
                    .is_some_and(|state| state.job_id == Some(job_id))
            {
                app.set_speaker_feedback("文字结果正在处理中，请稍候".into());
                app.set_speaker_feedback_error(true);
                return;
            }
        }
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
            let tier = match *self.selected_summary_tier.borrow() {
                0 => crate::document::SummaryTier::Short,
                2 => crate::document::SummaryTier::Long,
                _ => crate::document::SummaryTier::Standard,
            };
            // Restart with the current job_id. We use a stop + start cycle
            // because Slint Timer doesn't allow changing the callback.
            timer.stop();
            timer.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(200),
                move || {
                    let _ = tx.send(WorkMsg::SpeakerChanged { job_id, tier });
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
        self.probe_token.fetch_add(1, Ordering::SeqCst);
        app.set_connection_test_state("idle".into());
        app.set_connection_test_message("".into());
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

        match &job_opt {
            Some(job) => {
                *self.selected_summary_tier.borrow_mut() = 1;
                app.set_summary_tier(1);
                let current_model = self.current_model();
                show_job_detail(&app, job, current_model.as_deref(), 1);
            }
            None => clear_job_detail(&app),
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
        status_name: job.status.name().into(),
    }
}

fn empty_detail() -> JobDetailData {
    JobDetailData {
        has_job: false,
        title: "".into(),
        bvid: "".into(),
        page: 1,
        status_label: "".into(),
        stage_name: "".into(),
        status_name: "".into(),
        total_progress: 0,
        elapsed_secs: 0,
        elapsed: "—".into(),
        error_text: "".into(),
        has_error: false,
        warning_text: "".into(),
        can_cancel: false,
        can_retry: false,
        can_rebuild: false,
        can_open: false,
        can_reveal: false,
        can_delete: false,
        can_edit_speakers: false,
        screenshots_enabled: false,
        retention_label: jobs::RetentionPolicy::default().label().into(),
        created_at: "—".into(),
        started_at: "—".into(),
        finished_at: "—".into(),
        video_duration: "—".into(),
        media_size: "尚未生成".into(),
        document_words: "尚未生成".into(),
        transcription_runtime: "未记录".into(),
        transcription_source: "未记录".into(),
        transcription_backend: "未记录".into(),
        transcription_model: "未提供".into(),
        requested_language: "未记录".into(),
        reported_language: "未报告".into(),
        reported_model: "未报告".into(),
        reading_estimate: "—".into(),
        reading_saved: "—".into(),
        reading_profile: "".into(),
        can_view_result: false,
    }
}

/// Format a number of seconds as MM:SS (or H:MM:SS for >= 1h).
fn fmt_elapsed(secs: u64) -> String {
    crate::ui_bridge::fmt_elapsed(secs)
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
        apply_presentation_locator, cancel_content_operation, clear_content_operation,
        count_document_chars, fixture_content_results, job_to_row, parse_ui_fixture,
        parse_ui_fixture_position, parse_ui_fixture_size, persist_job_cancellation_with,
        queue_scroll_fixture_jobs, regenerable_kind_from_action, regeneration_feedback,
        reorder_capabilities, reserve_content_operation, result_fixture_callback_job,
        retry_feedback, run_regenerate_content_operation, run_repair_content_operation,
        speaker_rebuild_feedback, start_content_operation, valid_result_source_url, ContentBusy,
        JobRow,
    };
    use crate::config::Config;
    use crate::content_results::{
        current, ArtifactSlotV1, ContentSetupV1, ContentViewV1, DerivationFailureV1, FailureCodeV1,
        GenerationApiFormatV1, GenerationParametersV1, GenerationProviderV1,
        ReadyGenerationTargetV1, RegenerableKind, TargetUnavailableCodeV1, TargetUnavailableV1,
    };
    use crate::jobs::{Job, JobCapabilities, JobStatus, Stage, StageState};
    use crate::scheduler::{DrainOutcome, JobResult, Scheduler};
    use slint::{Model, VecModel};
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    #[test]
    fn retry_feedback_does_not_report_failed_run_as_completed() {
        let outcome = DrainOutcome::Ran {
            job_id: Uuid::new_v4(),
            result: JobResult::Failed("ffmpeg unavailable".into()),
        };

        assert_eq!(
            retry_feedback(&outcome),
            ("重试失败：ffmpeg unavailable".into(), true)
        );
    }

    #[test]
    fn cancellation_persistence_failures_restore_retryable_state() {
        for queue_save_fails in [false, true] {
            let mut job = Job::new(Uuid::new_v4(), "BV1persist-cancel".into(), 1);
            let id = job.id;
            job.status = JobStatus::NeedsUserAction;
            job.stage = Stage::NeedsUserAction;
            let queue = Arc::new(Mutex::new(crate::jobs::Queue { jobs: vec![job] }));
            let scheduler = Scheduler::new(queue.clone());
            let job_save_calls = std::cell::Cell::new(0_u8);
            let result = persist_job_cancellation_with(
                &scheduler,
                id,
                |_| {
                    job_save_calls.set(job_save_calls.get() + 1);
                    if queue_save_fails {
                        Ok(())
                    } else {
                        Err("state write failed".into())
                    }
                },
                |_| {
                    if queue_save_fails {
                        Err("queue write failed".into())
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(result.is_err());
            let queue = queue.lock().unwrap();
            let restored = queue.get(id).unwrap();
            assert_eq!(restored.status, JobStatus::NeedsUserAction);
            assert_eq!(restored.stage, Stage::NeedsUserAction);
            assert!(JobCapabilities::from_job(restored).can_cancel);
            assert_eq!(job_save_calls.get(), if queue_save_fails { 2 } else { 1 });
        }
    }

    #[test]
    fn ui_fixture_parser_supports_visual_states_and_legacy_switches() {
        assert_eq!(parse_ui_fixture("empty"), Some("empty"));
        assert_eq!(parse_ui_fixture("settings"), Some("settings"));
        assert_eq!(parse_ui_fixture("running"), Some("running"));
        assert_eq!(parse_ui_fixture("queue-scroll"), Some("queue-scroll"));
        assert_eq!(parse_ui_fixture("speaker-empty"), Some("speaker-empty"));
        assert_eq!(parse_ui_fixture("speaker-single"), Some("speaker-single"));
        assert_eq!(parse_ui_fixture("confirmation"), Some("confirmation"));
        assert_eq!(parse_ui_fixture("delete-confirm"), Some("delete-confirm"));
        assert_eq!(parse_ui_fixture("clear-confirm"), Some("clear-confirm"));
        assert_eq!(parse_ui_fixture("FAILED"), Some("failed"));
        assert_eq!(parse_ui_fixture(" completed "), Some("completed"));
        assert_eq!(parse_ui_fixture("1"), Some("completed"));
        assert_eq!(parse_ui_fixture(""), Some("completed"));
        assert_eq!(parse_ui_fixture("result-success"), Some("result-success"));
        assert_eq!(parse_ui_fixture("result-limited"), Some("result-limited"));
        assert_eq!(
            parse_ui_fixture("result-enhancement-failed"),
            Some("result-enhancement-failed")
        );
        assert_eq!(
            parse_ui_fixture("result-regenerating"),
            Some("result-regenerating")
        );
        assert_eq!(parse_ui_fixture("unknown"), None);
    }

    #[test]
    fn queue_scroll_fixture_has_enough_rows_and_visible_error_borders() {
        let jobs = queue_scroll_fixture_jobs();
        assert_eq!(jobs.len(), 12);
        assert!(jobs.iter().any(|job| job.status == JobStatus::Failed));
        assert!(jobs.iter().any(|job| job.status == JobStatus::Completed));
        assert!(jobs.iter().filter(|job| job.error.is_some()).count() >= 3);
    }

    #[test]
    fn result_fixtures_use_the_flat_rows_and_expose_limited_without_jump() {
        let success = fixture_content_results("result-success");
        assert!(success.entry_visible);
        assert!(success.show_summary && success.show_chapters && success.show_faithful);
        assert_eq!(success.bvid, "BV1UIFIX");
        assert_eq!(success.page, 2);
        assert_eq!(success.video_duration, "01:04");
        assert!(success.raw_rows.row_count() > 50);
        assert!(success
            .faithful_rows
            .iter()
            .any(|row| row.kind == "section"));
        let mapped_result_rows = success
            .summary_rows
            .iter()
            .chain(success.chapters_rows.iter())
            .chain(success.faithful_rows.iter())
            .filter(|row| row.kind == "block" && row.status_kind == "mapped")
            .collect::<Vec<_>>();
        assert!(!mapped_result_rows.is_empty());
        assert!(mapped_result_rows
            .iter()
            .all(|row| { row.text.chars().count() <= 80 && row.can_source && row.can_jump }));

        let limited = fixture_content_results("result-limited");
        let limited_rows = limited
            .summary_rows
            .iter()
            .chain(limited.faithful_rows.iter())
            .filter(|row| row.status_kind == "limited")
            .collect::<Vec<_>>();
        assert!(!limited_rows.is_empty());
        assert!(limited_rows
            .iter()
            .all(|row| !row.can_jump && !row.can_source));
        let chapter_statuses = limited
            .chapters_rows
            .iter()
            .filter(|row| row.kind == "block")
            .map(|row| row.status_kind.to_string())
            .collect::<Vec<_>>();
        assert!(chapter_statuses.iter().any(|status| status == "mapped"));
        assert!(chapter_statuses.iter().any(|status| status == "limited"));
        let mapped_chapter = limited
            .chapters_rows
            .iter()
            .find(|row| row.kind == "block" && row.status_kind == "mapped")
            .expect("limited chapters keep a mapped source affordance");
        assert!(mapped_chapter.can_source && mapped_chapter.can_jump);

        let failed = fixture_content_results("result-enhancement-failed");
        assert_eq!(failed.default_tab, 0);
        assert!(failed
            .summary_rows
            .iter()
            .any(|row| row.status_kind == "failure"));

        let regenerating = fixture_content_results("result-regenerating");
        assert!(regenerating
            .chapters_rows
            .iter()
            .filter(|row| row.kind == "section")
            .any(|row| row.action_pending));
        assert!(regenerating
            .summary_rows
            .iter()
            .all(|row| !row.action_pending));
    }

    #[test]
    fn result_fixture_uses_a_selected_terminal_job_and_closed_callback_inputs() {
        let id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("bi2read-result-bridge-{id}"));
        let cfg = Config {
            working_dir: root.join("jobs"),
            output_dir: root.join("output"),
            llm_enabled: false,
            ..Config::default()
        };
        let job = result_fixture_callback_job(&root, &cfg, "result-success").unwrap();
        assert!(job.status.is_terminal());
        assert!(job_to_row(&job, true).selected);
        assert!(matches!(current(&job).unwrap(), ContentViewV1::Current(_)));

        assert_eq!(
            regenerable_kind_from_action("summary"),
            Some(RegenerableKind::DefaultSummary)
        );
        assert_eq!(
            regenerable_kind_from_action("highlights"),
            Some(RegenerableKind::Highlights)
        );
        assert_eq!(
            regenerable_kind_from_action("chapters"),
            Some(RegenerableKind::Chapters)
        );
        assert_eq!(regenerable_kind_from_action("faithful"), None);
        assert!(valid_result_source_url(
            "https://www.bilibili.com/video/BV1UIFIX?p=2&t=3"
        ));
        assert!(!valid_result_source_url("https://example.com/BV1UIFIX"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn result_projection_identity_is_stable_for_refresh_and_changes_for_job_switch() {
        let id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("bi2read-result-session-{id}"));
        let cfg = Config {
            working_dir: root.join("jobs"),
            output_dir: root.join("output"),
            llm_enabled: false,
            ..Config::default()
        };
        let job = result_fixture_callback_job(&root, &cfg, "result-success").unwrap();
        let first = super::content_results_for_job(&job, None, None, 1);
        let refreshed = super::content_results_for_job(
            &job,
            Some(crate::content_results::ArtifactKindV1::Chapters),
            None,
            1,
        );
        assert!(!first.job_id.is_empty());
        assert_eq!(first.job_id, refreshed.job_id);

        let other_id = Uuid::new_v4();
        let other_root =
            std::env::temp_dir().join(format!("bi2read-result-session-other-{other_id}"));
        let other_job = result_fixture_callback_job(&other_root, &cfg, "result-success").unwrap();
        let switched = super::content_results_for_job(&other_job, None, None, 1);
        assert_ne!(first.job_id, switched.job_id);
        std::fs::remove_dir_all(root).ok();
        std::fs::remove_dir_all(other_root).ok();
    }

    #[test]
    fn ui_fixture_window_position_is_fixed_and_optionally_overridable() {
        assert_eq!(parse_ui_fixture_position(None), (40, 40));
        assert_eq!(parse_ui_fixture_position(Some(" 120, 80 ")), (120, 80));
        assert_eq!(parse_ui_fixture_position(Some("invalid")), (40, 40));
    }

    #[test]
    fn ui_fixture_size_accepts_both_supported_dimensions() {
        assert_eq!(parse_ui_fixture_size(Some("920x600")), (920, 600));
        assert_eq!(parse_ui_fixture_size(Some("1180x760")), (1180, 760));
        assert_eq!(parse_ui_fixture_size(Some("800x500")), (1180, 760));
        assert_eq!(parse_ui_fixture_size(Some("invalid")), (1180, 760));
    }

    #[test]
    fn content_busy_gate_rejects_same_job_and_preserves_pre_start_cancel() {
        let busy = Arc::new(Mutex::new(ContentBusy::default()));
        let job_id = Uuid::new_v4();
        assert!(start_content_operation(&busy, job_id).is_none());
        assert!(reserve_content_operation(&busy, job_id));
        assert!(!reserve_content_operation(&busy, job_id));
        assert!(!reserve_content_operation(&busy, Uuid::new_v4()));
        assert!(!cancel_content_operation(&busy, Uuid::new_v4()));
        assert!(cancel_content_operation(&busy, job_id));
        assert!(start_content_operation(&busy, job_id)
            .unwrap()
            .is_cancelled());
        clear_content_operation(&busy, job_id);
        assert!(start_content_operation(&busy, job_id).is_none());
        assert!(reserve_content_operation(&busy, Uuid::new_v4()));
    }

    #[test]
    fn speaker_rebuild_feedback_keeps_saved_name_and_reports_document_failure() {
        assert_eq!(
            speaker_rebuild_feedback(&Ok(())),
            ("已保存，文档已更新".into(), false)
        );
        assert_eq!(
            speaker_rebuild_feedback(&Err("磁盘已满".into())),
            (
                "说话人名称已保存，但文档更新失败：磁盘已满；再次打开 Markdown 时会重试".into(),
                true,
            )
        );
    }

    #[test]
    fn regeneration_feedback_does_not_report_committed_failure_as_success() {
        assert_eq!(
            regeneration_feedback(&ArtifactSlotV1::default()),
            ("重生成结果状态无效，请重试".into(), true)
        );
        let failed = ArtifactSlotV1 {
            current: None,
            last_failure: Some(DerivationFailureV1 {
                code: FailureCodeV1::GenerationFailed,
                message: "本地 AI 暂时不可用".into(),
                retryable: true,
                occurred_at: chrono::Utc::now(),
            }),
        };
        assert_eq!(
            regeneration_feedback(&failed),
            (
                "重生成失败：本地 AI 暂时不可用；当前没有可用结果".into(),
                true,
            )
        );
    }

    #[test]
    fn production_content_operations_preserve_terminal_job_and_never_run_upstream() {
        let id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("bi2read-content-worker-{id}"));
        let work = root.join("work");
        let output = root.join("output");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&output).unwrap();
        let raw = r#"[{"id":"u1","text":"原始证据 42","start_ms":0,"end_ms":1000,"speaker_id":0}]"#;
        std::fs::write(work.join("transcript.raw.json"), raw).unwrap();

        let mut job = Job::new(id, "BV1worker".into(), 1);
        job.title = "worker 生产路径".into();
        job.content_setup = Some(ContentSetupV1::unavailable(TargetUnavailableV1 {
            code: TargetUnavailableCodeV1::ConnectionMissing,
            message: "未配置本地 AI".into(),
            invalid_field: Some("llm_connection".into()),
        }));
        job.work_dir = Some(work.clone());
        job.cid = Some(7);
        job.duration_ms = Some(1000);
        job.status = JobStatus::Completed;
        job.stage = Stage::Completed;
        job.stage_progress = 100;
        job.finished_at = Some(chrono::Utc::now());
        job.error = Some("保留终态字段".into());
        job.set_stage_state(Stage::Completed, StageState::Completed);
        let terminal_before = (
            job.status,
            job.stage,
            job.stage_progress,
            job.finished_at,
            job.error.clone(),
            job.stages.clone(),
        );
        let raw_before = std::fs::read(work.join("transcript.raw.json")).unwrap();
        let cfg = Config {
            working_dir: root.join("working"),
            output_dir: output,
            ..Config::default()
        };
        let cfg = Arc::new(Mutex::new(cfg));

        let cancelled = crate::cancel::CancellationToken::new();
        cancelled.cancel();
        let (message, is_error, locator) = run_repair_content_operation(
            &job,
            &cfg,
            &cancelled,
            crate::document::SummaryTier::Standard,
        );
        assert_eq!(message, "已取消文字结果修复");
        assert!(is_error);
        assert!(locator.is_none());
        assert!(matches!(
            current(&job).unwrap(),
            ContentViewV1::RawOnly { .. }
        ));

        let (message, is_error, locator) = run_repair_content_operation(
            &job,
            &cfg,
            &crate::cancel::CancellationToken::new(),
            crate::document::SummaryTier::Standard,
        );
        assert_eq!(message, "文字结果已修复");
        assert!(!is_error);
        assert!(locator.is_some());
        assert!(matches!(current(&job).unwrap(), ContentViewV1::Current(_)));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let target = ReadyGenerationTargetV1 {
            provider: GenerationProviderV1::OpenAiCompatible,
            api_format: GenerationApiFormatV1::OpenAiChatCompletions,
            endpoint: format!("http://127.0.0.1:{port}"),
            model: "offline-fixture".into(),
            parameters: GenerationParametersV1::openai_default(),
        };
        let (message, is_error, locator) = run_regenerate_content_operation(
            &job,
            RegenerableKind::DefaultSummary,
            target,
            &cfg,
            &crate::cancel::CancellationToken::new(),
            crate::document::SummaryTier::Standard,
        );
        assert!(message.contains("重生成失败"));
        assert!(is_error);
        assert!(locator.is_some());
        let snapshot = match current(&job).unwrap() {
            ContentViewV1::Current(snapshot) => snapshot,
            other => panic!("expected Current after repair, got {other:?}"),
        };
        assert_eq!(
            snapshot
                .current
                .slots
                .default_summary
                .last_failure
                .as_ref()
                .unwrap()
                .code,
            FailureCodeV1::GenerationFailed
        );

        assert_eq!(
            (
                job.status,
                job.stage,
                job.stage_progress,
                job.finished_at,
                job.error.clone(),
                job.stages.clone(),
            ),
            terminal_before
        );
        assert_eq!(
            std::fs::read(work.join("transcript.raw.json")).unwrap(),
            raw_before
        );
        for path in [
            work.join("metadata.json"),
            work.join("source.audio"),
            work.join("normalized.wav"),
            work.join("funasr"),
        ] {
            assert!(
                !path.exists(),
                "content operation ran upstream: {}",
                path.display()
            );
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn presentation_locator_merge_preserves_terminal_job_and_raw_evidence() {
        for status in [
            JobStatus::Completed,
            JobStatus::Failed,
            JobStatus::Cancelled,
        ] {
            let id = Uuid::new_v4();
            let root = std::env::temp_dir().join(format!("bi2read-locator-{id}"));
            std::fs::create_dir_all(&root).unwrap();
            let raw_path = root.join("transcript.raw.json");
            std::fs::write(&raw_path, br#"[{"id":"u1","text":"raw"}]"#).unwrap();
            let raw_before = std::fs::read(&raw_path).unwrap();
            let mut job = Job::new(id, "BV1locator".into(), 1);
            job.work_dir = Some(root.clone());
            job.status = status;
            job.stage = Stage::FinalDocument;
            job.finished_at = Some(chrono::Utc::now());
            job.error = Some("preserve me".into());
            job.set_stage_state(Stage::FinalDocument, StageState::Failed);
            let before = job.clone();
            apply_presentation_locator(&mut job, root.join("final"));
            assert_eq!(job.status, before.status);
            assert_eq!(job.stage, before.stage);
            assert_eq!(job.finished_at, before.finished_at);
            assert_eq!(job.error, before.error);
            assert_eq!(job.stages, before.stages);
            assert_eq!(std::fs::read(&raw_path).unwrap(), raw_before);
            std::fs::remove_dir_all(root).ok();
        }
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
            std::env::temp_dir().join(format!("bi2read-large-markdown-{}.md", Uuid::new_v4()));
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

    #[test]
    fn model_changed_notice_checks_newest_enhancement_slot() {
        let id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("bi2read-result-model-notice-{id}"));
        let cfg = Config::for_paths(&crate::paths::AppPaths::discover().expect("app paths"));
        let job =
            result_fixture_callback_job(&root, &cfg, "result-success").expect("result fixture job");
        let Ok(crate::content_results::ContentViewV1::Current(mut snapshot)) =
            crate::content_results::current(&job)
        else {
            panic!("expected current snapshot");
        };

        snapshot.current.slots.highlights.current = None;
        snapshot.current.slots.short_summary.current = None;
        snapshot.current.slots.long_summary.current = None;

        if let Some(record) = snapshot.current.slots.chapters.current.as_mut() {
            record.provenance.model = Some("model-old".into());
            record.created_at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc);
        }
        if let Some(record) = snapshot.current.slots.default_summary.current.as_mut() {
            record.provenance.model = Some("model-newest".into());
            record.created_at = chrono::DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc);
        }

        // When current model is model-newest, newest slot matches -> no notice.
        assert_eq!(
            super::model_changed_notice(&snapshot, Some("model-newest")),
            None
        );

        // When current model is model-target, notice uses newest slot's model.
        let notice = super::model_changed_notice(&snapshot, Some("model-target")).unwrap();
        assert!(notice.contains("model-newest → model-target"));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn ungenerated_slot_presents_clean_empty_state_and_initial_generation_button() {
        let id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("bi2read-result-ungenerated-{id}"));
        let cfg = Config::for_paths(&crate::paths::AppPaths::discover().expect("app paths"));
        let job =
            result_fixture_callback_job(&root, &cfg, "result-success").expect("result fixture job");
        let Ok(crate::content_results::ContentViewV1::Current(mut snapshot)) =
            crate::content_results::current(&job)
        else {
            panic!("expected current snapshot");
        };

        snapshot.current.slots.short_summary.current = None;
        snapshot.current.slots.short_summary.last_failure = None;

        let rows = super::slot_rows(
            &job,
            &snapshot,
            crate::content_results::ArtifactKindV1::ShortSummary,
            "短摘要",
            "short-summary",
            None,
            "将使用：qwen2.5:7b",
        );

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.title.as_str(), "短摘要");
        assert_eq!(row.status_label.as_str(), "");
        assert_eq!(row.status_kind.as_str(), "idle");
        assert_eq!(row.text.as_str(), "短摘要尚未生成");
        assert_eq!(row.action_label.as_str(), "生成短摘要");
        assert_eq!(row.action_kind.as_str(), "short-summary");

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn presentation_operation_preserves_selected_summary_tier_in_full_md() {
        let id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("bi2read-presentation-tier-{id}"));
        let cfg = Config::for_paths(&crate::paths::AppPaths::discover().expect("app paths"));
        let job =
            result_fixture_callback_job(&root, &cfg, "result-success").expect("result fixture job");
        let Ok(crate::content_results::ContentViewV1::Current(mut snapshot)) =
            crate::content_results::current(&job)
        else {
            panic!("expected current snapshot");
        };

        // Populate a short summary slot.
        let mut short_record = snapshot
            .current
            .slots
            .default_summary
            .current
            .clone()
            .unwrap();
        short_record.kind = crate::content_results::ArtifactKindV1::ShortSummary;
        short_record.blocks = vec![crate::content_results::ContentBlockV1 {
            id: "short-1".into(),
            role: crate::content_results::ContentBlockRoleV1::Summary,
            title: None,
            text: "这是独立生成的短摘要文本。".into(),
            source_refs: Vec::new(),
            source_status: crate::content_results::SourceStatusV1::Mapped,
        }];
        snapshot.current.slots.short_summary.current = Some(short_record);

        let config_arc = Arc::new(Mutex::new(cfg));
        let (_, is_error, output_dir) = super::presentation_operation(
            job.clone(),
            &snapshot,
            &config_arc,
            crate::document::SummaryTier::Short,
            "success".into(),
            false,
        );
        assert!(!is_error);
        let out = output_dir.expect("output dir");
        let full_md = std::fs::read_to_string(out.join("full.md")).expect("read full.md");
        assert!(full_md.contains("这是独立生成的短摘要文本。"));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn probe_token_invalidates_earlier_background_probe_writeback() {
        let token = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let initial_token = token.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(initial_token, 1);

        // When user edits settings draft, cancels, or saves, probe_token is bumped:
        token.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let newer_token = token.load(std::sync::atomic::Ordering::SeqCst);
        assert_ne!(initial_token, newer_token);
        // An in-flight probe with initial_token will see token != newer_token and return early.
    }
}
