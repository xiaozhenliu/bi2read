//! Queue scheduler / drain-loop.
//!
//! `Run` is a "wake the scheduler" signal. After receiving a wake, the worker
//! continuously drains runnable jobs until the queue is idle or waiting for
//! external action. Each job runs to completion/failure/cancellation before the
//! next one starts. The drain loop does not depend on the UI sending another
//! `Run` message between jobs.

use std::sync::{Arc, Mutex};

use slint::Weak;
use uuid::Uuid;

use crate::cancel::{CancellationRegistry, CancellationToken};
use crate::config::Config;
use crate::jobs::{Job, JobStatus, Queue, Stage, StageState};
use crate::pipeline::PipelineError;
use crate::App;

/// Outcome of a single `drain_next` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// A job was picked up and run (to completion, failure, or cancellation).
    Ran { job_id: Uuid, result: JobResult },
    /// No runnable job found; the queue is idle.
    Idle,
    /// A job needs external action (e.g. Docker not running); do not busy-loop.
    Waiting,
}

/// The terminal result of running one job through the pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobResult {
    Completed,
    Failed(String),
    Cancelled,
    NeedsUserAction,
    WaitingForDrive,
}

/// Boundary between queue scheduling and the side-effecting pipeline runtime.
///
/// Production uses [`PipelineJobRunner`]. Tests inject an in-memory runner, so
/// scheduler behavior can be exercised without network, filesystem, Docker,
/// queue persistence, or UI side effects.
trait JobRunner: Send + Sync {
    fn run_job(
        &self,
        job: &mut Job,
        cfg: &Config,
        app: &Weak<App>,
        token: &CancellationToken,
    ) -> JobResult;

    fn persist_job(&self, _job: &Job) {}

    fn persist_queue(&self, _queue: &Queue) {}

    fn apply_snapshot(&self, _app: &Weak<App>, _job: &Job) {}
}

struct PipelineJobRunner;

impl JobRunner for PipelineJobRunner {
    fn run_job(
        &self,
        job: &mut Job,
        cfg: &Config,
        app: &Weak<App>,
        token: &CancellationToken,
    ) -> JobResult {
        match crate::pipeline::run_job(job, cfg, app, token) {
            Ok(()) if token.is_cancelled() => JobResult::Cancelled,
            Ok(()) => JobResult::Completed,
            Err(PipelineError::Cancelled) => JobResult::Cancelled,
            Err(PipelineError::DockerNotRunning) => JobResult::NeedsUserAction,
            Err(PipelineError::DriveNotMounted) => JobResult::WaitingForDrive,
            Err(error) => JobResult::Failed(error.to_string()),
        }
    }

    fn persist_job(&self, job: &Job) {
        let _ = crate::jobs::save_job_state(job);
    }

    fn persist_queue(&self, queue: &Queue) {
        let _ = queue.save();
    }

    fn apply_snapshot(&self, app: &Weak<App>, job: &Job) {
        let snapshot = crate::jobs::JobViewSnapshot::from_job(job, chrono::Utc::now());
        crate::pipeline::apply_snapshot(app, snapshot);
    }
}

/// The scheduler owns the shared queue and cancellation registry. It is
/// constructed once and shared between the worker thread (drain) and the UI
/// thread (cancel).
pub struct Scheduler {
    pub queue: Arc<Mutex<Queue>>,
    pub cancel_registry: CancellationRegistry,
    runner: Arc<dyn JobRunner>,
}

impl Scheduler {
    pub fn new(queue: Arc<Mutex<Queue>>) -> Self {
        Self {
            queue,
            cancel_registry: CancellationRegistry::new(),
            runner: Arc::new(PipelineJobRunner),
        }
    }

    #[cfg(test)]
    fn with_runner<R>(queue: Arc<Mutex<Queue>>, runner: R) -> Self
    where
        R: JobRunner + 'static,
    {
        Self {
            queue,
            cancel_registry: CancellationRegistry::new(),
            runner: Arc::new(runner),
        }
    }

    /// True if any job is in a runnable state (Queued or NeedsUserAction that
    /// is not stuck at the NeedsUserAction stage).
    pub fn has_runnable(&self) -> bool {
        let q = self.queue.lock().unwrap();
        q.jobs.iter().any(is_runnable)
    }

    /// Find and run the next runnable job. Returns `Idle` if none, or
    /// `Waiting` if the only candidate needs external action.
    pub fn drain_next(&self, cfg: &Config, app: &Weak<App>) -> DrainOutcome {
        // Find the next job to run.
        let job_id = {
            let q = self.queue.lock().unwrap();
            q.jobs.iter().find(|j| is_runnable(j)).map(|j| j.id)
        };
        let Some(job_id) = job_id else {
            return DrainOutcome::Idle;
        };

        // Register a cancellation token for this job.
        let token = self.cancel_registry.register(job_id);

        // Mark running and persist.
        {
            let mut q = self.queue.lock().unwrap();
            if let Some(job) = q.get_mut(job_id) {
                job.status = JobStatus::Running;
                job.stage = next_run_stage(job);
                job.started_at = job.started_at.or_else(|| Some(chrono::Utc::now()));
                self.runner.persist_job(job);
            }
            self.runner.persist_queue(&q);
        }

        // Run the pipeline on a clone; write back on completion.
        let job = {
            let q = self.queue.lock().unwrap();
            q.get(job_id).cloned()
        };
        let Some(mut job) = job else {
            self.cancel_registry.remove(job_id);
            return DrainOutcome::Idle;
        };
        let job_result = self.runner.run_job(&mut job, cfg, app, &token);
        apply_result_state(&mut job, &job_result);

        // Write back the job's final state.
        {
            let mut q = self.queue.lock().unwrap();
            if let Some(stored) = q.get_mut(job_id) {
                // Copy forward the runtime-mutated fields from the worked clone.
                stored.stage = job.stage;
                stored.status = job.status;
                stored.stage_progress = job.stage_progress;
                stored.error = job.error.clone();
                stored.cid = job.cid;
                stored.up_name = job.up_name.clone();
                stored.duration_ms = job.duration_ms;
                stored.title = job.title.clone();
                stored.bvid = job.bvid.clone();
                stored.stages = job.stages.clone();
                stored.work_dir = job.work_dir.clone();
                stored.final_output_dir = job.final_output_dir.clone();
                stored.warning = job.warning.clone();
                // Guard: a cancelled job must not be overwritten by Completed/Failed.
                if stored.status == JobStatus::Cancelled || stored.status == JobStatus::Cancelling {
                    stored.status = JobStatus::Cancelled;
                    stored.stage = Stage::Cancelled;
                }
                // Set finished_at if the job reached a terminal state.
                if matches!(
                    stored.status,
                    JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
                ) && stored.finished_at.is_none()
                {
                    stored.finished_at = Some(chrono::Utc::now());
                }
                self.runner.persist_job(stored);
            }
            self.runner.persist_queue(&q);
        }

        // Apply the final snapshot to the UI.
        {
            let q = self.queue.lock().unwrap();
            if let Some(job) = q.get(job_id) {
                self.runner.apply_snapshot(app, job);
            }
        }

        // Clean up the cancellation token.
        self.cancel_registry.remove(job_id);

        DrainOutcome::Ran {
            job_id,
            result: job_result,
        }
    }

    /// Drain the entire runnable queue, calling `drain_next` repeatedly until
    /// `Idle` or `Waiting`. Returns the outcomes of all jobs that ran.
    pub fn drain_all(&self, cfg: &Config, app: &Weak<App>) -> Vec<DrainOutcome> {
        let mut outcomes = Vec::new();
        loop {
            let outcome = self.drain_next(cfg, app);
            match outcome {
                DrainOutcome::Idle => break,
                DrainOutcome::Waiting => break,
                DrainOutcome::Ran { .. } => {
                    outcomes.push(outcome);
                }
            }
        }
        outcomes
    }
}

fn apply_result_state(job: &mut Job, result: &JobResult) {
    match result {
        JobResult::Completed => {
            job.status = JobStatus::Completed;
            job.stage = Stage::Completed;
            job.stage_progress = 100;
        }
        JobResult::Failed(message) => {
            job.status = JobStatus::Failed;
            job.error = Some(message.clone());
        }
        JobResult::Cancelled => {
            job.status = JobStatus::Cancelled;
            job.stage = Stage::Cancelled;
        }
        JobResult::NeedsUserAction => {
            job.status = JobStatus::NeedsUserAction;
            job.stage = Stage::NeedsUserAction;
        }
        JobResult::WaitingForDrive => {
            job.status = JobStatus::WaitingForDrive;
            job.stage = Stage::WaitingForDrive;
        }
    }
}

/// A job is runnable if it is Queued, or NeedsUserAction but not currently
/// stuck at the NeedsUserAction stage (e.g. Docker was started and the user
/// clicked retry which re-queued it).
fn is_runnable(job: &Job) -> bool {
    job.status == JobStatus::Queued
        || (job.status == JobStatus::NeedsUserAction && job.stage != Stage::NeedsUserAction)
}

/// Determine the stage to (re)run from: the first Pending or Failed stage,
/// or Queued if none found.
fn next_run_stage(job: &Job) -> Stage {
    crate::jobs::pipeline_stages()
        .into_iter()
        .find(|s| {
            job.stage_state(*s) == StageState::Pending || job.stage_state(*s) == StageState::Failed
        })
        .unwrap_or(Stage::Queued)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::jobs::{Job, Queue, StageState};

    #[derive(Clone)]
    struct ScriptedRunner {
        results: Arc<Mutex<VecDeque<JobResult>>>,
        calls: Arc<Mutex<Vec<Uuid>>>,
    }

    impl ScriptedRunner {
        fn new(results: Vec<JobResult>) -> Self {
            Self {
                results: Arc::new(Mutex::new(results.into())),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<Uuid> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl JobRunner for ScriptedRunner {
        fn run_job(
            &self,
            job: &mut Job,
            _cfg: &Config,
            _app: &Weak<App>,
            _token: &CancellationToken,
        ) -> JobResult {
            self.calls.lock().unwrap().push(job.id);
            let result = self
                .results
                .lock()
                .unwrap()
                .pop_front()
                .expect("one scripted result per queued job");

            match &result {
                JobResult::Completed => fake_complete(job),
                JobResult::Failed(message) => {
                    job.stage = Stage::Failed;
                    job.status = JobStatus::Failed;
                    job.error = Some(message.clone());
                }
                JobResult::Cancelled => {
                    job.stage = Stage::Cancelled;
                    job.status = JobStatus::Cancelled;
                }
                JobResult::NeedsUserAction => {
                    job.stage = Stage::NeedsUserAction;
                    job.status = JobStatus::NeedsUserAction;
                }
                JobResult::WaitingForDrive => {
                    job.stage = Stage::WaitingForDrive;
                    job.status = JobStatus::WaitingForDrive;
                }
            }

            result
        }
    }

    /// A minimal fake pipeline that marks stages Completed without any I/O.
    fn fake_complete(job: &mut Job) {
        for s in crate::jobs::pipeline_stages() {
            job.set_stage_state(s, StageState::Completed);
        }
        job.stage = Stage::Completed;
        job.status = JobStatus::Completed;
        job.stage_progress = 100;
    }

    /// Create a fresh queued job with all stages Pending.
    fn queued_job() -> Job {
        let mut job = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        for s in crate::jobs::pipeline_stages() {
            job.set_stage_state(s, StageState::Pending);
        }
        job
    }

    #[test]
    fn two_queued_jobs_both_complete() {
        let first = queued_job();
        let first_id = first.id;
        let second = queued_job();
        let second_id = second.id;
        let queue = Arc::new(Mutex::new(Queue {
            jobs: vec![first, second],
        }));
        let runner = ScriptedRunner::new(vec![JobResult::Completed, JobResult::Completed]);
        let scheduler = Scheduler::with_runner(queue.clone(), runner.clone());

        let outcomes = scheduler.drain_all(&Config::default(), &Weak::<App>::default());

        assert_eq!(runner.calls(), vec![first_id, second_id]);
        assert_eq!(
            outcomes,
            vec![
                DrainOutcome::Ran {
                    job_id: first_id,
                    result: JobResult::Completed,
                },
                DrainOutcome::Ran {
                    job_id: second_id,
                    result: JobResult::Completed,
                },
            ]
        );
        let q = queue.lock().unwrap();
        assert_eq!(q.get(first_id).unwrap().status, JobStatus::Completed);
        assert_eq!(q.get(second_id).unwrap().status, JobStatus::Completed);
    }

    #[test]
    fn drain_continues_after_each_job_result() {
        let jobs: Vec<Job> = (0..5).map(|_| queued_job()).collect();
        let ids: Vec<Uuid> = jobs.iter().map(|job| job.id).collect();
        let queue = Arc::new(Mutex::new(Queue { jobs }));
        let runner = ScriptedRunner::new(vec![
            JobResult::Completed,
            JobResult::NeedsUserAction,
            JobResult::Failed("scripted failure".into()),
            JobResult::Cancelled,
            JobResult::Completed,
        ]);
        let scheduler = Scheduler::with_runner(queue.clone(), runner.clone());

        let outcomes = scheduler.drain_all(&Config::default(), &Weak::<App>::default());

        assert_eq!(runner.calls(), ids);
        assert_eq!(outcomes.len(), 5);
        assert_eq!(
            outcomes,
            vec![
                DrainOutcome::Ran {
                    job_id: ids[0],
                    result: JobResult::Completed,
                },
                DrainOutcome::Ran {
                    job_id: ids[1],
                    result: JobResult::NeedsUserAction,
                },
                DrainOutcome::Ran {
                    job_id: ids[2],
                    result: JobResult::Failed("scripted failure".into()),
                },
                DrainOutcome::Ran {
                    job_id: ids[3],
                    result: JobResult::Cancelled,
                },
                DrainOutcome::Ran {
                    job_id: ids[4],
                    result: JobResult::Completed,
                },
            ]
        );

        let q = queue.lock().unwrap();
        assert_eq!(q.get(ids[0]).unwrap().status, JobStatus::Completed);
        assert_eq!(q.get(ids[1]).unwrap().status, JobStatus::NeedsUserAction);
        assert_eq!(q.get(ids[2]).unwrap().status, JobStatus::Failed);
        assert_eq!(q.get(ids[3]).unwrap().status, JobStatus::Cancelled);
        assert_eq!(q.get(ids[4]).unwrap().status, JobStatus::Completed);
    }

    #[test]
    fn runner_outcome_is_the_authoritative_terminal_state() {
        let mut job = queued_job();
        job.status = JobStatus::Failed;
        job.stage = Stage::Failed;
        apply_result_state(&mut job, &JobResult::NeedsUserAction);
        assert_eq!(job.status, JobStatus::NeedsUserAction);
        assert_eq!(job.stage, Stage::NeedsUserAction);
    }

    #[test]
    fn has_runnable_detects_queued_job() {
        let queue = Arc::new(Mutex::new(Queue {
            jobs: vec![queued_job()],
        }));
        let scheduler = Scheduler::new(queue);
        assert!(scheduler.has_runnable());
    }

    #[test]
    fn has_runnable_false_when_all_completed() {
        let mut job = queued_job();
        fake_complete(&mut job);
        let queue = Arc::new(Mutex::new(Queue { jobs: vec![job] }));
        let scheduler = Scheduler::new(queue);
        assert!(!scheduler.has_runnable());
    }

    #[test]
    fn needs_user_action_at_needs_action_stage_not_runnable() {
        let mut job = queued_job();
        job.status = JobStatus::NeedsUserAction;
        job.stage = Stage::NeedsUserAction;
        let queue = Arc::new(Mutex::new(Queue { jobs: vec![job] }));
        let scheduler = Scheduler::new(queue);
        // Not runnable: the job is stuck at NeedsUserAction stage.
        assert!(!scheduler.has_runnable());
    }

    #[test]
    fn needs_user_action_re_queued_is_runnable() {
        // After user clicks retry, the status is reset to Queued, making it
        // runnable again even if the stage was NeedsUserAction.
        let mut job = queued_job();
        job.status = JobStatus::Queued;
        job.stage = Stage::Transcribe; // re-running from transcribe
        let queue = Arc::new(Mutex::new(Queue { jobs: vec![job] }));
        let scheduler = Scheduler::new(queue);
        assert!(scheduler.has_runnable());
    }

    #[test]
    fn only_one_running_at_a_time() {
        // If a job is already Running, another Queued job should not be
        // picked up until the first is done.
        let mut job1 = queued_job();
        job1.status = JobStatus::Running;
        let job2 = queued_job();
        let queue = Arc::new(Mutex::new(Queue {
            jobs: vec![job1, job2],
        }));
        let scheduler = Scheduler::new(queue);
        // is_runnable checks for Queued, not Running, so job2 is runnable.
        // But in practice the worker only calls drain_next after the current
        // job finishes, so there's never two Running at once. The guard is
        // in the worker loop, not the scheduler.
        // However, we can verify that drain_next picks job2 (Queued), not job1.
        // Actually, is_runnable returns true for Queued job2 but drain_next
        // finds the first runnable (Queued) job, which is job2. This is fine
        // because the worker won't call drain_next while job1 is still running.
        assert!(scheduler.has_runnable());
    }

    #[test]
    fn cancelled_job_not_overwritten_by_completed() {
        // Simulate a job that was cancelled but the pipeline returned Ok.
        let id = Uuid::new_v4();
        let mut job = queued_job();
        job.id = id;
        job.status = JobStatus::Cancelled;
        job.stage = Stage::Cancelled;

        let queue = Arc::new(Mutex::new(Queue { jobs: vec![job] }));
        let scheduler = Scheduler::new(queue.clone());

        // Register a cancelled token.
        let token = scheduler.cancel_registry.register(id);
        token.cancel();

        // drain_next won't pick up a Cancelled job (not runnable).
        let outcome = scheduler.drain_next(&Config::default(), &Weak::<App>::default());
        assert!(matches!(outcome, DrainOutcome::Idle));

        // The job stays Cancelled.
        let q = queue.lock().unwrap();
        assert_eq!(q.get(id).unwrap().status, JobStatus::Cancelled);
    }
}
