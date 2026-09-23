//! Slint presentation adapter for persisted job state and pipeline progress.
//!
//! Pipeline and scheduler modules report domain state through these functions;
//! all mapping into generated Slint view models stays local to this module.

use slint::{Model, Weak};
use uuid::Uuid;

use crate::jobs::Stage;
use crate::App;

/// Apply a complete persisted snapshot to the queue row and selected detail.
pub(crate) fn apply_snapshot(app: &Weak<App>, snapshot: crate::jobs::JobViewSnapshot) {
    let app = app.clone();
    app.upgrade_in_event_loop(move |app| {
        apply_snapshot_inner(&app, &snapshot);
    })
    .ok();
}

fn apply_snapshot_inner(app: &crate::App, snap: &crate::jobs::JobViewSnapshot) {
    let id = snap.id.to_string();
    let stage_views = stage_views_from_snapshot(snap);

    if let Some(model) = app
        .get_stages()
        .as_any()
        .downcast_ref::<slint::VecModel<crate::StageView>>()
    {
        model.set_vec(stage_views);
    }

    let model = app.get_jobs();
    let mut found_selected = false;
    for index in 0..model.row_count() {
        if let Some(mut row) = model.row_data(index) {
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
                model.set_row_data(index, row.clone());
                found_selected = row.selected;
                break;
            }
        }
    }

    if found_selected {
        let capabilities = &snap.capabilities;
        let mut detail = app.get_detail();
        detail.has_job = true;
        detail.title = snap.title.clone().into();
        detail.bvid = snap.bvid.clone().into();
        detail.page = snap.page as i32;
        detail.status_label = snap.status.label().into();
        detail.total_progress = snap.total_progress as i32;
        detail.elapsed_secs = snap.elapsed_secs as i32;
        detail.elapsed = fmt_elapsed(snap.elapsed_secs).into();
        detail.error_text = snap.error.clone().unwrap_or_default().into();
        detail.has_error = snap.error.is_some();
        detail.warning_text = snap
            .warning
            .as_ref()
            .map(|warning| warning.label())
            .unwrap_or_default()
            .into();
        detail.can_cancel = capabilities.can_cancel;
        detail.can_retry = capabilities.can_retry;
        detail.can_rebuild = capabilities.can_rebuild;
        detail.can_open = capabilities.can_open_document;
        detail.can_reveal = capabilities.can_reveal;
        detail.can_delete = capabilities.can_delete;
        detail.can_edit_speakers = capabilities.can_edit_speakers;
        detail.retention_label = snap.retention_label.clone().into();
        detail.transcription_runtime = snap.transcription_runtime.clone().into();
        detail.transcription_source = snap.transcription_source.clone().into();
        detail.transcription_backend = snap.transcription_backend.clone().into();
        detail.transcription_model = snap.transcription_model.clone().into();
        detail.requested_language = snap.requested_language.clone().into();
        detail.reported_language = snap.reported_language.clone().into();
        detail.reported_model = snap.reported_model.clone().into();
        app.set_detail(detail);
    }
}

/// Convert persisted stage semantics into the single Slint representation used
/// by initial selection and background updates.
pub(crate) fn stage_views_from_snapshot(
    snapshot: &crate::jobs::JobViewSnapshot,
) -> Vec<crate::StageView> {
    snapshot
        .stages
        .iter()
        .map(|stage| {
            let is_current = stage.name == snapshot.stage.name();
            crate::StageView {
                name: stage.name.clone().into(),
                label: stage.label.clone().into(),
                done: stage.state == crate::jobs::StageState::Completed,
                skipped: stage.state == crate::jobs::StageState::Skipped,
                active: is_current && snapshot.status == crate::jobs::JobStatus::Running,
                progress: if is_current {
                    snapshot.stage_progress as i32
                } else {
                    0
                },
                indeterminate: is_current
                    && snapshot.status == crate::jobs::JobStatus::Running
                    && snapshot.stage == Stage::Transcribe,
                error: is_current && snapshot.status == crate::jobs::JobStatus::Failed,
            }
        })
        .collect()
}

pub(crate) fn set_job_stage(
    app: &Weak<App>,
    job_id: Uuid,
    name: &str,
    label: &str,
    indeterminate: bool,
) {
    let id = job_id.to_string();
    let name = name.to_string();
    let label = label.to_string();
    let app = app.clone();
    app.upgrade_in_event_loop(move |app| {
        let status_name = status_name_for_stage(&name);
        let updated = update_row(&app, &id, |row| {
            row.stage_name = name.clone().into();
            row.stage_label = label.clone().into();
            row.indeterminate = indeterminate;
            row.status_label = label.clone().into();
            row.status_name = status_name.into();
            row.stage_progress = 0;
            row.total_progress = recompute_total(row.stage_name.as_str(), row.stage_progress);
        });
        if let Some(row) = updated.filter(|row| row.selected) {
            mirror_row_to_detail(&app, &row);
            mark_detail_stage_started(&app, &name, indeterminate);
        }
    })
    .ok();
}

pub(crate) fn set_job_progress(app: &Weak<App>, job_id: Uuid, percent: u8) {
    let id = job_id.to_string();
    let app = app.clone();
    app.upgrade_in_event_loop(move |app| {
        let updated = update_row(&app, &id, |row| {
            row.stage_progress = percent as i32;
            row.total_progress = recompute_total(row.stage_name.as_str(), row.stage_progress);
        });
        if let Some(row) = updated.filter(|row| row.selected) {
            mirror_row_to_detail(&app, &row);
            update_detail_stage_progress(&app, row.stage_name.as_str(), percent as i32);
        }
    })
    .ok();
}

pub(crate) fn set_job_error(app: &Weak<App>, job_id: Uuid, error: &str) {
    let id = job_id.to_string();
    let error = error.to_string();
    let app = app.clone();
    app.upgrade_in_event_loop(move |app| {
        let updated = update_row(&app, &id, |row| {
            row.has_error = true;
            row.error_text = error.clone().into();
        });
        if let Some(row) = updated.filter(|row| row.selected) {
            mirror_row_to_detail(&app, &row);
        }
    })
    .ok();
}

/// Machine-readable JobStatus key implied by a stage push. Pipeline stages run
/// under `Running`; the wait/terminal pseudo-stages carry their own status.
fn status_name_for_stage(stage_name: &str) -> &'static str {
    match stage_name {
        "needs_user_action" => "needs_user_action",
        "waiting_for_drive" => "waiting_for_drive",
        "cancelled" => "cancelled",
        "completed" => "completed",
        _ => "running",
    }
}

/// Keep the detail header of the selected job in step with live row updates.
/// `apply_snapshot` still owns the full refresh; this only mirrors the fields
/// that background stage/progress/error pushes change, so the detail badge
/// does not sit at "排队中" for the whole run.
fn mirror_row_to_detail(app: &crate::App, row: &crate::JobRow) {
    let mut detail = app.get_detail();
    if !detail.has_job {
        return;
    }
    detail.stage_name = row.stage_name.clone();
    detail.status_name = row.status_name.clone();
    detail.status_label = row.status_label.clone();
    detail.total_progress = row.total_progress;
    detail.has_error = row.has_error;
    detail.error_text = row.error_text.clone();
    app.set_detail(detail);
}

/// A new stage started: earlier pipeline stages that were active are done,
/// the named stage becomes the active one with zero progress.
fn mark_detail_stage_started(app: &crate::App, stage_name: &str, indeterminate: bool) {
    let model = app.get_stages();
    let mut seen_current = false;
    for index in 0..model.row_count() {
        let Some(mut view) = model.row_data(index) else {
            continue;
        };
        let is_current = view.name == stage_name;
        if is_current {
            seen_current = true;
            view.active = true;
            view.progress = 0;
            view.indeterminate = indeterminate;
            view.error = false;
        } else {
            if view.active && !seen_current && !view.skipped {
                view.done = true;
            }
            view.active = false;
            view.indeterminate = false;
            view.progress = 0;
        }
        model.set_row_data(index, view);
    }
}

fn update_detail_stage_progress(app: &crate::App, stage_name: &str, percent: i32) {
    let model = app.get_stages();
    for index in 0..model.row_count() {
        if let Some(mut view) = model.row_data(index) {
            if view.name == stage_name {
                view.progress = percent;
                model.set_row_data(index, view);
                break;
            }
        }
    }
}

fn update_row<F: FnOnce(&mut crate::JobRow)>(
    app: &crate::App,
    id: &str,
    update: F,
) -> Option<crate::JobRow> {
    let model = app.get_jobs();
    for index in 0..model.row_count() {
        if let Some(mut row) = model.row_data(index) {
            if row.id == id {
                update(&mut row);
                model.set_row_data(index, row.clone());
                return Some(row);
            }
        }
    }
    None
}

pub(crate) fn fmt_elapsed(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds / 60) % 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}", minutes, seconds)
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
    fn live_progress_uses_the_job_progress_algorithm() {
        for stage in crate::pipeline::stage_sequence() {
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
    fn stage_push_implies_running_except_for_wait_and_terminal_pseudo_stages() {
        for stage in crate::pipeline::stage_sequence() {
            if stage == Stage::Completed {
                continue;
            }
            assert_eq!(
                status_name_for_stage(stage.name()),
                "running",
                "{}",
                stage.name()
            );
        }
        assert_eq!(
            status_name_for_stage("needs_user_action"),
            "needs_user_action"
        );
        assert_eq!(
            status_name_for_stage("waiting_for_drive"),
            "waiting_for_drive"
        );
        assert_eq!(status_name_for_stage("completed"), "completed");
        assert_eq!(status_name_for_stage("cancelled"), "cancelled");
    }
}
