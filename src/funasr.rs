//! Versioned BiMyScribe FunASR Runtime adapter.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const RUNTIME_MANIFEST: &str = "bimyscribe-runtime.toml";
pub const SUPPORTED_CONTRACT_VERSION: u32 = 1;
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Utterance {
    pub id: String,
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker_id: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeManifest {
    pub contract_version: u32,
    pub backend: RuntimeBackend,
    pub entrypoint: Option<PathBuf>,
    pub compose_file: Option<PathBuf>,
    pub transcribe_service: Option<String>,
    pub probe_service: Option<String>,
    pub output_schema_version: u32,
    pub output_schema_file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeBackend {
    NativeUv,
    DockerCompose,
}

impl RuntimeBackend {
    pub fn label(self) -> &'static str {
        match self {
            Self::NativeUv => "原生 uv",
            Self::DockerCompose => "Docker Compose",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeReady {
    contract_version: u32,
    backend: String,
    project_dir: String,
    fingerprint: u64,
    pub device: String,
}

pub fn runtime_status(project_dir: &Path, runtime_data: &Path) -> String {
    let manifest = match load_runtime(project_dir) {
        Ok(manifest) => manifest,
        Err(error) => return format!("配置无效：{error}"),
    };
    match matching_ready_record(project_dir, runtime_data, &manifest) {
        Some(ready) => {
            format!("{} · 已就绪 · {}", manifest.backend.label(), ready.device)
        }
        _ => format!("{} · 未安装或需要重新验证", manifest.backend.label()),
    }
}

pub fn runtime_is_ready(project_dir: &Path, runtime_data: &Path) -> bool {
    load_runtime(project_dir)
        .ok()
        .and_then(|manifest| matching_ready_record(project_dir, runtime_data, &manifest))
        .is_some()
}

fn matching_ready_record(
    project_dir: &Path,
    runtime_data: &Path,
    manifest: &RuntimeManifest,
) -> Option<RuntimeReady> {
    let ready = std::fs::read(runtime_data.join("runtime-ready.json"))
        .ok()
        .and_then(|data| serde_json::from_slice::<RuntimeReady>(&data).ok())?;
    let canonical = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let matches = ready.contract_version == manifest.contract_version
        && ready.backend == manifest.backend.label()
        && ready.project_dir == canonical.to_string_lossy()
        && ready.fingerprint == runtime_fingerprint(project_dir, manifest).ok()?;
    matches.then_some(ready)
}

fn runtime_fingerprint(project_dir: &Path, manifest: &RuntimeManifest) -> Result<u64, FunasrError> {
    let mut hasher = DefaultHasher::new();
    for relative in [
        Some(PathBuf::from(RUNTIME_MANIFEST)),
        Some(manifest.output_schema_file.clone()),
        manifest.entrypoint.clone(),
        manifest.compose_file.clone(),
        project_dir
            .join("transcribe.py")
            .is_file()
            .then(|| PathBuf::from("transcribe.py")),
        project_dir
            .join("uv.lock")
            .is_file()
            .then(|| PathBuf::from("uv.lock")),
    ]
    .into_iter()
    .flatten()
    {
        relative.hash(&mut hasher);
        std::fs::read(project_dir.join(relative))
            .map_err(FunasrError::Read)?
            .hash(&mut hasher);
    }
    Ok(hasher.finish())
}

pub fn install_runtime(
    project_dir: &Path,
    runtime_data: &Path,
) -> Result<RuntimeReady, FunasrError> {
    let manifest = load_runtime(project_dir)?;
    std::fs::create_dir_all(runtime_data).map_err(FunasrError::Read)?;
    let log_path = runtime_data.join("install.log");
    let device = match manifest.backend {
        RuntimeBackend::NativeUv => {
            install_native_uv(project_dir, runtime_data, &manifest, &log_path)?
        }
        RuntimeBackend::DockerCompose => {
            validate_runtime(
                project_dir,
                &runtime_data.join("models"),
                &runtime_data.join("cache"),
            )?;
            "Docker CPU".to_string()
        }
    };
    let canonical = project_dir.canonicalize().map_err(FunasrError::Read)?;
    let ready = RuntimeReady {
        contract_version: manifest.contract_version,
        backend: manifest.backend.label().to_string(),
        project_dir: canonical.to_string_lossy().into_owned(),
        fingerprint: runtime_fingerprint(project_dir, &manifest)?,
        device,
    };
    let data = serde_json::to_vec_pretty(&ready).map_err(FunasrError::Parse)?;
    crate::jobs::atomic_write(&runtime_data.join("runtime-ready.json"), &data)
        .map_err(|error| FunasrError::Io(error.to_string()))?;
    Ok(ready)
}

fn install_native_uv(
    project_dir: &Path,
    runtime_data: &Path,
    manifest: &RuntimeManifest,
    log_path: &Path,
) -> Result<String, FunasrError> {
    prepare_native_data_dirs(runtime_data)?;

    let version = native_uv_environment(
        crate::process::SubprocessSpec::new(vec!["uv".into(), "--version".into()]),
        runtime_data,
    )
    .log(log_path);
    let exit =
        crate::process::run(version).map_err(|error| FunasrError::Process(error.to_string()))?;
    if exit != 0 {
        return Err(FunasrError::Process(format!("uv 版本检查退出码 {exit}")));
    }

    let sync = native_uv_environment(
        crate::process::SubprocessSpec::new(vec![
            "uv".into(),
            "sync".into(),
            "--project".into(),
            project_dir.to_string_lossy().into_owned(),
            "--frozen".into(),
            "--no-dev".into(),
        ]),
        runtime_data,
    )
    .cwd(project_dir)
    .log(log_path);
    let exit =
        crate::process::run(sync).map_err(|error| FunasrError::Process(error.to_string()))?;
    if exit != 0 {
        return Err(FunasrError::Process(format!("uv sync 退出码 {exit}")));
    }

    let self_check = runtime_data.join("self-check.json");
    let entrypoint = manifest
        .entrypoint
        .as_ref()
        .expect("validated native entrypoint");
    let check = native_uv_environment(
        crate::process::SubprocessSpec::new(vec![
            "uv".into(),
            "run".into(),
            "--project".into(),
            project_dir.to_string_lossy().into_owned(),
            "--frozen".into(),
            "--no-sync".into(),
            "python".into(),
            entrypoint.to_string_lossy().into_owned(),
            "--self-check".into(),
            self_check.to_string_lossy().into_owned(),
            "--device".into(),
            "auto".into(),
        ]),
        runtime_data,
    )
    .cwd(project_dir)
    .log(log_path);
    let exit =
        crate::process::run(check).map_err(|error| FunasrError::Process(error.to_string()))?;
    if exit != 0 {
        return Err(FunasrError::Process(format!("Runtime 自检退出码 {exit}")));
    }
    #[derive(Deserialize)]
    struct SelfCheck {
        device: String,
    }
    let check: SelfCheck =
        serde_json::from_slice(&std::fs::read(&self_check).map_err(FunasrError::Read)?)
            .map_err(FunasrError::Parse)?;
    let _ = std::fs::remove_file(self_check);
    if !matches!(check.device.as_str(), "mps" | "cpu") {
        return Err(FunasrError::Validation(format!(
            "Runtime 返回未知设备：{}",
            check.device
        )));
    }
    Ok(check.device)
}

fn prepare_native_data_dirs(runtime_data: &Path) -> Result<(), FunasrError> {
    for directory in [
        "python-environment",
        "python-installations",
        "uv-cache",
        "models",
        "huggingface",
        "torch",
        "cache",
        "tmp",
    ] {
        std::fs::create_dir_all(runtime_data.join(directory)).map_err(FunasrError::Read)?;
    }
    Ok(())
}

fn native_uv_environment(
    spec: crate::process::SubprocessSpec,
    runtime_data: &Path,
) -> crate::process::SubprocessSpec {
    spec.env(
        "UV_PROJECT_ENVIRONMENT",
        runtime_data.join("python-environment").as_os_str(),
    )
    .env("UV_CACHE_DIR", runtime_data.join("uv-cache").as_os_str())
    .env("UV_NO_CONFIG", "1")
    .env("UV_PYTHON_PREFERENCE", "only-managed")
    .env(
        "UV_PYTHON_INSTALL_DIR",
        runtime_data.join("python-installations").as_os_str(),
    )
    .env("MODELSCOPE_CACHE", runtime_data.join("models").as_os_str())
    .env("HF_HOME", runtime_data.join("huggingface").as_os_str())
    .env("TORCH_HOME", runtime_data.join("torch").as_os_str())
    .env("TMPDIR", runtime_data.join("tmp").as_os_str())
    .env(
        "BIMYSCRIBE_FUNASR_CACHE_DIR",
        runtime_data.join("cache").as_os_str(),
    )
}

#[derive(Debug, Deserialize)]
struct NormalizedTranscript {
    schema_version: u32,
    segments: Vec<NormalizedSegment>,
}

#[derive(Debug, Deserialize)]
struct NormalizedSegment {
    text: String,
    start_ms: u64,
    end_ms: u64,
    speaker: serde_json::Value,
}

pub fn load_runtime(project_dir: &Path) -> Result<RuntimeManifest, FunasrError> {
    if !project_dir.is_absolute() || !project_dir.is_dir() {
        return Err(FunasrError::Validation(format!(
            "Runtime 目录无效：{}",
            project_dir.display()
        )));
    }
    let data =
        std::fs::read_to_string(project_dir.join(RUNTIME_MANIFEST)).map_err(FunasrError::Read)?;
    let manifest: RuntimeManifest =
        toml::from_str(&data).map_err(|error| FunasrError::Manifest(error.to_string()))?;
    if manifest.contract_version != SUPPORTED_CONTRACT_VERSION {
        return Err(FunasrError::Validation(format!(
            "不支持 Runtime contract_version {}",
            manifest.contract_version
        )));
    }
    if manifest.output_schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(FunasrError::Validation(format!(
            "不支持输出 schema_version {}",
            manifest.output_schema_version
        )));
    }
    let mut required_files = vec![&manifest.output_schema_file];
    match manifest.backend {
        RuntimeBackend::NativeUv => {
            let entrypoint = manifest.entrypoint.as_ref().ok_or_else(|| {
                FunasrError::Validation("native-uv Runtime 缺少 entrypoint".into())
            })?;
            required_files.push(entrypoint);
            for required in ["pyproject.toml", ".python-version", "uv.lock"] {
                if !project_dir.join(required).is_file() {
                    return Err(FunasrError::Validation(format!(
                        "native-uv Runtime 文件不存在：{required}"
                    )));
                }
            }
        }
        RuntimeBackend::DockerCompose => {
            let compose = manifest.compose_file.as_ref().ok_or_else(|| {
                FunasrError::Validation("docker-compose Runtime 缺少 compose_file".into())
            })?;
            required_files.push(compose);
            if manifest
                .transcribe_service
                .as_deref()
                .is_none_or(str::is_empty)
                || manifest.probe_service.as_deref().is_none_or(str::is_empty)
            {
                return Err(FunasrError::Validation(
                    "Docker Runtime 服务名不能为空".into(),
                ));
            }
        }
    }
    for relative in required_files {
        validate_runtime_relative_path(relative)?;
        if !project_dir.join(relative).is_file() {
            return Err(FunasrError::Validation(format!(
                "Runtime 文件不存在：{}",
                relative.display()
            )));
        }
    }
    Ok(manifest)
}

fn validate_runtime_relative_path(path: &Path) -> Result<(), FunasrError> {
    if path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(FunasrError::Validation(format!(
            "Runtime manifest 包含越界路径：{}",
            path.display()
        )));
    }
    Ok(())
}

pub fn docker_is_running() -> bool {
    crate::process::run(crate::process::SubprocessSpec::new(vec![
        "docker".into(),
        "ps".into(),
    ]))
    .map(|code| code == 0)
    .unwrap_or(false)
}

pub fn validate_runtime(
    project_dir: &Path,
    model_cache: &Path,
    runtime_cache: &Path,
) -> Result<(), FunasrError> {
    let manifest = load_runtime(project_dir)?;
    if manifest.backend != RuntimeBackend::DockerCompose {
        return Err(FunasrError::Validation(
            "原生 Runtime 必须通过冻结环境自检，不使用 Docker 探针".into(),
        ));
    }
    let probe_root = runtime_cache.join(format!("probe-{}", uuid::Uuid::new_v4()));
    let input = probe_root.join("input");
    let output = probe_root.join("output");
    std::fs::create_dir_all(&input).map_err(FunasrError::Read)?;
    std::fs::create_dir_all(&output).map_err(FunasrError::Read)?;
    std::fs::create_dir_all(model_cache).map_err(FunasrError::Read)?;
    std::fs::create_dir_all(runtime_cache).map_err(FunasrError::Read)?;
    std::fs::write(input.join("sentinel"), b"bimyscribe-runtime-probe")
        .map_err(FunasrError::Read)?;

    let config_exit = crate::process::run(compose_spec(
        project_dir,
        &manifest,
        &input,
        &output,
        model_cache,
        runtime_cache,
        vec!["config".into()],
    ))
    .map_err(|error| FunasrError::Docker(error.to_string()))?;
    if config_exit != 0 {
        cleanup_probe(&probe_root);
        return Err(FunasrError::Docker("Docker Compose 配置解析失败".into()));
    }

    let probe_exit = crate::process::run(compose_spec(
        project_dir,
        &manifest,
        &input,
        &output,
        model_cache,
        runtime_cache,
        vec![
            "run".into(),
            "--rm".into(),
            manifest
                .probe_service
                .clone()
                .expect("validated probe service"),
        ],
    ))
    .map_err(|error| FunasrError::Docker(error.to_string()))?;
    cleanup_probe(&probe_root);
    if probe_exit != 0 {
        return Err(FunasrError::Docker("Runtime 四挂载探针失败".into()));
    }
    Ok(())
}

fn cleanup_probe(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

fn compose_spec(
    project_dir: &Path,
    manifest: &RuntimeManifest,
    input: &Path,
    output: &Path,
    model_cache: &Path,
    runtime_cache: &Path,
    tail: Vec<String>,
) -> crate::process::SubprocessSpec {
    let mut argv = vec![
        "docker".into(),
        "compose".into(),
        "-f".into(),
        manifest
            .compose_file
            .as_ref()
            .expect("validated compose file")
            .to_string_lossy()
            .into_owned(),
    ];
    argv.extend(tail);
    crate::process::SubprocessSpec::new(argv)
        .cwd(project_dir)
        .env("BIMYSCRIBE_FUNASR_INPUT_DIR", input.as_os_str())
        .env("BIMYSCRIBE_FUNASR_OUTPUT_DIR", output.as_os_str())
        .env("BIMYSCRIBE_FUNASR_MODEL_CACHE_DIR", model_cache.as_os_str())
        .env("BIMYSCRIBE_FUNASR_CACHE_DIR", runtime_cache.as_os_str())
}

fn native_uv_spec(
    project_dir: &Path,
    manifest: &RuntimeManifest,
    input: &Path,
    output: &Path,
    runtime_data: &Path,
) -> crate::process::SubprocessSpec {
    let entrypoint = manifest
        .entrypoint
        .as_ref()
        .expect("validated native entrypoint");
    native_uv_environment(
        crate::process::SubprocessSpec::new(vec![
            "uv".into(),
            "run".into(),
            "--project".into(),
            project_dir.to_string_lossy().into_owned(),
            "--frozen".into(),
            "--no-sync".into(),
            "python".into(),
            entrypoint.to_string_lossy().into_owned(),
            input.to_string_lossy().into_owned(),
            "--out".into(),
            output.to_string_lossy().into_owned(),
            "--device".into(),
            "auto".into(),
        ]),
        runtime_data,
    )
    .cwd(project_dir)
}

pub fn container_name(job_id: &uuid::Uuid, instance_nonce: &str) -> String {
    let nonce = &instance_nonce[..instance_nonce.len().min(12)];
    format!("bimyscribe-{nonce}-{job_id}")
}

pub struct TranscribeRequest<'a> {
    pub job_dir: &'a Path,
    pub normalized_wav: &'a Path,
    pub project_dir: &'a Path,
    pub runtime_data_dir: &'a Path,
    pub log_path: &'a Path,
    pub job_id: &'a uuid::Uuid,
    pub instance_nonce: &'a str,
    pub cancel_token: &'a crate::cancel::CancellationToken,
}

pub fn run(request: TranscribeRequest<'_>) -> Result<Vec<Utterance>, FunasrError> {
    let manifest = load_runtime(request.project_dir)?;
    if matching_ready_record(request.project_dir, request.runtime_data_dir, &manifest).is_none() {
        return Err(FunasrError::Validation(
            "Runtime 尚未安装、验证记录已失效或项目内容已改变".into(),
        ));
    }
    let funasr_dir = request.job_dir.join("funasr");
    let input_dir = funasr_dir.join("input");
    let output_dir = funasr_dir.join("output");
    std::fs::create_dir_all(&input_dir).map_err(FunasrError::Read)?;
    std::fs::create_dir_all(&output_dir).map_err(FunasrError::Read)?;
    let model_cache = request.runtime_data_dir.join("models");
    let runtime_cache = request.runtime_data_dir.join("cache");
    std::fs::create_dir_all(&model_cache).map_err(FunasrError::Read)?;
    std::fs::create_dir_all(&runtime_cache).map_err(FunasrError::Read)?;
    let input_copy = input_dir.join("normalized.wav");
    std::fs::copy(request.normalized_wav, &input_copy).map_err(FunasrError::Read)?;

    if request.cancel_token.is_cancelled() {
        let _ = std::fs::remove_file(&input_copy);
        return Err(FunasrError::Cancelled);
    }

    let (spec, owned_container) = match manifest.backend {
        RuntimeBackend::NativeUv => (
            native_uv_spec(
                request.project_dir,
                &manifest,
                &input_copy,
                &output_dir,
                request.runtime_data_dir,
            )
            .log(request.log_path),
            None,
        ),
        RuntimeBackend::DockerCompose => {
            if !docker_is_running() {
                return Err(FunasrError::DockerUnavailable);
            }
            let cname = container_name(request.job_id, request.instance_nonce);
            let tail = vec![
                "run".into(),
                "--rm".into(),
                "--name".into(),
                cname.clone(),
                "--label".into(),
                "com.bimyscribe.app=true".into(),
                "--label".into(),
                format!("com.bimyscribe.job={}", request.job_id),
                "--label".into(),
                format!("com.bimyscribe.instance={}", request.instance_nonce),
                manifest
                    .transcribe_service
                    .clone()
                    .expect("validated transcribe service"),
                "/workspace/input/normalized.wav".into(),
                "--out".into(),
                "/workspace/output".into(),
            ];
            (
                compose_spec(
                    request.project_dir,
                    &manifest,
                    &input_dir,
                    &output_dir,
                    &model_cache,
                    &runtime_cache,
                    tail,
                )
                .log(request.log_path),
                Some(cname),
            )
        }
    };
    let mut handle = crate::process::spawn(spec)
        .map_err(|error| FunasrError::Process(format!("启动 Runtime：{error}")))?;
    let exit = loop {
        if request.cancel_token.is_cancelled() {
            let _ = handle.cancel();
            if let Some(cname) = &owned_container {
                let _ = remove_owned_container(cname, request.job_id, request.instance_nonce);
            }
            return Err(FunasrError::Cancelled);
        }
        if handle.is_running() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        } else {
            break handle
                .wait()
                .map_err(|error| FunasrError::Process(error.to_string()))?;
        }
    };
    if exit != 0 {
        return Err(FunasrError::Process(format!("Runtime 退出码 {exit}")));
    }

    let normalized_json = output_dir.join("normalized.json");
    let utterances = parse_transcript(&normalized_json)?;
    let dest = request.job_dir.join("transcript.raw.json");
    let data = serde_json::to_vec_pretty(&utterances).map_err(FunasrError::Parse)?;
    crate::jobs::atomic_write(&dest, &data).map_err(|error| FunasrError::Io(error.to_string()))?;
    let _ = std::fs::remove_file(input_copy);
    Ok(utterances)
}

fn remove_owned_container(container: &str, job_id: &uuid::Uuid, instance_nonce: &str) -> bool {
    let output = std::process::Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{ index .Config.Labels \"com.bimyscribe.app\" }}|{{ index .Config.Labels \"com.bimyscribe.job\" }}|{{ index .Config.Labels \"com.bimyscribe.instance\" }}",
            container,
        ])
        .output();
    let Ok(output) = output else { return false };
    if !output.status.success() {
        return false;
    }
    let labels = String::from_utf8_lossy(&output.stdout);
    if labels.trim() != format!("true|{job_id}|{instance_nonce}") {
        return false;
    }
    crate::process::docker_rm_force(container) == 0
}

/// Remove only containers whose labels prove they belong to a persisted job
/// from an earlier BiMyScribe process instance. Unknown containers are left
/// untouched.
pub fn cleanup_residual_containers<'a>(
    job_ids: impl IntoIterator<Item = &'a uuid::Uuid>,
    current_instance: &str,
) {
    let known: HashSet<String> = job_ids.into_iter().map(ToString::to_string).collect();
    let output = std::process::Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=com.bimyscribe.app=true",
            "--format",
            "{{.ID}}",
        ])
        .output();
    let Ok(output) = output else { return };
    if !output.status.success() {
        return;
    }
    for container in String::from_utf8_lossy(&output.stdout).lines() {
        let inspect = std::process::Command::new("docker")
            .args([
                "inspect",
                "--format",
                "{{ index .Config.Labels \"com.bimyscribe.job\" }}|{{ index .Config.Labels \"com.bimyscribe.instance\" }}",
                container,
            ])
            .output();
        let Ok(inspect) = inspect else { continue };
        if !inspect.status.success() {
            continue;
        }
        let labels = String::from_utf8_lossy(&inspect.stdout);
        let Some((job, instance)) = labels.trim().split_once('|') else {
            continue;
        };
        if known.contains(job) && !instance.is_empty() && instance != current_instance {
            let _ = crate::process::docker_rm_force(container);
        }
    }
}

pub fn parse_transcript(path: &Path) -> Result<Vec<Utterance>, FunasrError> {
    let data = std::fs::read(path).map_err(FunasrError::Read)?;
    let transcript: NormalizedTranscript =
        serde_json::from_slice(&data).map_err(FunasrError::Parse)?;
    if transcript.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(FunasrError::Validation(format!(
            "不支持 normalized schema_version {}",
            transcript.schema_version
        )));
    }
    let mut speakers = HashMap::<String, u32>::new();
    let mut out = Vec::with_capacity(transcript.segments.len());
    for (index, segment) in transcript.segments.into_iter().enumerate() {
        if segment.end_ms < segment.start_ms {
            return Err(FunasrError::Validation(format!(
                "segment {} 的结束时间早于开始时间",
                index + 1
            )));
        }
        let speaker_id = match segment.speaker {
            serde_json::Value::Null => 0,
            serde_json::Value::String(label) => {
                // Reserve zero for segments whose speaker is explicitly null,
                // so unknown speech is never merged with the first detected
                // speaker.
                let next = speakers.len() as u32 + 1;
                *speakers.entry(label).or_insert(next)
            }
            _ => {
                return Err(FunasrError::Validation(format!(
                    "segment {} 的 speaker 必须是字符串或 null",
                    index + 1
                )));
            }
        };
        out.push(Utterance {
            id: format!("u{:04}", index + 1),
            text: segment.text,
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            speaker_id,
        });
    }
    validate(&out)?;
    Ok(out)
}

fn validate(utterances: &[Utterance]) -> Result<(), FunasrError> {
    let mut ids = HashSet::new();
    for utterance in utterances {
        if !ids.insert(&utterance.id) {
            return Err(FunasrError::Validation(format!(
                "重复的 utterance id：{}",
                utterance.id
            )));
        }
        if utterance.end_ms < utterance.start_ms {
            return Err(FunasrError::Validation(format!(
                "{} 的结束时间早于开始时间",
                utterance.id
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum ProgressEvent {
    Started { total_ms: u64 },
    Indeterminate,
    Completed,
}

#[derive(Debug, Error)]
pub enum FunasrError {
    #[error("读取 Runtime 数据：{0}")]
    Read(std::io::Error),
    #[error("解析转录结果：{0}")]
    Parse(serde_json::Error),
    #[error("解析 Runtime manifest：{0}")]
    Manifest(String),
    #[error("验证失败：{0}")]
    Validation(String),
    #[error("I/O：{0}")]
    Io(String),
    #[error("Docker：{0}")]
    Docker(String),
    #[error("Docker Desktop 未运行")]
    DockerUnavailable,
    #[error("Runtime 进程：{0}")]
    Process(String),
    #[error("任务已取消")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_fixture(manifest: &str, files: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bimyscribe-runtime-manifest-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("schemas")).unwrap();
        std::fs::write(root.join(RUNTIME_MANIFEST), manifest).unwrap();
        for file in files {
            let path = root.join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, "fixture").unwrap();
        }
        root
    }

    #[test]
    fn loads_native_uv_contract_without_running_uv() {
        let root = runtime_fixture(
            r#"contract_version = 1
backend = "native-uv"
entrypoint = "transcribe.py"
output_schema_version = 1
output_schema_file = "schemas/normalized-v1.schema.json"
"#,
            &[
                "transcribe.py",
                "pyproject.toml",
                ".python-version",
                "uv.lock",
                "schemas/normalized-v1.schema.json",
            ],
        );
        let manifest = load_runtime(&root).unwrap();
        assert_eq!(manifest.backend, RuntimeBackend::NativeUv);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn loads_docker_contract_without_requiring_native_files() {
        let root = runtime_fixture(
            r#"contract_version = 1
backend = "docker-compose"
compose_file = "docker-compose.yml"
transcribe_service = "transcribe"
probe_service = "probe"
output_schema_version = 1
output_schema_file = "schemas/normalized-v1.schema.json"
"#,
            &["docker-compose.yml", "schemas/normalized-v1.schema.json"],
        );
        let manifest = load_runtime(&root).unwrap();
        assert_eq!(manifest.backend, RuntimeBackend::DockerCompose);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn parses_normalized_schema_v1() {
        let path =
            std::env::temp_dir().join(format!("bimyscribe-schema-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"{"schema_version":1,"segments":[{"text":"你好","start_ms":0,"end_ms":500,"speaker":"speaker-1"},{"text":"世界","start_ms":600,"end_ms":900,"speaker":null}]}"#,
        )
        .unwrap();
        let utterances = parse_transcript(&path).unwrap();
        assert_eq!(utterances.len(), 2);
        assert_eq!(utterances[0].speaker_id, 1);
        assert_eq!(utterances[1].speaker_id, 0);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let path =
            std::env::temp_dir().join(format!("bimyscribe-schema-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, r#"{"schema_version":2,"segments":[]}"#).unwrap();
        assert!(matches!(
            parse_transcript(&path),
            Err(FunasrError::Validation(_))
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_missing_or_invalid_speaker_field() {
        for document in [
            r#"{"schema_version":1,"segments":[{"text":"你好","start_ms":0,"end_ms":1}]}"#,
            r#"{"schema_version":1,"segments":[{"text":"你好","start_ms":0,"end_ms":1,"speaker":7}]}"#,
        ] {
            let path = std::env::temp_dir()
                .join(format!("bimyscribe-schema-{}.json", uuid::Uuid::new_v4()));
            std::fs::write(&path, document).unwrap();
            assert!(parse_transcript(&path).is_err());
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn native_uv_environment_redirects_all_large_or_temporary_data() {
        let runtime_data = Path::new("/runtime-data");
        let spec = native_uv_environment(
            crate::process::SubprocessSpec::new(vec!["uv".into(), "--version".into()]),
            runtime_data,
        );
        let environment: HashMap<_, _> = spec.env.into_iter().collect();
        for (name, relative) in [
            ("UV_PROJECT_ENVIRONMENT", "python-environment"),
            ("UV_CACHE_DIR", "uv-cache"),
            ("UV_PYTHON_INSTALL_DIR", "python-installations"),
            ("MODELSCOPE_CACHE", "models"),
            ("HF_HOME", "huggingface"),
            ("TORCH_HOME", "torch"),
            ("TMPDIR", "tmp"),
            ("BIMYSCRIBE_FUNASR_CACHE_DIR", "cache"),
        ] {
            assert_eq!(
                environment.get(std::ffi::OsStr::new(name)),
                Some(&runtime_data.join(relative).into_os_string())
            );
        }
        assert_eq!(
            environment.get(std::ffi::OsStr::new("UV_NO_CONFIG")),
            Some(&std::ffi::OsString::from("1"))
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("UV_PYTHON_PREFERENCE")),
            Some(&std::ffi::OsString::from("only-managed"))
        );
    }

    #[test]
    fn container_name_contains_instance_and_job() {
        let id = uuid::Uuid::new_v4();
        let name = container_name(&id, "0123456789abcdef");
        assert_eq!(name, format!("bimyscribe-0123456789ab-{id}"));
    }

    #[test]
    #[ignore = "requires an explicitly configured Docker Runtime and audio sample"]
    fn docker_runtime_adapter_smoke() {
        let required_path = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_else(|| panic!("{name} must be set for this ignored test"))
        };
        let project_dir = required_path("BIMYSCRIBE_RUNTIME_PROJECT");
        let runtime_data = required_path("BIMYSCRIBE_RUNTIME_DATA_DIR");
        let normalized_wav = required_path("BIMYSCRIBE_RUNTIME_SMOKE_WAV");
        let smoke_root = required_path("BIMYSCRIBE_RUNTIME_SMOKE_ROOT");
        let job_dir = smoke_root.join(format!("job-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&job_dir).unwrap();

        let ready = install_runtime(&project_dir, &runtime_data).unwrap();
        assert_eq!(ready.device, "Docker CPU");
        let utterances = run(TranscribeRequest {
            job_dir: &job_dir,
            normalized_wav: &normalized_wav,
            project_dir: &project_dir,
            runtime_data_dir: &runtime_data,
            log_path: &job_dir.join("runtime.log"),
            job_id: &uuid::Uuid::new_v4(),
            instance_nonce: "runtime-smoke-instance",
            cancel_token: &crate::cancel::CancellationToken::new(),
        })
        .unwrap();

        assert!(!utterances.is_empty());
        assert!(job_dir.join("transcript.raw.json").is_file());
    }

    #[test]
    #[ignore = "requires an explicitly configured native Runtime and audio sample"]
    fn native_runtime_adapter_smoke() {
        let required_path = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_else(|| panic!("{name} must be set for this ignored test"))
        };
        let project_dir = required_path("BIMYSCRIBE_NATIVE_RUNTIME_PROJECT");
        let runtime_data = required_path("BIMYSCRIBE_NATIVE_RUNTIME_DATA_DIR");
        let normalized_wav = required_path("BIMYSCRIBE_NATIVE_RUNTIME_SMOKE_WAV");
        let smoke_root = required_path("BIMYSCRIBE_NATIVE_RUNTIME_SMOKE_ROOT");
        let job_dir = smoke_root.join(format!("job-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&job_dir).unwrap();

        let ready = install_runtime(&project_dir, &runtime_data).unwrap();
        assert!(matches!(ready.device.as_str(), "mps" | "cpu"));
        let utterances = run(TranscribeRequest {
            job_dir: &job_dir,
            normalized_wav: &normalized_wav,
            project_dir: &project_dir,
            runtime_data_dir: &runtime_data,
            log_path: &job_dir.join("runtime.log"),
            job_id: &uuid::Uuid::new_v4(),
            instance_nonce: "native-runtime-smoke-instance",
            cancel_token: &crate::cancel::CancellationToken::new(),
        })
        .unwrap();

        assert!(!utterances.is_empty());
        assert!(job_dir.join("transcript.raw.json").is_file());
        assert!(utterances.iter().any(|utterance| utterance.speaker_id > 1));
    }
}
