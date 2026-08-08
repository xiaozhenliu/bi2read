//! Persisted application configuration.
//! Secrets are NOT stored here; they live in macOS Keychain (TODO step 8) and
//! are referenced by `key_ref`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::jobs::RetentionPolicy;
use crate::paths::AppPaths;

#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub working_dir: PathBuf,
    pub output_dir: PathBuf,
    /// Path to a compatible BiMyScribe FunASR Runtime project.
    pub runtime_project: Option<PathBuf>,
    /// Python environment, dependency, model, and temporary cache root.
    pub runtime_data_dir: PathBuf,
    pub llm_enabled: bool,
    pub llm_connection: Option<crate::llm::LlmConnection>,
    pub default_screenshots: bool,
    pub default_retention: RetentionPolicy,
    /// Unique to this process; never persisted or accepted from the UI.
    #[serde(skip)]
    pub instance_nonce: String,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    working_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    #[serde(alias = "funasr_script")]
    runtime_project: Option<PathBuf>,
    runtime_data_dir: Option<PathBuf>,
    llm_enabled: Option<bool>,
    llm_connection: Option<crate::llm::LlmConnection>,
    default_screenshots: Option<bool>,
    default_retention: Option<RetentionPolicy>,
}

impl Default for Config {
    fn default() -> Self {
        AppPaths::discover()
            .map(|paths| Self::for_paths(&paths))
            .unwrap_or_else(|_| Self {
                working_dir: PathBuf::from("BiMyScribe/Jobs"),
                output_dir: PathBuf::from("BiMyScribe/Documents"),
                runtime_project: None,
                runtime_data_dir: PathBuf::from("BiMyScribe/Runtime"),
                llm_enabled: false,
                llm_connection: None,
                default_screenshots: false,
                default_retention: RetentionPolicy::Recommended,
                instance_nonce: new_instance_nonce(),
            })
    }
}

impl Config {
    pub fn for_paths(paths: &AppPaths) -> Self {
        Self {
            working_dir: paths.jobs_dir(),
            output_dir: paths.markdown_output_dir(),
            runtime_project: None,
            runtime_data_dir: paths.funasr_runtime_data_dir(),
            llm_enabled: false,
            llm_connection: None,
            default_screenshots: false,
            default_retention: RetentionPolicy::Recommended,
            instance_nonce: new_instance_nonce(),
        }
    }

    pub fn load() -> std::io::Result<Self> {
        let paths = AppPaths::discover()?;
        Self::load_from(&paths.config_file(), &paths)
    }

    pub fn load_from(path: &std::path::Path, paths: &AppPaths) -> std::io::Result<Self> {
        if !path.exists() {
            return Ok(Self::for_paths(paths));
        }
        let data = std::fs::read_to_string(path)?;
        let file: ConfigFile = toml::from_str(&data).map_err(std::io::Error::other)?;
        let mut config = Self::for_paths(paths);
        if let Some(value) = file.working_dir {
            config.working_dir = value;
        }
        if let Some(value) = file.output_dir {
            config.output_dir = value;
        }
        config.runtime_project = file.runtime_project;
        if let Some(value) = file.runtime_data_dir {
            config.runtime_data_dir = value;
        }
        if let Some(value) = file.llm_enabled {
            config.llm_enabled = value;
        }
        config.llm_connection = file.llm_connection;
        if let Some(value) = file.default_screenshots {
            config.default_screenshots = value;
        }
        if let Some(value) = file.default_retention {
            config.default_retention = value;
        }
        Ok(config)
    }

    pub fn normalize_legacy_defaults(&mut self, paths: &AppPaths) {
        if is_private_default(&self.working_dir) {
            self.working_dir = paths.jobs_dir();
        }
        if self.output_dir.as_os_str().is_empty() || is_private_default(&self.output_dir) {
            self.output_dir = paths.markdown_output_dir();
        }
        if self
            .runtime_project
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty() || is_private_default(path))
        {
            self.runtime_project = None;
        }
        if self.runtime_data_dir.as_os_str().is_empty()
            || is_private_default(&self.runtime_data_dir)
        {
            self.runtime_data_dir = paths.funasr_runtime_data_dir();
        }
    }

    /// Atomically persist the config.
    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::jobs::atomic_write(&path, data.as_bytes())
    }

    // ---- Conversions to/from the Slint SettingsView ----

    pub fn to_view(&self) -> crate::SettingsView {
        let conn = self.llm_connection.as_ref();
        let (base_url, api_format, model, name) = match conn {
            Some(c) => (
                c.base_url.clone(),
                c.api_format.label().to_string(),
                c.model.clone(),
                c.name.clone(),
            ),
            None => (
                String::new(),
                crate::llm::ApiFormat::OpenAiChatCompletions
                    .label()
                    .to_string(),
                String::new(),
                String::new(),
            ),
        };
        let is_remote = !base_url.is_empty() && conn.map(|c| !c.is_local()).unwrap_or(false);
        crate::SettingsView {
            working_dir: self.working_dir.to_string_lossy().to_string().into(),
            output_dir: self.output_dir.to_string_lossy().to_string().into(),
            runtime_project: self
                .runtime_project
                .as_deref()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default()
                .into(),
            funasr_status: self
                .runtime_project
                .as_deref()
                .map(|project| crate::funasr::runtime_status(project, &self.runtime_data_dir))
                .unwrap_or_else(|| "未配置".into())
                .into(),
            runtime_data_dir: self.runtime_data_dir.to_string_lossy().to_string().into(),
            llm_enabled: self.llm_enabled,
            llm_connection_name: name.into(),
            llm_model: model.into(),
            llm_base_url: base_url.into(),
            llm_api_format: api_format.into(),
            llm_remote_warn: is_remote && self.llm_enabled,
            default_screenshots: self.default_screenshots,
            default_retention: self.default_retention.label().into(),
        }
    }

    /// Apply the Slint view to this config. Returns `Err` with a structured
    /// message if validation fails.
    pub fn apply_view(&mut self, s: &crate::SettingsView) -> Result<(), String> {
        self.working_dir =
            validate_writable_directory(&PathBuf::from(s.working_dir.to_string()), "工作目录")?;
        self.output_dir = validate_writable_directory(
            &PathBuf::from(s.output_dir.to_string()),
            "Markdown 输出目录",
        )?;
        self.runtime_data_dir = validate_writable_directory(
            &PathBuf::from(s.runtime_data_dir.to_string()),
            "Runtime 数据目录",
        )?;
        let runtime_project = s.runtime_project.to_string();
        self.runtime_project = if runtime_project.trim().is_empty() {
            None
        } else {
            let runtime =
                absolute_existing_directory(&PathBuf::from(runtime_project), "FunASR Runtime")?;
            crate::funasr::load_runtime(&runtime)
                .map_err(|error| format!("FunASR Runtime：{error}"))?;
            Some(runtime)
        };
        self.llm_enabled = s.llm_enabled;
        if s.llm_enabled || !s.llm_base_url.is_empty() {
            let api_format = crate::llm::ApiFormat::parse(&s.llm_api_format)
                .unwrap_or(crate::llm::ApiFormat::OpenAiChatCompletions);
            // Normalize the base URL to prevent duplicate path segments.
            let base_url = crate::llm::normalize_base_url(s.llm_base_url.as_ref());
            self.llm_connection = Some(crate::llm::LlmConnection {
                id: "default".into(),
                name: s.llm_connection_name.to_string(),
                api_format,
                base_url,
                model: s.llm_model.to_string(),
            });
        } else {
            self.llm_connection = None;
        }
        self.default_screenshots = s.default_screenshots;
        self.default_retention = RetentionPolicy::from_label(s.default_retention.as_ref());
        Ok(())
    }
}

fn new_instance_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn config_path() -> std::io::Result<PathBuf> {
    Ok(AppPaths::discover()?.config_file())
}

fn is_private_default(path: &std::path::Path) -> bool {
    let value = path.to_string_lossy();
    (value.starts_with("/Volumes/") && value.ends_with("/bilibili-reader/jobs"))
        || (value.starts_with("/Users/") && value.ends_with("/Projects/funasr-docker"))
}

fn absolute_existing_directory(path: &std::path::Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label}必须是绝对路径"));
    }
    if !path.is_dir() {
        return Err(format!("{label}不是有效目录：{}", path.display()));
    }
    path.canonicalize()
        .map_err(|error| format!("无法解析{label}：{error}"))
}

fn validate_writable_directory(path: &std::path::Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label}必须是绝对路径"));
    }
    if path.is_file() {
        return Err(format!("{label}指向普通文件"));
    }

    let (probe_parent, resolved) = if path.exists() {
        let resolved = path
            .canonicalize()
            .map_err(|error| format!("无法解析{label}：{error}"))?;
        (resolved.clone(), resolved)
    } else {
        let mut ancestor = path.to_path_buf();
        let mut missing = Vec::new();
        while !ancestor.exists() {
            let name = ancestor
                .file_name()
                .ok_or_else(|| format!("{label}没有可创建的父目录"))?
                .to_os_string();
            missing.push(name);
            if !ancestor.pop() {
                return Err(format!("{label}没有可创建的父目录"));
            }
        }
        if !ancestor.is_dir() {
            return Err(format!("{label}的已有父路径不是目录"));
        }
        let resolved_ancestor = ancestor
            .canonicalize()
            .map_err(|error| format!("无法解析{label}父目录：{error}"))?;
        let mut resolved = resolved_ancestor.clone();
        for name in missing.into_iter().rev() {
            resolved.push(name);
        }
        (resolved_ancestor, resolved)
    };

    let probe = probe_parent.join(format!(".bimyscribe-write-probe-{}", uuid::Uuid::new_v4()));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
        .map_err(|error| format!("{label}不可写：{error}"))?;
    drop(file);
    std::fs::remove_file(&probe).map_err(|error| format!("{label}探针清理失败：{error}"))?;
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_policy_roundtrip() {
        for p in [
            RetentionPolicy::Recommended,
            RetentionPolicy::KeepAll,
            RetentionPolicy::DocumentsOnly,
        ] {
            assert_eq!(RetentionPolicy::from_label(p.label()), p);
        }
        // Unknown label defaults to Recommended.
        assert_eq!(
            RetentionPolicy::from_label("garbage"),
            RetentionPolicy::Recommended
        );
    }

    #[test]
    fn retention_policy_serde() {
        let json = serde_json::to_string(&RetentionPolicy::KeepAll).unwrap();
        assert_eq!(json, "\"keep_all\"");
        let p: RetentionPolicy = serde_json::from_str("\"documents_only\"").unwrap();
        assert_eq!(p, RetentionPolicy::DocumentsOnly);
        // Old/default value deserializes correctly.
        let p: RetentionPolicy = serde_json::from_str("\"recommended\"").unwrap();
        assert_eq!(p, RetentionPolicy::Recommended);
    }

    #[test]
    fn default_config_uses_recommended_retention() {
        let cfg = Config::default();
        assert_eq!(cfg.default_retention, RetentionPolicy::Recommended);
    }

    #[test]
    fn missing_fields_use_explicit_platform_defaults() {
        let home = std::env::temp_dir().join(format!("bimyscribe-config-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::for_home(&home);
        std::fs::create_dir_all(paths.application_support()).unwrap();
        std::fs::write(paths.config_file(), "llm_enabled = true\n").unwrap();
        let config = Config::load_from(&paths.config_file(), &paths).unwrap();
        assert_eq!(config.working_dir, paths.jobs_dir());
        assert_eq!(config.output_dir, paths.markdown_output_dir());
        assert!(config.llm_enabled);
        assert_eq!(config.runtime_project, None);
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn legacy_runtime_field_loads_but_only_new_field_is_saved() {
        let home = std::env::temp_dir().join(format!("bimyscribe-config-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::for_home(&home);
        std::fs::create_dir_all(paths.application_support()).unwrap();
        let runtime = home.join("runtime");
        std::fs::write(
            paths.config_file(),
            format!("funasr_script = {:?}\n", runtime.to_string_lossy()),
        )
        .unwrap();

        let config = Config::load_from(&paths.config_file(), &paths).unwrap();
        assert_eq!(config.runtime_project.as_deref(), Some(runtime.as_path()));
        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains("runtime_project"));
        assert!(!serialized.contains("funasr_script"));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn writable_directory_validation_resolves_symlink_and_does_not_create_target() {
        let root = std::env::temp_dir().join(format!("bimyscribe-path-{}", uuid::Uuid::new_v4()));
        let actual = root.join("actual");
        std::fs::create_dir_all(&actual).unwrap();

        #[cfg(unix)]
        {
            let link = root.join("linked");
            std::os::unix::fs::symlink(&actual, &link).unwrap();
            assert_eq!(
                validate_writable_directory(&link, "测试目录").unwrap(),
                actual.canonicalize().unwrap()
            );
        }

        let missing = actual.join("new").join("nested");
        assert_eq!(
            validate_writable_directory(&missing, "测试目录").unwrap(),
            actual.canonicalize().unwrap().join("new").join("nested")
        );
        assert!(!missing.exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn writable_directory_validation_rejects_relative_and_regular_file_paths() {
        assert!(
            validate_writable_directory(std::path::Path::new("relative"), "测试目录")
                .unwrap_err()
                .contains("绝对路径")
        );

        let root = std::env::temp_dir().join(format!("bimyscribe-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("ordinary-file");
        std::fs::write(&file, "fixture").unwrap();
        assert!(validate_writable_directory(&file, "测试目录")
            .unwrap_err()
            .contains("普通文件"));
        std::fs::remove_dir_all(root).ok();
    }
}
