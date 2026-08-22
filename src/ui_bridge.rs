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

pub(crate) fn set_job_progress(app: &Weak<App>, job_id: Uuid, percent: u8) {
    let id = job_id.to_string();
    let app = app.clone();
    app.upgrade_in_event_loop(move |app| {
        update_row(&app, &id, |row| {
            row.stage_progress = percent as i32;
            row.total_progress = recompute_total(row.stage_name.as_str(), row.stage_progress);
        });
    })
    .ok();
}

pub(crate) fn set_job_error(app: &Weak<App>, job_id: Uuid, error: &str) {
    let id = job_id.to_string();
    let error = error.to_string();
    let app = app.clone();
    app.upgrade_in_event_loop(move |app| {
        update_row(&app, &id, |row| {
            row.has_error = true;
            row.error_text = error.clone().into();
        });
    })
    .ok();
}

fn update_row<F: FnOnce(&mut crate::JobRow)>(app: &crate::App, id: &str, update: F) {
    let model = app.get_jobs();
    for index in 0..model.row_count() {
        if let Some(mut row) = model.row_data(index) {
            if row.id == id {
                update(&mut row);
                model.set_row_data(index, row);
                break;
            }
        }
    }
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
}
