//! Platform paths, single-instance ownership, and state-directory migration.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::Config;
use crate::jobs::Queue;

const APP_NAME: &str = "BiMyScribe";
const LEGACY_APP_NAME: &str = "Bilibili Reader";
const STATE_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    home: PathBuf,
}

impl AppPaths {
    pub fn discover() -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        Ok(Self::for_home(home))
    }

    pub fn for_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn application_support_parent(&self) -> PathBuf {
        self.home.join("Library").join("Application Support")
    }

    pub fn application_support(&self) -> PathBuf {
        self.application_support_parent().join(APP_NAME)
    }

    pub fn legacy_application_support(&self) -> PathBuf {
        self.application_support_parent().join(LEGACY_APP_NAME)
    }

    pub fn config_file(&self) -> PathBuf {
        self.application_support().join("config.toml")
    }

    pub fn queue_file(&self) -> PathBuf {
        self.application_support().join("queue.json")
    }

    pub fn jobs_dir(&self) -> PathBuf {
        self.application_support().join("Jobs")
    }

    pub fn markdown_output_dir(&self) -> PathBuf {
        self.home.join("Documents").join(APP_NAME)
    }

    pub fn funasr_model_cache_dir(&self) -> PathBuf {
        self.funasr_runtime_data_dir().join("models")
    }

    pub fn funasr_cache_dir(&self) -> PathBuf {
        self.funasr_runtime_data_dir().join("cache")
    }

    pub fn funasr_runtime_data_dir(&self) -> PathBuf {
        self.home
            .join("Library")
            .join("Caches")
            .join(APP_NAME)
            .join("FunASR")
    }

    fn lock_file(&self) -> PathBuf {
        self.application_support_parent().join(".bimyscribe.lock")
    }
}

#[derive(Debug)]
pub struct InstanceLock {
    file: File,
}

impl InstanceLock {
    pub fn acquire(paths: &AppPaths) -> io::Result<Self> {
        fs::create_dir_all(paths.application_support_parent())?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(paths.lock_file())?;
        file.try_lock_exclusive().map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                io::Error::new(io::ErrorKind::WouldBlock, "BiMyScribe is already running")
            } else {
                error
            }
        })?;
        Ok(Self { file })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Serialize, Deserialize)]
struct StateMarker {
    schema_version: u32,
    kind: String,
    source: Option<String>,
    config_bytes: u64,
    queue_bytes: u64,
    queue_jobs: usize,
    complete: bool,
    #[serde(default)]
    validated: bool,
}

pub fn initialize_or_migrate(paths: &AppPaths) -> io::Result<()> {
    let target = paths.application_support();
    if target.exists() {
        return validate_existing_state(&target);
    }

    remove_stale_staging(paths)?;
    let legacy = paths.legacy_application_support();
    let (mut config, queue, kind, source) = if legacy.exists() {
        let config = Config::load_from(&legacy.join("config.toml"), paths)?;
        let queue = Queue::load_from(&legacy.join("queue.json"))?;
        (
            config,
            queue,
            "migration",
            Some(legacy.display().to_string()),
        )
    } else {
        (
            Config::for_paths(paths),
            Queue::default(),
            "initialization",
            None,
        )
    };
    config.normalize_legacy_defaults(paths);

    let staging = paths
        .application_support_parent()
        .join(format!(".{APP_NAME}.staging-{}", Uuid::new_v4()));
    fs::create_dir(&staging)?;

    let config_data = toml::to_string_pretty(&config).map_err(io::Error::other)?;
    let queue_data = serde_json::to_vec_pretty(&queue).map_err(io::Error::other)?;
    write_synced(&staging.join("config.toml"), config_data.as_bytes())?;
    write_synced(&staging.join("queue.json"), &queue_data)?;

    let marker = StateMarker {
        schema_version: STATE_SCHEMA,
        kind: kind.to_string(),
        source,
        config_bytes: config_data.len() as u64,
        queue_bytes: queue_data.len() as u64,
        queue_jobs: queue.jobs.len(),
        complete: true,
        validated: false,
    };
    let marker_data = serde_json::to_vec_pretty(&marker).map_err(io::Error::other)?;
    write_synced(&staging.join("migration.json"), &marker_data)?;
    sync_dir(&staging)?;
    fs::rename(&staging, &target)?;
    sync_dir(&paths.application_support_parent())?;
    validate_existing_state(&target)
}

fn validate_existing_state(target: &Path) -> io::Result<()> {
    let marker_path = target.join("migration.json");
    let config = target.join("config.toml");
    let queue = target.join("queue.json");
    if !marker_path.is_file() || !config.is_file() || !queue.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "incomplete BiMyScribe state directory: {}",
                target.display()
            ),
        ));
    }
    let mut marker: StateMarker =
        serde_json::from_slice(&fs::read(&marker_path)?).map_err(io::Error::other)?;
    let _: toml::Value = toml::from_str(&fs::read_to_string(&config)?).map_err(io::Error::other)?;
    let config_bytes = fs::metadata(config)?.len();
    let queue_data = fs::read(queue)?;
    let queue: Queue = serde_json::from_slice(&queue_data).map_err(io::Error::other)?;
    let invalid_marker = marker.schema_version != STATE_SCHEMA || !marker.complete;
    let invalid_summary = !marker.validated
        && (marker.config_bytes != config_bytes
            || marker.queue_bytes != queue_data.len() as u64
            || marker.queue_jobs != queue.jobs.len());
    if invalid_marker || invalid_summary {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "BiMyScribe state marker does not match persisted state",
        ));
    }
    if !marker.validated {
        marker.validated = true;
        let data = serde_json::to_vec_pretty(&marker).map_err(io::Error::other)?;
        replace_synced(&marker_path, &data)?;
        sync_dir(target)?;
        if let Some(parent) = target.parent() {
            sync_dir(parent)?;
        }
    }
    Ok(())
}

fn remove_stale_staging(paths: &AppPaths) -> io::Result<()> {
    let parent = paths.application_support_parent();
    if !parent.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".BiMyScribe.staging-")
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn replace_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "marker has no file name"))?;
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}",
        file_name.to_string_lossy(),
        Uuid::new_v4()
    ));
    write_synced(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths() -> AppPaths {
        AppPaths::for_home(
            std::env::temp_dir().join(format!("bimyscribe-paths-{}", Uuid::new_v4())),
        )
    }

    #[test]
    fn computes_platform_paths_from_supplied_home() {
        let paths = AppPaths::for_home("/tmp/person");
        assert_eq!(
            paths.application_support(),
            PathBuf::from("/tmp/person/Library/Application Support/BiMyScribe")
        );
        assert_eq!(paths.jobs_dir(), paths.application_support().join("Jobs"));
        assert_eq!(
            paths.markdown_output_dir(),
            PathBuf::from("/tmp/person/Documents/BiMyScribe")
        );
        assert_eq!(
            paths.funasr_runtime_data_dir(),
            PathBuf::from("/tmp/person/Library/Caches/BiMyScribe/FunASR")
        );
        assert_eq!(
            paths.funasr_model_cache_dir(),
            paths.funasr_runtime_data_dir().join("models")
        );
        assert_eq!(
            paths.funasr_cache_dir(),
            paths.funasr_runtime_data_dir().join("cache")
        );
    }

    #[test]
    fn initializes_complete_state_with_parent_lock() {
        let paths = temp_paths();
        let lock = InstanceLock::acquire(&paths).unwrap();
        assert!(!paths.application_support().exists());
        initialize_or_migrate(&paths).unwrap();
        assert!(paths.config_file().is_file());
        assert!(paths.queue_file().is_file());
        assert!(paths.application_support().join("migration.json").is_file());
        let marker: StateMarker = serde_json::from_slice(
            &fs::read(paths.application_support().join("migration.json")).unwrap(),
        )
        .unwrap();
        assert!(marker.complete);
        assert!(marker.validated);
        drop(lock);
        fs::remove_dir_all(paths.home).ok();
    }

    #[test]
    fn rejects_second_instance() {
        let paths = temp_paths();
        let first = InstanceLock::acquire(&paths).unwrap();
        let second = InstanceLock::acquire(&paths);
        assert_eq!(second.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        drop(first);
        fs::remove_dir_all(paths.home).ok();
    }

    #[test]
    fn migrates_legacy_config_and_queue_without_deleting_source() {
        let paths = temp_paths();
        let legacy = paths.legacy_application_support();
        fs::create_dir_all(&legacy).unwrap();
        fs::write(
            legacy.join("config.toml"),
            r#"working_dir = "/Volumes/LegacyDisk/bilibili-reader/jobs"
output_dir = ""
funasr_script = "/Users/developer/Projects/funasr-docker"
llm_enabled = false
default_screenshots = false
default_retention = "recommended"
"#,
        )
        .unwrap();
        let job = crate::jobs::Job::new(Uuid::new_v4(), "BVTEST".into(), 1);
        let custom_work_dir = paths.home.join("CustomJobs").join(job.id.to_string());
        let custom_output_dir = paths.home.join("CustomOutput");
        let mut job = job;
        job.stage = crate::jobs::Stage::Transcribe;
        job.status = crate::jobs::JobStatus::NeedsUserAction;
        job.work_dir = Some(custom_work_dir.clone());
        job.final_output_dir = Some(custom_output_dir.clone());
        let queue = Queue {
            jobs: vec![job.clone()],
        };
        fs::write(
            legacy.join("queue.json"),
            serde_json::to_vec_pretty(&queue).unwrap(),
        )
        .unwrap();

        let _lock = InstanceLock::acquire(&paths).unwrap();
        initialize_or_migrate(&paths).unwrap();

        let migrated = Config::load_from(&paths.config_file(), &paths).unwrap();
        let migrated_queue = Queue::load_from(&paths.queue_file()).unwrap();
        assert_eq!(migrated.working_dir, paths.jobs_dir());
        assert_eq!(migrated.output_dir, paths.markdown_output_dir());
        assert_eq!(migrated.runtime_project, None);
        assert_eq!(migrated_queue.jobs[0].id, job.id);
        assert_eq!(migrated_queue.jobs[0].stage, job.stage);
        assert_eq!(migrated_queue.jobs[0].status, job.status);
        assert_eq!(migrated_queue.jobs[0].work_dir, Some(custom_work_dir));
        assert_eq!(
            migrated_queue.jobs[0].final_output_dir,
            Some(custom_output_dir)
        );
        assert!(legacy.join("config.toml").is_file());
        fs::remove_dir_all(paths.home).ok();
    }

    #[test]
    fn migration_preserves_user_selected_absolute_paths() {
        let paths = temp_paths();
        let legacy = paths.legacy_application_support();
        fs::create_dir_all(&legacy).unwrap();
        let working = paths.home.join("UserJobs");
        let output = paths.home.join("UserDocuments");
        let runtime = paths.home.join("RuntimeProject");
        fs::write(
            legacy.join("config.toml"),
            format!(
                "working_dir = {:?}\noutput_dir = {:?}\nruntime_project = {:?}\n",
                working.to_string_lossy(),
                output.to_string_lossy(),
                runtime.to_string_lossy()
            ),
        )
        .unwrap();
        fs::write(legacy.join("queue.json"), r#"{"jobs":[]}"#).unwrap();

        let _lock = InstanceLock::acquire(&paths).unwrap();
        initialize_or_migrate(&paths).unwrap();
        let migrated = Config::load_from(&paths.config_file(), &paths).unwrap();
        assert_eq!(migrated.working_dir, working);
        assert_eq!(migrated.output_dir, output);
        assert_eq!(migrated.runtime_project, Some(runtime));
        fs::remove_dir_all(paths.home).ok();
    }

    #[test]
    fn removes_stale_staging_before_retrying_legacy_migration() {
        let paths = temp_paths();
        let legacy = paths.legacy_application_support();
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("config.toml"), "").unwrap();
        fs::write(legacy.join("queue.json"), r#"{"jobs":[]}"#).unwrap();
        let staging = paths
            .application_support_parent()
            .join(".BiMyScribe.staging-interrupted");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("partial"), "incomplete").unwrap();

        let _lock = InstanceLock::acquire(&paths).unwrap();
        initialize_or_migrate(&paths).unwrap();
        assert!(!staging.exists());
        assert!(paths.config_file().is_file());
        fs::remove_dir_all(paths.home).ok();
    }

    #[test]
    fn complete_new_state_wins_when_legacy_state_also_exists() {
        let paths = temp_paths();
        let _lock = InstanceLock::acquire(&paths).unwrap();
        initialize_or_migrate(&paths).unwrap();
        let original_config = fs::read(paths.config_file()).unwrap();
        let legacy = paths.legacy_application_support();
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("config.toml"), "not valid toml = [").unwrap();

        initialize_or_migrate(&paths).unwrap();
        assert_eq!(fs::read(paths.config_file()).unwrap(), original_config);
        fs::remove_dir_all(paths.home).ok();
    }

    #[test]
    fn rejects_unvalidated_marker_with_mismatched_summary() {
        let paths = temp_paths();
        let _lock = InstanceLock::acquire(&paths).unwrap();
        initialize_or_migrate(&paths).unwrap();
        let marker_path = paths.application_support().join("migration.json");
        let mut marker: StateMarker =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        marker.validated = false;
        marker.queue_jobs += 1;
        fs::write(&marker_path, serde_json::to_vec_pretty(&marker).unwrap()).unwrap();

        let error = initialize_or_migrate(&paths).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(paths.home).ok();
    }

    #[test]
    fn rejects_target_missing_config_or_queue() {
        for missing in ["config.toml", "queue.json"] {
            let paths = temp_paths();
            let _lock = InstanceLock::acquire(&paths).unwrap();
            initialize_or_migrate(&paths).unwrap();
            fs::remove_file(paths.application_support().join(missing)).unwrap();
            let error = initialize_or_migrate(&paths).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            fs::remove_dir_all(paths.home).ok();
        }
    }

    #[test]
    fn rejects_partial_target_state() {
        let paths = temp_paths();
        fs::create_dir_all(paths.application_support()).unwrap();
        fs::write(paths.config_file(), "").unwrap();
        let error = initialize_or_migrate(&paths).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(paths.home).ok();
    }
}
