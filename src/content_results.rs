//! v0.5 结构化文字结果的持久化 schema 与文件边界。
//!
//! 这一层负责 Content Current 的机器可读事实、Job 创建时冻结的初始设置、四类内容的
//! 生成/来源校验和 whole-file 原子存储。调用方只表达 Intent，不应绕过本模块直接读写
//! raw Evidence 或 current 文件。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::cancel::CancellationToken;
use crate::funasr::Utterance;
use crate::llm::{ApiFormat, LlmConnection};

/// The only structured content fact file for a v0.5 Job. The `v1` names the
/// file family, not the schema version inside it (v0.6 keeps this name).
pub(crate) const CONTENT_CURRENT_FILE: &str = "content-current.v1.json";
/// On-disk schema version written by new publications. Readers accept both
/// 1 and 2; a v1 byte stream decodes into the in-memory v2 shape with the
/// summary-tier slots left empty via `#[serde(default)]`.
pub(crate) const CONTENT_SCHEMA_VERSION: u32 = 2;
/// Schema versions accepted by readers: everything from the first published
/// version up to [`CONTENT_SCHEMA_VERSION`]. Anything newer is rejected.
const CONTENT_SCHEMA_VERSIONS_READ: [u32; 2] = [1, 2];

/// Readers accept every published schema version up to the current one;
/// writers always emit [`CONTENT_SCHEMA_VERSION`]. A v1 byte stream decodes
/// into the in-memory v2 shape unchanged.
pub(crate) fn is_supported_content_schema_version(version: u32) -> bool {
    CONTENT_SCHEMA_VERSIONS_READ.contains(&version)
}

/// Resolve the single "primary consumption artifact" for F5 reading-time
/// estimation: the selected summary tier when it has been generated, otherwise
/// directly falls back to the faithful body text (never silently falls back
/// to another summary tier). All three display surfaces (result caption, job
/// detail, full.md metadata) call this one function.
pub(crate) fn primary_consumption_record(
    snapshot: &ContentSnapshotV1,
    selected_tier: Option<ArtifactKindV1>,
) -> Option<&DerivationRecordV1> {
    if let Some(kind) = selected_tier {
        if let Some(record) = snapshot.current.slots.get(kind).current.as_ref() {
            return Some(record);
        }
    }
    snapshot.current.slots.faithful_text.current.as_ref()
}

/// Plain text of one record, in block order. Structured records never carry
/// Markdown syntax or metadata by construction.
pub(crate) fn record_plain_text(record: &DerivationRecordV1) -> String {
    record
        .blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
pub(crate) const CONTENT_RECIPE_SET_VERSION: u32 = 1;
pub(crate) const EVIDENCE_PLATFORM: &str = "bilibili";
const MAX_UNAVAILABLE_MESSAGE_CHARS: usize = 160;
const MAX_UNAVAILABLE_FIELD_CHARS: usize = 64;

/// Return the lowercase hexadecimal SHA-256 digest used by raw Evidence and
/// identity material. Keeping this tiny primitive in the content module
/// prevents each caller from inventing a different encoding.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_sha256_hex(field: &str, value: &str) -> Result<(), ContentSchemaError> {
    if value.len() != 64
        || !value
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, 'a'..='f'))
    {
        return Err(ContentSchemaError::Invariant(format!(
            "{field} must be a 64-character lowercase hexadecimal SHA-256"
        )));
    }
    Ok(())
}

/// A persisted unavailable-target reason must never carry the endpoint,
/// credentials, query parameters, or an unbounded adapter error. The resolver
/// is responsible for choosing the short user-facing message; schema
/// validation is the final persistence boundary.
fn contains_sensitive_detail(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "://",
        "@",
        "?",
        "#",
        "authorization",
        "x-api-key",
        "api_key",
        "token",
        "secret",
        "password",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Parse and validate the canonical v0.5 local, no-auth endpoint.
///
/// The returned string is the only persisted/requestable spelling: scheme and
/// host are lowercase, default ports and trailing slashes are absent, and one
/// exact protocol suffix is removed. Callers that receive user/config input
/// persist this returned value; persisted fields use
/// [`require_canonical_local_endpoint`] to reject another spelling.
pub(crate) fn canonical_local_endpoint(endpoint: &str) -> Result<String, ContentSchemaError> {
    let input = endpoint.trim();
    let parsed = Url::parse(input)
        .map_err(|_| ContentSchemaError::Invariant("endpoint is not a valid URL".into()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ContentSchemaError::Invariant(
            "endpoint scheme must be http or https".into(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ContentSchemaError::Invariant(
            "endpoint must not contain userinfo".into(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ContentSchemaError::Invariant(
            "endpoint must not contain query or fragment".into(),
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| ContentSchemaError::Invariant("endpoint must contain a host".into()))?;
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if !matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
        return Err(ContentSchemaError::Invariant(
            "endpoint host must be localhost or loopback".into(),
        ));
    }

    let mut path = parsed.path().to_owned();
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    for suffix in ["/v1/chat/completions", "/v1/messages", "/v1"] {
        if path == suffix {
            path.clear();
            break;
        }
        if path.ends_with(suffix) {
            path.truncate(path.len() - suffix.len());
            break;
        }
    }
    if path == "/" {
        path.clear();
    }

    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    let mut canonical = format!("{}://{host}", parsed.scheme());
    if let Some(port) = parsed.port() {
        let is_default = (parsed.scheme() == "http" && port == 80)
            || (parsed.scheme() == "https" && port == 443);
        if !is_default {
            canonical.push(':');
            canonical.push_str(&port.to_string());
        }
    }
    canonical.push_str(&path);

    Ok(canonical)
}

fn require_canonical_local_endpoint(endpoint: &str) -> Result<(), ContentSchemaError> {
    let canonical = canonical_local_endpoint(endpoint)?;
    if endpoint.trim() != canonical {
        return Err(ContentSchemaError::Invariant(
            "endpoint must use its canonical normalized spelling".into(),
        ));
    }
    Ok(())
}

/// Job creation-time content setup. `Some` on [`crate::jobs::Job`] is the v0.5
/// marker; `None` is reserved for legacy jobs and is never inferred from files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ContentSetupV1 {
    pub(crate) schema_version: u32,
    pub(crate) enhancements_requested: bool,
    pub(crate) recipe_set_version: u32,
    pub(crate) initial_target: InitialTargetV1,
}

impl ContentSetupV1 {
    pub(crate) fn disabled() -> Self {
        Self {
            schema_version: CONTENT_SCHEMA_VERSION,
            enhancements_requested: false,
            recipe_set_version: CONTENT_RECIPE_SET_VERSION,
            initial_target: InitialTargetV1::Disabled,
        }
    }

    pub(crate) fn ready(target: ReadyGenerationTargetV1) -> Self {
        Self {
            schema_version: CONTENT_SCHEMA_VERSION,
            enhancements_requested: true,
            recipe_set_version: CONTENT_RECIPE_SET_VERSION,
            initial_target: InitialTargetV1::Ready(target),
        }
    }

    pub(crate) fn unavailable(target: TargetUnavailableV1) -> Self {
        Self {
            schema_version: CONTENT_SCHEMA_VERSION,
            enhancements_requested: true,
            recipe_set_version: CONTENT_RECIPE_SET_VERSION,
            initial_target: InitialTargetV1::Unavailable(target),
        }
    }

    /// Build the frozen creation marker from the resolver's closed three-state
    /// result. Keeping the boolean derived here prevents callers from making
    /// `enhancements_requested` disagree with the selected target variant.
    #[cfg(test)]
    pub(crate) fn from_initial_target(initial_target: InitialTargetV1) -> Self {
        match initial_target {
            InitialTargetV1::Disabled => Self::disabled(),
            InitialTargetV1::Ready(target) => Self::ready(target),
            InitialTargetV1::Unavailable(target) => Self::unavailable(target),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ContentSchemaError> {
        if !is_supported_content_schema_version(self.schema_version) {
            return Err(ContentSchemaError::SchemaVersion {
                expected: CONTENT_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }
        if self.recipe_set_version != CONTENT_RECIPE_SET_VERSION {
            return Err(ContentSchemaError::RecipeSetVersion {
                expected: CONTENT_RECIPE_SET_VERSION,
                actual: self.recipe_set_version,
            });
        }
        match (&self.initial_target, self.enhancements_requested) {
            (InitialTargetV1::Disabled, false) => Ok(()),
            (InitialTargetV1::Ready(target), true) => target.validate(),
            (InitialTargetV1::Unavailable(target), true) => target.validate(),
            (InitialTargetV1::Disabled, true) => Err(ContentSchemaError::Invariant(
                "disabled target cannot request enhancements".into(),
            )),
            (InitialTargetV1::Ready(_), false) | (InitialTargetV1::Unavailable(_), false) => Err(
                ContentSchemaError::Invariant("enabled target must request enhancements".into()),
            ),
        }
    }
}

/// Resolve the creation-time content plan from the one persisted Config
/// connection.  Both CLI and desktop creation call this function; the result
/// is immediately frozen into `Job.content_setup`.  In particular, an
/// enabled-but-invalid connection is never silently converted to Disabled.
pub(crate) fn resolve_content_setup(
    llm_enabled: bool,
    connection: Option<&LlmConnection>,
) -> ContentSetupV1 {
    if !llm_enabled {
        return ContentSetupV1::disabled();
    }

    let Some(connection) = connection else {
        return ContentSetupV1::unavailable(TargetUnavailableV1 {
            code: TargetUnavailableCodeV1::ConnectionMissing,
            message: "未配置本地 AI".into(),
            invalid_field: Some("llm_connection".into()),
        });
    };

    let (provider, api_format, parameters) = match connection.api_format {
        ApiFormat::OpenAiChatCompletions => (
            GenerationProviderV1::OpenAiCompatible,
            GenerationApiFormatV1::OpenAiChatCompletions,
            GenerationParametersV1::openai_default(),
        ),
        ApiFormat::AnthropicMessages => (
            GenerationProviderV1::AnthropicCompatible,
            GenerationApiFormatV1::AnthropicMessages,
            GenerationParametersV1::anthropic_default(),
        ),
    };

    let endpoint = match canonical_local_endpoint(&connection.base_url) {
        Ok(endpoint) => endpoint,
        Err(_) => {
            return ContentSetupV1::unavailable(TargetUnavailableV1 {
                code: TargetUnavailableCodeV1::EndpointInvalid,
                message: "本地 AI 地址不可用".into(),
                invalid_field: Some("llm_connection.base_url".into()),
            });
        }
    };
    let model = connection.model.trim();
    if model.is_empty() {
        return ContentSetupV1::unavailable(TargetUnavailableV1 {
            code: TargetUnavailableCodeV1::ModelMissing,
            message: "未配置本地 AI 模型".into(),
            invalid_field: Some("llm_connection.model".into()),
        });
    }

    ContentSetupV1::ready(ReadyGenerationTargetV1 {
        provider,
        api_format,
        endpoint,
        model: model.to_string(),
        parameters,
    })
}

/// Initial target status is deliberately closed. No credentials or raw
/// endpoint are persisted for an unavailable target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum InitialTargetV1 {
    Disabled,
    Ready(ReadyGenerationTargetV1),
    Unavailable(TargetUnavailableV1),
}

/// Local, no-auth target captured at Job creation or at an explicit
/// regeneration click. Target resolution is implemented by the generation
/// layer; this type only validates the persisted shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReadyGenerationTargetV1 {
    pub(crate) provider: GenerationProviderV1,
    pub(crate) api_format: GenerationApiFormatV1,
    pub(crate) endpoint: String,
    pub(crate) model: String,
    pub(crate) parameters: GenerationParametersV1,
}

impl ReadyGenerationTargetV1 {
    pub(crate) fn validate(&self) -> Result<(), ContentSchemaError> {
        require_canonical_local_endpoint(&self.endpoint)?;
        validate_canonical_model(&self.model)?;
        if self.model.trim().is_empty() {
            return Err(ContentSchemaError::Invariant(
                "model must not be empty".into(),
            ));
        }
        self.parameters.validate()?;
        let compatible = matches!(
            (&self.provider, &self.api_format, &self.parameters),
            (
                GenerationProviderV1::OpenAiCompatible,
                GenerationApiFormatV1::OpenAiChatCompletions,
                GenerationParametersV1::OpenAiChatCompletions { .. }
            ) | (
                GenerationProviderV1::AnthropicCompatible,
                GenerationApiFormatV1::AnthropicMessages,
                GenerationParametersV1::AnthropicMessages { .. }
            )
        );
        if !compatible {
            return Err(ContentSchemaError::Invariant(
                "provider, api_format, and parameters are incompatible".into(),
            ));
        }
        Ok(())
    }
}

fn validate_canonical_model(model: &str) -> Result<(), ContentSchemaError> {
    if model.trim() != model {
        return Err(ContentSchemaError::Invariant(
            "model must use its canonical trimmed spelling".into(),
        ));
    }
    if model.chars().any(char::is_control) {
        return Err(ContentSchemaError::Invariant(
            "model must not contain control characters".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenerationProviderV1 {
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
    #[serde(rename = "anthropic_compatible")]
    AnthropicCompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenerationApiFormatV1 {
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    AnthropicMessages,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenerationParametersV1 {
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions {
        temperature: f64,
    },
    AnthropicMessages {
        max_tokens: u32,
    },
}

impl GenerationParametersV1 {
    pub(crate) fn openai_default() -> Self {
        Self::OpenAiChatCompletions { temperature: 0.2 }
    }

    pub(crate) fn anthropic_default() -> Self {
        Self::AnthropicMessages { max_tokens: 4096 }
    }

    pub(crate) fn max_tokens_for_probe(&self) -> u32 {
        match self {
            Self::AnthropicMessages { max_tokens } => *max_tokens,
            Self::OpenAiChatCompletions { .. } => 4096,
        }
    }

    fn validate(&self) -> Result<(), ContentSchemaError> {
        match self {
            Self::OpenAiChatCompletions { temperature }
                if temperature.is_finite() && temperature.to_bits() == 0.2f64.to_bits() =>
            {
                Ok(())
            }
            Self::AnthropicMessages { max_tokens } if *max_tokens == 4096 => Ok(()),
            Self::OpenAiChatCompletions { .. } => Err(ContentSchemaError::Invariant(
                "OpenAI temperature must be exactly 0.2".into(),
            )),
            Self::AnthropicMessages { .. } => Err(ContentSchemaError::Invariant(
                "Anthropic max_tokens must be exactly 4096".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TargetUnavailableCodeV1 {
    ConnectionMissing,
    ProtocolUnsupported,
    EndpointInvalid,
    ModelMissing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TargetUnavailableV1 {
    pub(crate) code: TargetUnavailableCodeV1,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) invalid_field: Option<String>,
}

impl TargetUnavailableV1 {
    fn validate(&self) -> Result<(), ContentSchemaError> {
        let message = self.message.trim();
        if message.is_empty()
            || message.chars().count() > MAX_UNAVAILABLE_MESSAGE_CHARS
            || message.chars().any(char::is_control)
            || contains_sensitive_detail(message)
        {
            return Err(ContentSchemaError::Invariant(
                "target unavailable message must be a short sanitized sentence".into(),
            ));
        }
        if let Some(field) = &self.invalid_field {
            if field.trim().is_empty()
                || field.chars().count() > MAX_UNAVAILABLE_FIELD_CHARS
                || field.chars().any(char::is_control)
                || contains_sensitive_detail(field)
            {
                return Err(ContentSchemaError::Invariant(
                    "target unavailable field must be short and sanitized".into(),
                ));
            }
        }
        Ok(())
    }
}

/// The six v0.6 content kinds. `ShortSummary`/`LongSummary` are the two
/// permanently optional summary tiers added by schema v2; the other four are
/// the original v1 kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactKindV1 {
    FaithfulText,
    Chapters,
    DefaultSummary,
    Highlights,
    ShortSummary,
    LongSummary,
}

impl ArtifactKindV1 {
    /// The four initial-pipeline kinds: everything a settled current must
    /// cover. The summary-tier slots are on-demand only and never required.
    #[cfg(test)]
    pub(crate) const INITIAL_ALL: [Self; 4] = [
        Self::FaithfulText,
        Self::DefaultSummary,
        Self::Highlights,
        Self::Chapters,
    ];

    /// True for the two permanently optional tiers. The standard summary
    /// stays in the initial pipeline's required enhancement set; only
    /// short/long are optional and never required by `validate()`.
    pub(crate) fn is_summary_tier(self) -> bool {
        matches!(self, Self::ShortSummary | Self::LongSummary)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceContextSnapshotV1 {
    pub(crate) main_title: String,
    #[serde(default)]
    pub(crate) part_title: Option<String>,
    #[serde(default)]
    pub(crate) terms: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EvidenceDescriptorV1 {
    pub(crate) identity: String,
    pub(crate) job_id: Uuid,
    pub(crate) platform: String,
    pub(crate) bvid: String,
    pub(crate) page: u32,
    pub(crate) cid: u64,
    pub(crate) duration_ms: u64,
    pub(crate) raw_sha256: String,
    pub(crate) utterance_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EvidenceIdentityMaterialV1 {
    pub(crate) domain: String,
    pub(crate) job_id: Uuid,
    pub(crate) platform: String,
    pub(crate) bvid: String,
    pub(crate) page: u32,
    pub(crate) cid: u64,
    pub(crate) duration_ms: u64,
    pub(crate) raw_sha256: String,
    pub(crate) utterance_count: usize,
}

impl EvidenceDescriptorV1 {
    pub(crate) fn validate(&self) -> Result<(), ContentSchemaError> {
        if self.platform != EVIDENCE_PLATFORM {
            return Err(ContentSchemaError::Invariant(format!(
                "unsupported evidence platform {:?}",
                self.platform
            )));
        }
        validate_sha256_hex("identity", &self.identity)?;
        validate_sha256_hex("raw_sha256", &self.raw_sha256)?;
        if self.bvid.trim().is_empty() || self.page == 0 || self.cid == 0 {
            return Err(ContentSchemaError::Invariant(
                "evidence source unit is incomplete".into(),
            ));
        }
        if self.recompute_identity()? != self.identity {
            return Err(ContentSchemaError::Invariant(
                "evidence identity does not match its fixed material".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn identity_material(&self) -> EvidenceIdentityMaterialV1 {
        EvidenceIdentityMaterialV1 {
            domain: "bimyscribe/evidence/v1".into(),
            job_id: self.job_id,
            platform: self.platform.clone(),
            bvid: self.bvid.clone(),
            page: self.page,
            cid: self.cid,
            duration_ms: self.duration_ms,
            raw_sha256: self.raw_sha256.clone(),
            utterance_count: self.utterance_count,
        }
    }

    /// Recompute identity only from a canonical descriptor. Raw bytes are
    /// verified by the Evidence loader in the next issue; this boundary still
    /// rejects an uppercase/non-hex raw digest before it can enter identity
    /// material, so callers cannot accidentally create two identities for the
    /// same digest spelling.
    pub(crate) fn recompute_identity(&self) -> Result<String, ContentSchemaError> {
        validate_sha256_hex("raw_sha256", &self.raw_sha256)?;
        let material = serde_json::to_vec(&self.identity_material())
            .map_err(|error| ContentSchemaError::Json(error.to_string()))?;
        Ok(sha256_hex(&material))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ArtifactSlotV1 {
    #[serde(default)]
    pub(crate) current: Option<DerivationRecordV1>,
    #[serde(default)]
    pub(crate) last_failure: Option<DerivationFailureV1>,
}

impl ArtifactSlotV1 {
    pub(crate) fn is_settled(&self) -> bool {
        self.current.is_some() || self.last_failure.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DerivationRecordV1 {
    pub(crate) schema_version: u32,
    pub(crate) revision: u32,
    pub(crate) kind: ArtifactKindV1,
    pub(crate) evidence_identity: String,
    pub(crate) source_context: SourceContextSnapshotV1,
    pub(crate) provenance: DerivationProvenanceV1,
    pub(crate) blocks: Vec<ContentBlockV1>,
    pub(crate) validation: ValidationReportV1,
    pub(crate) created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DerivationProvenanceV1 {
    pub(crate) recipe_id: String,
    pub(crate) recipe_version: u32,
    #[serde(default)]
    pub(crate) prompt_id: Option<String>,
    #[serde(default)]
    pub(crate) prompt_version: Option<u32>,
    pub(crate) rules_id: String,
    pub(crate) rules_version: u32,
    #[serde(default)]
    pub(crate) provider: Option<GenerationProviderV1>,
    #[serde(default)]
    pub(crate) api_format: Option<GenerationApiFormatV1>,
    #[serde(default)]
    pub(crate) endpoint: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    pub(crate) parameters: Option<GenerationParametersV1>,
    pub(crate) effective_path: EffectivePathV1,
    #[serde(default)]
    pub(crate) fallback_reason: Option<FallbackReasonV1>,
}

impl DerivationProvenanceV1 {
    fn validate(&self, kind: ArtifactKindV1) -> Result<(), ContentSchemaError> {
        let expected_recipe = recipe_id(kind);
        if self.recipe_id != expected_recipe || self.rules_id != expected_recipe {
            return Err(ContentSchemaError::Invariant(
                "provenance recipe and rules ids do not match the artifact kind".into(),
            ));
        }
        if self.recipe_version != RECIPE_VERSION || self.rules_version != RULES_VERSION {
            return Err(ContentSchemaError::Invariant(
                "provenance recipe or rules version is not the fixed version".into(),
            ));
        }

        match (
            &self.provider,
            &self.api_format,
            &self.endpoint,
            &self.model,
            &self.parameters,
        ) {
            (None, None, None, None, None) => {}
            (
                Some(GenerationProviderV1::OpenAiCompatible),
                Some(GenerationApiFormatV1::OpenAiChatCompletions),
                Some(endpoint),
                Some(model),
                Some(GenerationParametersV1::OpenAiChatCompletions { .. }),
            )
            | (
                Some(GenerationProviderV1::AnthropicCompatible),
                Some(GenerationApiFormatV1::AnthropicMessages),
                Some(endpoint),
                Some(model),
                Some(GenerationParametersV1::AnthropicMessages { .. }),
            ) => {
                require_canonical_local_endpoint(endpoint)?;
                validate_canonical_model(model)?;
                if model.is_empty() {
                    return Err(ContentSchemaError::Invariant(
                        "provenance target endpoint and model must not be empty".into(),
                    ));
                }
            }
            _ => {
                return Err(ContentSchemaError::Invariant(
                    "provenance target and typed parameters are incompatible".into(),
                ));
            }
        }
        if let Some(parameters) = &self.parameters {
            parameters.validate()?;
        }
        if let Some(reason) = &self.fallback_reason {
            reason.validate()?;
        }
        if let Some(prompt_id) = &self.prompt_id {
            if prompt_id != expected_recipe || self.prompt_version != Some(PROMPT_VERSION) {
                return Err(ContentSchemaError::Invariant(
                    "provenance prompt identity does not match the fixed recipe".into(),
                ));
            }
        } else if self.prompt_version.is_some() {
            return Err(ContentSchemaError::Invariant(
                "provenance prompt version requires a prompt identity".into(),
            ));
        }
        match self.effective_path {
            EffectivePathV1::Rules
                if self.provider.is_some()
                    || self.api_format.is_some()
                    || self.endpoint.is_some()
                    || self.model.is_some()
                    || self.parameters.is_some()
                    || self.prompt_id.is_some()
                    || self.prompt_version.is_some()
                    || self.fallback_reason.is_some() =>
            {
                return Err(ContentSchemaError::Invariant(
                    "rules provenance must not carry an AI target or fallback reason".into(),
                ));
            }
            EffectivePathV1::Llm
                if self.fallback_reason.is_some()
                    || self.prompt_id.is_none()
                    || self.provider.is_none() =>
            {
                return Err(ContentSchemaError::Invariant(
                    "llm provenance must carry the fixed prompt and target".into(),
                ));
            }
            EffectivePathV1::RulesFallback => {
                let Some(reason) = &self.fallback_reason else {
                    return Err(ContentSchemaError::Invariant(
                        "rules fallback provenance must carry a fallback reason".into(),
                    ));
                };
                let target_present = self.provider.is_some()
                    && self.api_format.is_some()
                    && self.endpoint.is_some()
                    && self.model.is_some()
                    && self.parameters.is_some();
                match (reason.code, target_present, self.prompt_id.is_some()) {
                    (FallbackReasonCodeV1::TargetUnavailable, false, false)
                    | (FallbackReasonCodeV1::GenerationFailed, true, true)
                    | (FallbackReasonCodeV1::ResponseInvalid, true, true) => {}
                    _ => {
                        return Err(ContentSchemaError::Invariant(
                            "fallback provenance target does not match its reason".into(),
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn provenance_target_matches(
    provenance: &DerivationProvenanceV1,
    target: &ReadyGenerationTargetV1,
) -> bool {
    provenance.provider == Some(target.provider)
        && provenance.api_format == Some(target.api_format)
        && provenance.endpoint.as_deref() == Some(target.endpoint.as_str())
        && provenance.model.as_deref() == Some(target.model.as_str())
        && provenance.parameters.as_ref() == Some(&target.parameters)
}

fn validate_record_provenance_for_setup(
    kind: ArtifactKindV1,
    provenance: &DerivationProvenanceV1,
    setup: &ContentSetupV1,
) -> Result<(), ContentSchemaError> {
    if kind != ArtifactKindV1::FaithfulText {
        if provenance.effective_path != EffectivePathV1::Llm {
            return Err(ContentSchemaError::Invariant(
                "enhancement current must use the LLM path".into(),
            ));
        }
        return Ok(());
    }
    match &setup.initial_target {
        InitialTargetV1::Disabled => {
            if provenance.effective_path != EffectivePathV1::Rules {
                return Err(ContentSchemaError::Invariant(
                    "disabled faithful current must use rules".into(),
                ));
            }
        }
        InitialTargetV1::Unavailable(target) => {
            let reason = provenance.fallback_reason.as_ref();
            if provenance.effective_path != EffectivePathV1::RulesFallback
                || provenance.provider.is_some()
                || reason.is_none_or(|reason| {
                    reason.code != FallbackReasonCodeV1::TargetUnavailable
                        || reason.message != target.message
                })
            {
                return Err(ContentSchemaError::Invariant(
                    "unavailable faithful current must be the matching target fallback".into(),
                ));
            }
        }
        InitialTargetV1::Ready(target) => {
            if provenance.effective_path == EffectivePathV1::Rules
                || (provenance.effective_path == EffectivePathV1::Llm
                    && !provenance_target_matches(provenance, target))
                || (provenance.effective_path == EffectivePathV1::RulesFallback
                    && (!provenance_target_matches(provenance, target)
                        || !matches!(
                            provenance
                                .fallback_reason
                                .as_ref()
                                .map(|reason| reason.code),
                            Some(FallbackReasonCodeV1::GenerationFailed)
                                | Some(FallbackReasonCodeV1::ResponseInvalid)
                        )))
            {
                return Err(ContentSchemaError::Invariant(
                    "ready faithful current target/path does not match frozen setup".into(),
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectivePathV1 {
    Rules,
    Llm,
    RulesFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FallbackReasonV1 {
    pub(crate) code: FallbackReasonCodeV1,
    pub(crate) message: String,
}

impl FallbackReasonV1 {
    fn validate(&self) -> Result<(), ContentSchemaError> {
        let message = self.message.trim();
        if message.is_empty()
            || message.chars().count() > MAX_UNAVAILABLE_MESSAGE_CHARS
            || message.chars().any(char::is_control)
            || contains_sensitive_detail(message)
        {
            return Err(ContentSchemaError::Invariant(
                "fallback reason must be a short sanitized sentence".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FallbackReasonCodeV1 {
    TargetUnavailable,
    GenerationFailed,
    ResponseInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContentBlockV1 {
    pub(crate) id: String,
    pub(crate) role: ContentBlockRoleV1,
    #[serde(default)]
    pub(crate) title: Option<String>,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) source_refs: Vec<SourceRefV1>,
    pub(crate) source_status: SourceStatusV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContentBlockRoleV1 {
    Paragraph,
    Summary,
    Highlight,
    Chapter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceRefV1 {
    pub(crate) utterance_ids: Vec<String>,
    pub(crate) start_ms: u64,
    pub(crate) end_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceStatusV1 {
    Mapped,
    Limited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ValidationReportV1 {
    pub(crate) processed_utterance_count: usize,
    pub(crate) total_utterance_count: usize,
    pub(crate) processing_coverage_complete: bool,
    #[serde(default)]
    pub(crate) processing_chunks: Vec<ProcessingChunkV1>,
    pub(crate) record_source_status: SourceStatusV1,
    #[serde(default)]
    pub(crate) checks: Vec<ValidationCheckV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessingChunkV1 {
    pub(crate) chunk_index: usize,
    pub(crate) input_utterance_ids: Vec<String>,
    pub(crate) output_block_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ValidationCheckV1 {
    pub(crate) name: String,
    pub(crate) passed: bool,
    #[serde(default)]
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DerivationFailureV1 {
    pub(crate) code: FailureCodeV1,
    pub(crate) message: String,
    pub(crate) retryable: bool,
    pub(crate) occurred_at: DateTime<Utc>,
}

impl DerivationFailureV1 {
    fn validate(&self) -> Result<(), ContentSchemaError> {
        let message = self.message.trim();
        if message.is_empty()
            || message.chars().count() > MAX_UNAVAILABLE_MESSAGE_CHARS
            || message.chars().any(char::is_control)
            || contains_sensitive_detail(message)
        {
            return Err(ContentSchemaError::Invariant(
                "derivation failure must be a short sanitized sentence".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureCodeV1 {
    TargetUnavailable,
    GenerationFailed,
    ResponseInvalid,
}

/// The on-disk current. All four slots are explicit fields so the schema is
/// closed and stable; unknown artifact kinds fail during serde decoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ContentCurrentV1 {
    pub(crate) schema_version: u32,
    pub(crate) job_id: Uuid,
    pub(crate) evidence: EvidenceDescriptorV1,
    pub(crate) source_context: SourceContextSnapshotV1,
    pub(crate) setup: ContentSetupV1,
    pub(crate) slots: ContentSlotsV1,
}

impl ContentCurrentV1 {
    pub(crate) fn validate(&self) -> Result<(), ContentSchemaError> {
        if !is_supported_content_schema_version(self.schema_version) {
            return Err(ContentSchemaError::SchemaVersion {
                expected: CONTENT_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }
        self.evidence.validate()?;
        self.setup.validate()?;
        if self.job_id != self.evidence.job_id {
            return Err(ContentSchemaError::Invariant(
                "current job_id does not match evidence job_id".into(),
            ));
        }
        self.slots.validate(&self.setup)?;
        for (kind, slot) in self.slots.iter() {
            if let Some(record) = &slot.current {
                if !is_supported_content_schema_version(record.schema_version) {
                    return Err(ContentSchemaError::SchemaVersion {
                        expected: CONTENT_SCHEMA_VERSION,
                        actual: record.schema_version,
                    });
                }
                if record.kind != kind {
                    return Err(ContentSchemaError::Invariant(format!(
                        "record kind mismatch for {kind:?}"
                    )));
                }
                if record.revision == 0 {
                    return Err(ContentSchemaError::Invariant(format!(
                        "record revision must be positive for {kind:?}"
                    )));
                }
                if record.evidence_identity != self.evidence.identity {
                    return Err(ContentSchemaError::Invariant(format!(
                        "record evidence identity mismatch for {kind:?}"
                    )));
                }
                if record.source_context != self.source_context {
                    return Err(ContentSchemaError::Invariant(format!(
                        "record source context mismatch for {kind:?}"
                    )));
                }
                record.provenance.validate(record.kind)?;
                validate_record_provenance_for_setup(record.kind, &record.provenance, &self.setup)?;
                validate_record_shape(record)?;
            }
            if let Some(failure) = &slot.last_failure {
                failure.validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ContentSlotsV1 {
    pub(crate) faithful_text: ArtifactSlotV1,
    pub(crate) default_summary: ArtifactSlotV1,
    pub(crate) highlights: ArtifactSlotV1,
    pub(crate) chapters: ArtifactSlotV1,
    /// v2 summary tiers. `#[serde(default)]` lets a v1 byte stream decode
    /// with these slots empty; they are permanently optional and are never
    /// generated by the initial pipeline or Pipeline Retry.
    #[serde(default)]
    pub(crate) short_summary: ArtifactSlotV1,
    #[serde(default)]
    pub(crate) long_summary: ArtifactSlotV1,
}

impl ContentSlotsV1 {
    pub(crate) fn validate(&self, setup: &ContentSetupV1) -> Result<(), ContentSchemaError> {
        if self.faithful_text.current.is_none() {
            return Err(ContentSchemaError::Invariant(
                "published current must contain faithful_text".into(),
            ));
        }
        for (kind, slot) in self.iter() {
            if kind == ArtifactKindV1::FaithfulText {
                continue;
            }
            // Summary tiers are permanently optional: an empty tier slot is
            // settled by definition and never counts as incomplete.
            if kind.is_summary_tier() {
                continue;
            }
            if !setup.enhancements_requested && slot.is_settled() {
                return Err(ContentSchemaError::Invariant(format!(
                    "enhancement slot {kind:?} is populated while enhancements are disabled"
                )));
            }
            if setup.enhancements_requested && !slot.is_settled() {
                return Err(ContentSchemaError::Invariant(format!(
                    "requested enhancement slot {kind:?} is unsettled"
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn get(&self, kind: ArtifactKindV1) -> &ArtifactSlotV1 {
        match kind {
            ArtifactKindV1::FaithfulText => &self.faithful_text,
            ArtifactKindV1::DefaultSummary => &self.default_summary,
            ArtifactKindV1::Highlights => &self.highlights,
            ArtifactKindV1::Chapters => &self.chapters,
            ArtifactKindV1::ShortSummary => &self.short_summary,
            ArtifactKindV1::LongSummary => &self.long_summary,
        }
    }

    pub(crate) fn get_mut(&mut self, kind: ArtifactKindV1) -> &mut ArtifactSlotV1 {
        match kind {
            ArtifactKindV1::FaithfulText => &mut self.faithful_text,
            ArtifactKindV1::DefaultSummary => &mut self.default_summary,
            ArtifactKindV1::Highlights => &mut self.highlights,
            ArtifactKindV1::Chapters => &mut self.chapters,
            ArtifactKindV1::ShortSummary => &mut self.short_summary,
            ArtifactKindV1::LongSummary => &mut self.long_summary,
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (ArtifactKindV1, &ArtifactSlotV1)> {
        [
            (ArtifactKindV1::FaithfulText, &self.faithful_text),
            (ArtifactKindV1::DefaultSummary, &self.default_summary),
            (ArtifactKindV1::Highlights, &self.highlights),
            (ArtifactKindV1::Chapters, &self.chapters),
            (ArtifactKindV1::ShortSummary, &self.short_summary),
            (ArtifactKindV1::LongSummary, &self.long_summary),
        ]
        .into_iter()
    }

    pub(crate) fn iter_mut(
        &mut self,
    ) -> impl Iterator<Item = (ArtifactKindV1, &mut ArtifactSlotV1)> {
        [
            (ArtifactKindV1::FaithfulText, &mut self.faithful_text),
            (ArtifactKindV1::DefaultSummary, &mut self.default_summary),
            (ArtifactKindV1::Highlights, &mut self.highlights),
            (ArtifactKindV1::Chapters, &mut self.chapters),
            (ArtifactKindV1::ShortSummary, &mut self.short_summary),
            (ArtifactKindV1::LongSummary, &mut self.long_summary),
        ]
        .into_iter()
    }

    pub(crate) fn iter_mut_compat(
        &mut self,
    ) -> impl Iterator<Item = (ArtifactKindV1, &mut ArtifactSlotV1)> {
        self.iter_mut()
    }
}

/// Validated raw Evidence is kept in memory only; raw utterance text is not
/// duplicated in `content-current.v1.json`.
#[derive(Debug, Clone)]
pub(crate) struct ValidatedEvidenceViewV1 {
    pub(crate) descriptor: EvidenceDescriptorV1,
    pub(crate) utterances: Vec<crate::funasr::Utterance>,
}

#[derive(Debug, Clone)]
pub(crate) struct ContentSnapshotV1 {
    pub(crate) current: ContentCurrentV1,
    pub(crate) evidence: ValidatedEvidenceViewV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CurrentIssueV1 {
    NotGenerated,
    Corrupt,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // Read-only snapshots stay value-typed at this crate-private boundary.
pub(crate) enum ContentViewV1 {
    Legacy,
    RawOnly {
        evidence: ValidatedEvidenceViewV1,
        current_issue: CurrentIssueV1,
    },
    Current(ContentSnapshotV1),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegenerableKind {
    DefaultSummary,
    Highlights,
    Chapters,
    ShortSummary,
    LongSummary,
}

impl RegenerableKind {
    pub(crate) fn artifact_kind(self) -> ArtifactKindV1 {
        match self {
            Self::DefaultSummary => ArtifactKindV1::DefaultSummary,
            Self::Highlights => ArtifactKindV1::Highlights,
            Self::Chapters => ArtifactKindV1::Chapters,
            Self::ShortSummary => ArtifactKindV1::ShortSummary,
            Self::LongSummary => ArtifactKindV1::LongSummary,
        }
    }

    /// Parse the UI action key. The standard summary keeps its historical
    /// `"summary"` wire name; the tiers add `"summary-short"`/`"summary-long"`.
    pub(crate) fn from_action(action: &str) -> Option<Self> {
        match action {
            "summary" => Some(Self::DefaultSummary),
            "summary-short" => Some(Self::ShortSummary),
            "summary-long" => Some(Self::LongSummary),
            "highlights" => Some(Self::Highlights),
            "chapters" => Some(Self::Chapters),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Intent {
    Initial,
    Regenerate {
        kind: RegenerableKind,
        target: ReadyGenerationTargetV1,
    },
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ContentError {
    #[error("content schema: {0}")]
    Schema(#[from] ContentSchemaError),
    #[error("content I/O: {0}")]
    Io(#[from] io::Error),
    #[error("content legacy job")]
    Legacy,
    #[error("content Evidence is invalid: {0}")]
    Evidence(String),
    #[error("content operation cancelled")]
    Cancelled,
    #[error("content intent is invalid: {0}")]
    InvalidIntent(String),
    #[error("content generation failed: {0}")]
    Generation(String),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ContentSchemaError {
    #[error("unsupported schema version: expected {expected}, got {actual}")]
    SchemaVersion { expected: u32, actual: u32 },
    #[error("unsupported recipe set version: expected {expected}, got {actual}")]
    RecipeSetVersion { expected: u32, actual: u32 },
    #[error("invalid content invariant: {0}")]
    Invariant(String),
    #[error("invalid current JSON: {0}")]
    Json(String),
}

/// Path-bound private store. There is exactly one canonical filename and no
/// record directory or pointer layer.
struct ContentStore<'a> {
    work_dir: &'a Path,
}

impl<'a> ContentStore<'a> {
    fn new(work_dir: &'a Path) -> Result<Self, ContentError> {
        if !work_dir.is_dir() {
            return Err(ContentError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "content work directory is not a directory: {}",
                    work_dir.display()
                ),
            )));
        }
        Ok(Self { work_dir })
    }

    fn path(&self) -> PathBuf {
        self.work_dir.join(CONTENT_CURRENT_FILE)
    }

    fn load(&self) -> Result<Option<ContentCurrentV1>, ContentError> {
        let path = self.path();
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ContentError::Io(error)),
        };
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        let current = serde_json::from_slice::<ContentCurrentV1>(&data)
            .map_err(|error| ContentSchemaError::Json(error.to_string()))?;
        current.validate()?;
        Ok(Some(current))
    }

    /// Commit one complete, already-validated snapshot. The temporary file is
    /// colocated with the target, synced, read back and validated before rename.
    fn save(&self, current: &ContentCurrentV1) -> Result<(), ContentError> {
        current.validate()?;
        let path = self.path();
        let tmp = self.work_dir.join(format!("{CONTENT_CURRENT_FILE}.tmp"));
        let data = serde_json::to_vec_pretty(current)
            .map_err(|error| ContentSchemaError::Json(error.to_string()))?;
        let result = (|| -> Result<(), ContentError> {
            let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
            file.write_all(&data)?;
            file.sync_all()?;
            drop(file);

            let mut reread = Vec::new();
            File::open(&tmp)?.read_to_end(&mut reread)?;
            let decoded = serde_json::from_slice::<ContentCurrentV1>(&reread)
                .map_err(|error| ContentSchemaError::Json(error.to_string()))?;
            decoded.validate()?;
            // Rename is the commit point. A parent-directory fsync failure
            // cannot undo the now-visible new current; retain that fact and
            // expose only a sanitized warning.
            fs::rename(&tmp, &path)?;
            if let Err(error) = sync_parent(self.work_dir) {
                log::warn!(
                    "content current committed but parent sync failed for {}: {}",
                    self.work_dir.display(),
                    error
                );
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Recovery is explicit; ordinary `load` never writes or cleans up a
    /// temporary file. Execute paths may call this before their next commit.
    fn cleanup_temp(&self) -> io::Result<()> {
        let tmp = self.work_dir.join(format!("{CONTENT_CURRENT_FILE}.tmp"));
        match fs::remove_file(tmp) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn sync_parent(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

const CHUNK_MAX_UTTERANCES: usize = 20;
const CHUNK_MAX_CHARS: usize = 6_000;
const RECIPE_VERSION: u32 = 1;
const PROMPT_VERSION: u32 = 1;
const RULES_VERSION: u32 = 1;
const HTTP_CONTENT_TYPE: &str = "application/json";
const HTTP_USER_AGENT: &str = "bimyscribe/0.5";

/// A request is assembled entirely by `ContentResults` before it reaches an
/// adapter. The adapter cannot inspect Config or choose a recipe/default.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
struct GenerationRequest {
    kind: ArtifactKindV1,
    chunk_index: usize,
    chunk_count: usize,
    utterances: Vec<Utterance>,
    prompt: String,
}

#[derive(Debug, Clone)]
struct GeneratedChunk {
    processed_utterance_ids: Vec<String>,
    blocks: Vec<GeneratedBlock>,
}

/// A chunk is not a publishable artifact. This ledger records the exact input
/// IDs consumed by one response so the final merge can prove that every chunk
/// arrived once, in order, before any record is built.
#[derive(Debug, Clone)]
struct ChunkResult {
    chunk_index: usize,
    input_ids: Vec<String>,
    blocks: Vec<ContentBlockV1>,
}

#[derive(Debug, Clone)]
struct GeneratedBlock {
    id: String,
    title: Option<String>,
    text: String,
    declared_role: Option<ContentBlockRoleV1>,
    source_refs: Vec<SourceRefV1>,
    declared_status: Option<SourceStatusV1>,
}

#[derive(Debug, thiserror::Error)]
enum GenerationPortError {
    #[error("transport failed")]
    Transport,
    #[error("response is invalid")]
    ResponseInvalid,
}

/// The only model seam in this module. It is deliberately private: the
/// Pipeline and UI express an Intent, never a provider or a prompt.
trait GenerationPort {
    fn generate(
        &self,
        target: &ReadyGenerationTargetV1,
        request: &GenerationRequest,
    ) -> Result<GeneratedChunk, GenerationPortError>;
}

struct HttpGenerationPort;

impl GenerationPort for HttpGenerationPort {
    fn generate(
        &self,
        target: &ReadyGenerationTargetV1,
        request: &GenerationRequest,
    ) -> Result<GeneratedChunk, GenerationPortError> {
        match target.api_format {
            GenerationApiFormatV1::OpenAiChatCompletions => {
                OpenAiGenerationAdapter.generate(target, request)
            }
            GenerationApiFormatV1::AnthropicMessages => {
                AnthropicGenerationAdapter.generate(target, request)
            }
        }
    }
}

struct OpenAiGenerationAdapter;
struct AnthropicGenerationAdapter;

impl GenerationPort for OpenAiGenerationAdapter {
    fn generate(
        &self,
        target: &ReadyGenerationTargetV1,
        request: &GenerationRequest,
    ) -> Result<GeneratedChunk, GenerationPortError> {
        http_generate(
            target,
            request,
            GenerationApiFormatV1::OpenAiChatCompletions,
        )
    }
}

impl GenerationPort for AnthropicGenerationAdapter {
    fn generate(
        &self,
        target: &ReadyGenerationTargetV1,
        request: &GenerationRequest,
    ) -> Result<GeneratedChunk, GenerationPortError> {
        http_generate(target, request, GenerationApiFormatV1::AnthropicMessages)
    }
}

fn http_generate(
    target: &ReadyGenerationTargetV1,
    request: &GenerationRequest,
    api_format: GenerationApiFormatV1,
) -> Result<GeneratedChunk, GenerationPortError> {
    if target.api_format != api_format {
        return Err(GenerationPortError::ResponseInvalid);
    }
    target
        .validate()
        .map_err(|_| GenerationPortError::ResponseInvalid)?;
    let url = request_url(target, api_format);
    let body = request_body(target, request);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(120))
        .redirects(0)
        .build();
    let response = agent
        .post(&url)
        .set("Content-Type", HTTP_CONTENT_TYPE)
        .set("User-Agent", HTTP_USER_AGENT)
        .send_string(&body.to_string())
        .map_err(|_| GenerationPortError::Transport)?;
    let value: serde_json::Value = serde_json::from_reader(response.into_reader())
        .map_err(|_| GenerationPortError::ResponseInvalid)?;
    let text = match api_format {
        GenerationApiFormatV1::OpenAiChatCompletions => value
            .get("choices")
            .and_then(|choices| choices.as_array())
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_str())
            .map(str::to_owned)
            .ok_or(GenerationPortError::ResponseInvalid)?,
        GenerationApiFormatV1::AnthropicMessages => value
            .get("content")
            .and_then(|content| content.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| {
                        (block.get("type").and_then(|v| v.as_str()) == Some("text"))
                            .then(|| block.get("text").and_then(|v| v.as_str()))
                            .flatten()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .ok_or(GenerationPortError::ResponseInvalid)?,
    };
    parse_generated_chunk(&text)
}

fn api_path(api_format: GenerationApiFormatV1) -> &'static str {
    match api_format {
        GenerationApiFormatV1::OpenAiChatCompletions => "/v1/chat/completions",
        GenerationApiFormatV1::AnthropicMessages => "/v1/messages",
    }
}

// ---- v0.6 F1: connection test ----

/// Overall probe deadline. Deliberately separate from the generation path's
/// connect-10s/read-120s budget: the test is a diagnosis, not a derivation.
const CONNECTION_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectionTestSuccess {
    pub(crate) api_format: GenerationApiFormatV1,
    pub(crate) model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionTestFailure {
    InvalidAddress,
    Unreachable,
    ProtocolMismatch,
    ModelUnavailable,
    Timeout,
}

impl ConnectionTestFailure {
    /// Fixed user-facing copy (Design Plan §3.2). `InvalidAddress` covers only
    /// the pre-send validation; the other four come from the probe. The model
    /// suffix is applied by the settings UI so the enum stays side-effect-free.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::InvalidAddress => "地址无效：请检查格式，例如 http://127.0.0.1:11434/v1。",
            Self::Unreachable => "无法连接：请确认本地服务已启动，且地址与端口正确。",
            Self::ProtocolMismatch => {
                "服务已响应，但不符合所选 API 格式：请确认 API 格式与地址匹配。"
            }
            Self::ModelUnavailable => {
                "服务可用，但模型不可用：请确认模型已拉取或加载，且名称一致。"
            }
            Self::Timeout => "连接超时：服务可能正在加载模型，请稍后重试。",
        }
    }

    /// Insert `model` into the `ModelUnavailable` template, which contains a
    /// quoted model placeholder. Other failures keep this plain text.
    pub(crate) fn with_model(self, model: &str) -> String {
        if self == Self::ModelUnavailable {
            return format!(
                "服务可用，但模型“{model}”不可用：请确认模型已拉取或加载，且名称一致。"
            );
        }
        self.message().to_string()
    }
}

/// One classified socket observation, kept separate from the classification so
/// listener-free unit tests can drive every branch.
#[derive(Debug, Clone, PartialEq)]
enum ProbeOutcome {
    /// A protocol-shaped success body for the selected format.
    Success,
    /// HTTP status with the raw body (bounded read).
    HttpError(u16, String),
    /// connect/DNS/other io transport failure.
    Transport,
    /// Deadline elapsed before any response completed.
    Timeout,
    /// 2xx but body does not parse as the selected protocol shape.
    MalformedBody,
}

/// Pure classification (Spec §5.1). Never matches on error-message text; only
/// on structured shapes and status codes.
fn classify_probe(outcome: ProbeOutcome) -> Result<(), ConnectionTestFailure> {
    match outcome {
        ProbeOutcome::Success => Ok(()),
        ProbeOutcome::Transport => Err(ConnectionTestFailure::Unreachable),
        ProbeOutcome::Timeout => Err(ConnectionTestFailure::Timeout),
        ProbeOutcome::MalformedBody => Err(ConnectionTestFailure::ProtocolMismatch),
        ProbeOutcome::HttpError(status, body) => {
            let parsed: Option<serde_json::Value> = serde_json::from_str(&body).ok();
            if is_model_missing_error(status, parsed.as_ref()) {
                return Err(ConnectionTestFailure::ModelUnavailable);
            }
            if status == 404 && parsed.is_none() {
                // Protocol path 404 without a protocol-shaped error body.
                return Err(ConnectionTestFailure::ProtocolMismatch);
            }
            if (200..300).contains(&status) {
                return Err(ConnectionTestFailure::ProtocolMismatch);
            }
            // Remaining 4xx/5xx cannot be reliably attributed: the service
            // answered but not in the selected format. No sixth category.
            Err(ConnectionTestFailure::ProtocolMismatch)
        }
    }
}

/// Detect the two protocol-level model-missing shapes:
/// - OpenAI-compatible: `model_not_found`, or a 404 whose error body names the model;
/// - Anthropic-compatible: `not_found_error`.
fn is_model_missing_error(status: u16, body: Option<&serde_json::Value>) -> bool {
    let Some(body) = body else {
        return false;
    };
    let error_type = body
        .get("error")
        .and_then(|error| error.get("type"))
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let code = body
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if error_type == "not_found_error" || code == "model_not_found" {
        return true;
    }
    let message = body
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if status == 404
        && (message.contains("model")
            && (message.contains("not found")
                || message.contains("does not exist")
                || message.contains("unknown model")))
    {
        return true;
    }
    false
}

/// Whether a response body parses as the selected protocol's success shape.
fn body_is_protocol_success(api_format: GenerationApiFormatV1, value: &serde_json::Value) -> bool {
    match api_format {
        GenerationApiFormatV1::OpenAiChatCompletions => value
            .get("choices")
            .and_then(|choices| choices.as_array())
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .is_some(),
        GenerationApiFormatV1::AnthropicMessages => value
            .get("content")
            .and_then(|content| content.as_array())
            .is_some(),
    }
}

fn probe_request_body(target: &ReadyGenerationTargetV1) -> serde_json::Value {
    const PING_PROMPT: &str = "Reply with exactly one word: ping";
    match target.api_format {
        GenerationApiFormatV1::OpenAiChatCompletions => serde_json::json!({
            "model": target.model,
            "messages": [{"role": "user", "content": PING_PROMPT}],
            "temperature": 0.2,
            "max_tokens": 8,
        }),
        GenerationApiFormatV1::AnthropicMessages => serde_json::json!({
            "model": target.model,
            "max_tokens": GenerationParametersV1::anthropic_default().max_tokens_for_probe(),
            "messages": [{"role": "user", "content": PING_PROMPT}],
        }),
    }
}

/// Run one minimal generation request against the draft target and classify
/// the outcome into one of five fixed failure classes. The probe writes no
/// provenance and touches no slot; it reuses this module's endpoint
/// canonicalization and adapter URL/body construction.
pub(crate) fn test_connection(
    target: &ReadyGenerationTargetV1,
) -> Result<ConnectionTestSuccess, ConnectionTestFailure> {
    // Address validation happens strictly before any byte is sent.
    if target.validate().is_err() {
        return Err(ConnectionTestFailure::InvalidAddress);
    }

    let url = request_url(target, target.api_format);
    let body = probe_request_body(target);
    let agent = ureq::AgentBuilder::new()
        .timeout(CONNECTION_TEST_TIMEOUT)
        .redirects(0)
        .build();
    let response = match agent
        .post(&url)
        .set("Content-Type", HTTP_CONTENT_TYPE)
        .set("User-Agent", HTTP_USER_AGENT)
        .send_string(&body.to_string())
    {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let mut text = String::new();
            let _ = response
                .into_reader()
                .take(64 * 1024)
                .read_to_string(&mut text);
            return classify_probe(ProbeOutcome::HttpError(status, text)).map(|()| {
                ConnectionTestSuccess {
                    api_format: target.api_format,
                    model: target.model.clone(),
                }
            });
        }
        Err(ureq::Error::Transport(error)) => {
            let is_timeout = error.kind() == ureq::ErrorKind::Io
                && error.to_string().to_ascii_lowercase().contains("timed out");
            let outcome = if is_timeout {
                ProbeOutcome::Timeout
            } else {
                ProbeOutcome::Transport
            };
            return classify_probe(outcome).map(|()| ConnectionTestSuccess {
                api_format: target.api_format,
                model: target.model.clone(),
            });
        }
    };

    // 2xx: parse and verify the selected protocol shape.
    let mut text = String::new();
    let outcome = match response
        .into_reader()
        .take(1024 * 1024)
        .read_to_string(&mut text)
    {
        Ok(_) => {
            let value: Option<serde_json::Value> = serde_json::from_str(&text).ok();
            match &value {
                Some(value) if body_is_protocol_success(target.api_format, value) => {
                    ProbeOutcome::Success
                }
                _ => ProbeOutcome::MalformedBody,
            }
        }
        Err(err) => {
            if err.kind() == std::io::ErrorKind::TimedOut
                || err.to_string().to_lowercase().contains("timed out")
                || err.to_string().to_lowercase().contains("timeout")
            {
                ProbeOutcome::Timeout
            } else {
                ProbeOutcome::MalformedBody
            }
        }
    };
    classify_probe(outcome).map(|()| ConnectionTestSuccess {
        api_format: target.api_format,
        model: target.model.clone(),
    })
}

fn request_url(target: &ReadyGenerationTargetV1, api_format: GenerationApiFormatV1) -> String {
    format!("{}{}", target.endpoint, api_path(api_format))
}

// ---- v0.6 F4: single-task search ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchQuery {
    TimeRange(u64, u64),
    Speaker(String),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchLayer {
    RawTranscript,
    FaithfulText,
    Speaker,
}

impl SearchLayer {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::RawTranscript => "原始稿",
            Self::FaithfulText => "忠实正文",
            Self::Speaker => "说话人",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchHit {
    pub layer: SearchLayer,
    pub snippet: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub has_source_refs: bool,
}

pub(crate) fn parse_search_query(input: &str) -> Option<SearchQuery> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(range) = parse_time_range(trimmed) {
        return Some(SearchQuery::TimeRange(range.0, range.1));
    }
    if let Some(point) = parse_time_point(trimmed) {
        let start = point.saturating_sub(30_000);
        let end = point.saturating_add(30_000);
        return Some(SearchQuery::TimeRange(start, end));
    }
    Some(SearchQuery::Text(trimmed.to_string()))
}

pub(crate) fn parse_search_query_with_speakers(
    input: &str,
    speakers: &std::collections::BTreeMap<u32, String>,
) -> Option<SearchQuery> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(range) = parse_time_range(trimmed) {
        return Some(SearchQuery::TimeRange(range.0, range.1));
    }
    if let Some(point) = parse_time_point(trimmed) {
        let start = point.saturating_sub(30_000);
        let end = point.saturating_add(30_000);
        return Some(SearchQuery::TimeRange(start, end));
    }
    let lower = trimmed.to_ascii_lowercase();
    for name in speakers.values() {
        if name.to_ascii_lowercase().contains(&lower) || lower.contains(&name.to_ascii_lowercase())
        {
            return Some(SearchQuery::Speaker(name.clone()));
        }
    }
    Some(SearchQuery::Text(trimmed.to_string()))
}

fn parse_time_point(input: &str) -> Option<u64> {
    let parts: Vec<&str> = input.split(':').collect();
    if parts.len() == 2 {
        let mm: u64 = parts[0].parse().ok()?;
        let ss: u64 = parts[1].parse().ok()?;
        if ss >= 60 {
            return None;
        }
        let total_secs = mm.checked_mul(60)?.checked_add(ss)?;
        return total_secs.checked_mul(1000);
    }
    if parts.len() == 3 {
        let hh: u64 = parts[0].parse().ok()?;
        let mm: u64 = parts[1].parse().ok()?;
        let ss: u64 = parts[2].parse().ok()?;
        if mm >= 60 || ss >= 60 {
            return None;
        }
        let total_secs = hh
            .checked_mul(3600)?
            .checked_add(mm.checked_mul(60)?)?
            .checked_add(ss)?;
        return total_secs.checked_mul(1000);
    }
    None
}

fn parse_time_range(input: &str) -> Option<(u64, u64)> {
    let dash = input.find('-')?;
    let left = input[..dash].trim();
    let right = input[dash + 1..].trim();
    let start = parse_time_point(left)?;
    let end = parse_time_point(right)?;
    if end < start {
        return None;
    }
    Some((start, end))
}

fn find_case_insensitive_match(text: &str, query: &str) -> Option<(usize, usize)> {
    if query.is_empty() || text.is_empty() {
        return None;
    }
    let lower_query = query.to_lowercase();
    let upper_query = query.to_uppercase();
    let query_char_count = query.chars().count();
    let min_chars = query_char_count.saturating_sub(1).max(1);
    let max_chars = query_char_count + 1;

    let char_indices: Vec<(usize, char)> = text.char_indices().collect();
    let total_chars = char_indices.len();

    for i in 0..total_chars {
        let start_byte = char_indices[i].0;
        for len in min_chars..=max_chars {
            let end_byte = if i + len < total_chars {
                char_indices[i + len].0
            } else {
                text.len()
            };
            let sub = &text[start_byte..end_byte];
            if sub.to_lowercase() == lower_query || sub.to_uppercase() == upper_query {
                return Some((start_byte, end_byte));
            }
            if i + len >= total_chars {
                break;
            }
        }
    }
    None
}

fn highlight_snippet(text: &str, query: &str) -> String {
    let mut snippet = if let Some((start_byte, end_byte)) = find_case_insensitive_match(text, query)
    {
        let sentence_start = text[..start_byte]
            .rfind(['。', '\n'])
            .map(|pos| {
                let delim_char = text[pos..].chars().next().unwrap();
                pos + delim_char.len_utf8()
            })
            .unwrap_or(0);
        let sentence_end = text[end_byte..]
            .find(['。', '\n'])
            .map(|pos| {
                let delim_char = text[end_byte + pos..].chars().next().unwrap();
                end_byte + pos + delim_char.len_utf8()
            })
            .unwrap_or(text.len());

        let before = &text[sentence_start..start_byte];
        let matched = &text[start_byte..end_byte];
        let after = &text[end_byte..sentence_end];
        format!("{before}【{matched}】{after}").trim().to_string()
    } else {
        text.chars().take(80).collect()
    };
    if snippet.is_empty() {
        snippet = text.chars().take(80).collect();
    }
    snippet
}

pub(crate) fn search(
    snapshot: &ContentSnapshotV1,
    speakers: &std::collections::BTreeMap<u32, String>,
    query: &SearchQuery,
) -> Vec<SearchHit> {
    match query {
        SearchQuery::TimeRange(start, end) => search_time_range(snapshot, *start, *end),
        SearchQuery::Speaker(name) => search_speaker(snapshot, speakers, name),
        SearchQuery::Text(text) => search_text(snapshot, text),
    }
}

fn search_time_range(snapshot: &ContentSnapshotV1, start: u64, end: u64) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for utt in &snapshot.evidence.utterances {
        if utt.end_ms >= start && utt.start_ms <= end {
            hits.push(SearchHit {
                layer: SearchLayer::RawTranscript,
                snippet: utt.text.clone(),
                start_ms: utt.start_ms,
                end_ms: utt.end_ms,
                has_source_refs: true,
            });
        }
    }
    // Faithful blocks whose source refs overlap the range.
    if let Some(record) = snapshot.current.slots.faithful_text.current.as_ref() {
        for block in &record.blocks {
            let overlaps = block
                .source_refs
                .iter()
                .any(|r| r.end_ms >= start && r.start_ms <= end);
            if overlaps {
                let snippet = block.text.clone();
                let has_refs = !block.source_refs.is_empty();
                let (s, e) = block
                    .source_refs
                    .first()
                    .map(|r| (r.start_ms, r.end_ms))
                    .unwrap_or((start, end));
                hits.push(SearchHit {
                    layer: SearchLayer::FaithfulText,
                    snippet,
                    start_ms: s,
                    end_ms: e,
                    has_source_refs: has_refs,
                });
            }
        }
    }
    hits
}

fn search_speaker(
    snapshot: &ContentSnapshotV1,
    speakers: &std::collections::BTreeMap<u32, String>,
    name: &str,
) -> Vec<SearchHit> {
    let lower = name.to_ascii_lowercase();
    let matching_speakers: Vec<(u32, String)> = speakers
        .iter()
        .filter(|(_, v)| v.to_ascii_lowercase().contains(&lower))
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    let ids: Vec<u32> = matching_speakers.iter().map(|(k, _)| *k).collect();
    snapshot
        .evidence
        .utterances
        .iter()
        .filter(|utt| ids.contains(&utt.speaker_id))
        .map(|utt| {
            let speaker_name = matching_speakers
                .iter()
                .find(|(k, _)| *k == utt.speaker_id)
                .map(|(_, v)| v.as_str())
                .unwrap_or("说话人");
            SearchHit {
                layer: SearchLayer::Speaker,
                snippet: format!("【{}】: {}", speaker_name, utt.text),
                start_ms: utt.start_ms,
                end_ms: utt.end_ms,
                has_source_refs: true,
            }
        })
        .collect()
}

fn search_text(snapshot: &ContentSnapshotV1, query: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for utt in &snapshot.evidence.utterances {
        if find_case_insensitive_match(&utt.text, query).is_some() {
            hits.push(SearchHit {
                layer: SearchLayer::RawTranscript,
                snippet: highlight_snippet(&utt.text, query),
                start_ms: utt.start_ms,
                end_ms: utt.end_ms,
                has_source_refs: true,
            });
        }
    }
    if let Some(record) = snapshot.current.slots.faithful_text.current.as_ref() {
        for block in &record.blocks {
            if find_case_insensitive_match(&block.text, query).is_some() {
                let first_ref = block.source_refs.first();
                let has_refs = !block.source_refs.is_empty();
                let (s, e) = first_ref.map(|r| (r.start_ms, r.end_ms)).unwrap_or((0, 0));
                hits.push(SearchHit {
                    layer: SearchLayer::FaithfulText,
                    snippet: highlight_snippet(&block.text, query),
                    start_ms: s,
                    end_ms: e,
                    has_source_refs: has_refs,
                });
            }
        }
    }
    hits
}

fn request_body(
    target: &ReadyGenerationTargetV1,
    request: &GenerationRequest,
) -> serde_json::Value {
    match target.api_format {
        GenerationApiFormatV1::OpenAiChatCompletions => serde_json::json!({
            "model": target.model,
            "messages": [{"role": "user", "content": request.prompt}],
            "temperature": 0.2,
        }),
        GenerationApiFormatV1::AnthropicMessages => serde_json::json!({
            "model": target.model,
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": request.prompt}],
        }),
    }
}

#[derive(Debug, Deserialize)]
struct GeneratedPayload {
    processed_utterance_ids: Vec<String>,
    blocks: Vec<GeneratedPayloadBlock>,
}

#[derive(Debug, Deserialize)]
struct GeneratedPayloadBlock {
    id: String,
    #[serde(default)]
    title: Option<String>,
    text: String,
    #[serde(default)]
    role: Option<ContentBlockRoleV1>,
    #[serde(default)]
    source_refs: Vec<SourceRefV1>,
    #[serde(default)]
    source_status: Option<SourceStatusV1>,
}

fn parse_generated_chunk(text: &str) -> Result<GeneratedChunk, GenerationPortError> {
    let json = extract_json_object(text).ok_or(GenerationPortError::ResponseInvalid)?;
    let payload: GeneratedPayload =
        serde_json::from_str(&json).map_err(|_| GenerationPortError::ResponseInvalid)?;
    Ok(GeneratedChunk {
        processed_utterance_ids: payload.processed_utterance_ids,
        blocks: payload
            .blocks
            .into_iter()
            .map(|block| GeneratedBlock {
                id: block.id,
                title: block.title,
                text: block.text,
                declared_role: block.role,
                source_refs: block.source_refs,
                declared_status: block.source_status,
            })
            .collect(),
    })
}

fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for index in start..bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=index].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn chunk_utterances(utterances: &[Utterance]) -> Vec<Vec<Utterance>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut chars = 0usize;
    for utterance in utterances {
        let length = utterance.text.chars().count();
        let would_overflow = !current.is_empty()
            && (current.len() >= CHUNK_MAX_UTTERANCES || chars + length > CHUNK_MAX_CHARS);
        if would_overflow {
            chunks.push(std::mem::take(&mut current));
            chars = 0;
        }
        chars += length;
        current.push(utterance.clone());
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn recipe_id(kind: ArtifactKindV1) -> &'static str {
    match kind {
        ArtifactKindV1::FaithfulText => "faithful-text",
        // Spec names this tier `summary-standard v1` logically, but the wire
        // recipe id stays `default-summary` for v1 compatibility; prompts and
        // docs carry the `summary-standard v1` label instead.
        ArtifactKindV1::DefaultSummary => "default-summary",
        ArtifactKindV1::ShortSummary => "summary-short",
        ArtifactKindV1::LongSummary => "summary-long",
        ArtifactKindV1::Highlights => "highlights",
        ArtifactKindV1::Chapters => "chapters",
    }
}

fn expected_role(kind: ArtifactKindV1) -> ContentBlockRoleV1 {
    match kind {
        ArtifactKindV1::FaithfulText => ContentBlockRoleV1::Paragraph,
        ArtifactKindV1::DefaultSummary
        | ArtifactKindV1::ShortSummary
        | ArtifactKindV1::LongSummary => ContentBlockRoleV1::Summary,
        ArtifactKindV1::Highlights => ContentBlockRoleV1::Highlight,
        ArtifactKindV1::Chapters => ContentBlockRoleV1::Chapter,
    }
}

struct SummaryTierContract {
    min_cjk_chars: usize,
    max_cjk_chars: usize,
    min_en_words: usize,
    max_en_words: usize,
}

fn summary_tier_contract(kind: ArtifactKindV1) -> Option<SummaryTierContract> {
    match kind {
        // Spec §5.1 / §7.1:
        // short  ≤200 汉字 / ≤120 英文词（上限按 1.5× 触发 ResponseInvalid）
        // standard 300–500 汉字 / 180–300 英文词（下限 0.5×, 上限 1.5×）
        // long   800–1500 汉字 / 480–900 英文词（下限 0.5×, 上限 1.5×）
        ArtifactKindV1::ShortSummary => Some(SummaryTierContract {
            min_cjk_chars: 0,
            max_cjk_chars: 300,
            min_en_words: 0,
            max_en_words: 180,
        }),
        ArtifactKindV1::DefaultSummary => Some(SummaryTierContract {
            min_cjk_chars: 150,
            max_cjk_chars: 750,
            min_en_words: 90,
            max_en_words: 450,
        }),
        ArtifactKindV1::LongSummary => Some(SummaryTierContract {
            min_cjk_chars: 400,
            max_cjk_chars: 2250,
            min_en_words: 240,
            max_en_words: 1350,
        }),
        _ => None,
    }
}

fn is_english_dominant(scale: crate::reading_time::TextScale) -> bool {
    // Consistent with reading_time display policy: treat mixed content by
    // dominant script, so tier contracts do not fight that policy.
    scale.latin_words * 2 > scale.cjk_chars
}

fn validate_summary_tier_length(
    kind: ArtifactKindV1,
    blocks: &[ContentBlockV1],
) -> Result<(), GenerationPortError> {
    let Some(contract) = summary_tier_contract(kind) else {
        return Ok(());
    };
    let text: String = blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let scale = crate::reading_time::measure(&text);
    let dominated_en = is_english_dominant(scale);
    let (count, min, max) = if dominated_en {
        (
            scale.latin_words as usize,
            contract.min_en_words,
            contract.max_en_words,
        )
    } else {
        (
            scale.cjk_chars as usize,
            contract.min_cjk_chars,
            contract.max_cjk_chars,
        )
    };
    if count > max || count < min {
        return Err(GenerationPortError::ResponseInvalid);
    }
    Ok(())
}

fn required_validation_checks(kind: ArtifactKindV1) -> [&'static str; 4] {
    [
        "schema",
        "processing_coverage",
        "source_refs",
        match kind {
            ArtifactKindV1::FaithfulText => "faithful_alignment",
            ArtifactKindV1::DefaultSummary
            | ArtifactKindV1::ShortSummary
            | ArtifactKindV1::LongSummary
            | ArtifactKindV1::Highlights => "supporting_claims",
            ArtifactKindV1::Chapters => "chapter_partition",
        },
    ]
}

fn make_prompt(
    kind: ArtifactKindV1,
    context: &SourceContextSnapshotV1,
    chunk: &[Utterance],
    chunk_index: usize,
    chunk_count: usize,
) -> String {
    let input = serde_json::json!({
        "recipe": recipe_id(kind),
        "recipe_version": RECIPE_VERSION,
        "kind": kind,
        "chunk_index": chunk_index,
        "chunk_count": chunk_count,
        "context": {
            "main_title": context.main_title,
            "part_title": context.part_title,
            "terms": context.terms,
        },
        "utterances": chunk.iter().map(|utterance| serde_json::json!({
            "id": utterance.id,
            "text": utterance.text,
            "start_ms": utterance.start_ms,
            "end_ms": utterance.end_ms,
            "speaker_id": utterance.speaker_id,
        })).collect::<Vec<_>>(),
    });
    let chunk_info = if chunk_count > 1 {
        format!(
            "（本次为多块处理，第 {}/{} 块，请按比例控制摘要字数）",
            chunk_index + 1,
            chunk_count
        )
    } else {
        String::new()
    };
    let tier_instructions: String = match kind {
        ArtifactKindV1::ShortSummary => format!(
            "本次任务档位 summary-short v1：输出单段整体结论，全文控制在 200 个汉字（或 120 个英文词）以内，不逐节展开、不分段列表{chunk_info}。"
        ),
        ArtifactKindV1::DefaultSummary => format!(
            "本次任务档位 summary-standard v1：输出 300 到 500 个汉字（或 180 到 300 个英文词）的标准摘要{chunk_info}。"
        ),
        ArtifactKindV1::LongSummary => format!(
            "本次任务档位 summary-long v1：输出 800 到 1500 个汉字（或 480 到 900 个英文词）的详细概述，按章节顺序逐节展开，每个 block 尽量携带可核对的 source_refs{chunk_info}。"
        ),
        _ => String::new(),
    };
    let mut instruction_lines: Vec<&str> = vec![
        "你是 BiMyScribe 的可信文字整理器。请严格按 JSON 输出。",
        "必须把本块全部 utterance id 放入 processed_utterance_ids，不能遗漏、重复或虚构。",
        "blocks 的 source_refs 只能引用输入 id，并填写精确 start_ms/end_ms；无法可靠关联时使用空 refs，不得伪造时间。",
        "faithful_text 必须保留事实、限定条件、数字、金额、日期、URL 和顺序，禁止摘要或删减。",
        "输出对象必须包含 processed_utterance_ids 和 blocks；每个 block 至少有 id、text、source_refs。",
    ];
    if !tier_instructions.is_empty() {
        instruction_lines.push(&tier_instructions);
    }
    let instructions = instruction_lines.join("\\n");
    format!(
        "{instructions}\\nrecipe={}; kind={:?}; prompt_version={}; input={}",
        recipe_id(kind),
        kind,
        PROMPT_VERSION,
        input
    )
}

fn validate_record_shape(record: &DerivationRecordV1) -> Result<(), ContentSchemaError> {
    if record.blocks.is_empty() {
        return Err(ContentSchemaError::Invariant(format!(
            "{} record must contain a block",
            recipe_id(record.kind)
        )));
    }
    if record.validation.total_utterance_count == 0
        || record.validation.processed_utterance_count != record.validation.total_utterance_count
        || !record.validation.processing_coverage_complete
        || record.validation.processing_chunks.is_empty()
    {
        return Err(ContentSchemaError::Invariant(format!(
            "{} record does not prove complete processing coverage",
            recipe_id(record.kind)
        )));
    }
    let mut output_block_ids = HashSet::new();
    for (expected_index, chunk) in record.validation.processing_chunks.iter().enumerate() {
        if chunk.chunk_index != expected_index
            || chunk.input_utterance_ids.is_empty()
            || chunk.output_block_ids.is_empty()
            || chunk
                .output_block_ids
                .iter()
                .any(|id| !output_block_ids.insert(id.as_str()))
        {
            return Err(ContentSchemaError::Invariant(
                "processing chunk ledger is not ordered or contains duplicate/missing IDs".into(),
            ));
        }
    }
    let expected = expected_role(record.kind);
    let mut block_ids = HashSet::new();
    let mut computed_status = SourceStatusV1::Mapped;
    for block in &record.blocks {
        if block.id.trim().is_empty() || !block_ids.insert(block.id.as_str()) {
            return Err(ContentSchemaError::Invariant(
                "record block ids must be non-empty and unique".into(),
            ));
        }
        if block.role != expected || block.text.trim().is_empty() {
            return Err(ContentSchemaError::Invariant(format!(
                "{} block role or text is invalid",
                recipe_id(record.kind)
            )));
        }
        if record.kind == ArtifactKindV1::FaithfulText && block.title.is_some() {
            return Err(ContentSchemaError::Invariant(
                "faithful blocks must not carry titles".into(),
            ));
        }
        if record.kind == ArtifactKindV1::Chapters
            && block
                .title
                .as_deref()
                .is_none_or(|title| title.trim().is_empty())
        {
            return Err(ContentSchemaError::Invariant(
                "chapter blocks must have a non-empty title".into(),
            ));
        }
        if block.source_refs.is_empty() {
            if block.source_status != SourceStatusV1::Limited {
                return Err(ContentSchemaError::Invariant(
                    "a block without refs must be limited".into(),
                ));
            }
            computed_status = SourceStatusV1::Limited;
        } else if block.source_status != SourceStatusV1::Mapped {
            return Err(ContentSchemaError::Invariant(
                "a block with refs must be mapped".into(),
            ));
        }
    }
    if computed_status != record.validation.record_source_status {
        return Err(ContentSchemaError::Invariant(
            "record source status does not match its blocks".into(),
        ));
    }
    let required_checks = required_validation_checks(record.kind);
    if record.validation.checks.len() != required_checks.len()
        || record
            .validation
            .checks
            .iter()
            .zip(required_checks)
            .any(|(check, expected)| check.name != expected || !check.passed)
    {
        return Err(ContentSchemaError::Invariant(
            "record validation checks are incomplete, out of order, or failed".into(),
        ));
    }
    Ok(())
}

fn expected_context(job: &crate::jobs::Job) -> SourceContextSnapshotV1 {
    SourceContextSnapshotV1 {
        main_title: job.title.clone(),
        part_title: job.part_title.clone(),
        terms: Vec::new(),
    }
}

fn read_evidence(job: &crate::jobs::Job) -> Result<ValidatedEvidenceViewV1, ContentError> {
    let work_dir = job
        .work_dir
        .as_deref()
        .ok_or_else(|| ContentError::Evidence("任务没有工作目录".into()))?;
    let raw_path = work_dir.join("transcript.raw.json");
    let bytes = fs::read(&raw_path)
        .map_err(|error| ContentError::Evidence(format!("无法读取原始 Evidence：{error}")))?;
    let utterances: Vec<Utterance> = serde_json::from_slice(&bytes)
        .map_err(|error| ContentError::Evidence(format!("原始 Evidence JSON 无效：{error}")))?;
    if utterances.is_empty() {
        return Err(ContentError::Evidence("原始 Evidence 不包含片段".into()));
    }
    let duration = job
        .duration_ms
        .ok_or_else(|| ContentError::Evidence("任务缺少视频时长".into()))?;
    if job.bvid.trim().is_empty() || job.page == 0 || job.cid.unwrap_or(0) == 0 {
        return Err(ContentError::Evidence("任务来源单元不完整".into()));
    }
    let mut ids = HashSet::new();
    let mut previous_start = 0u64;
    for (index, utterance) in utterances.iter().enumerate() {
        if utterance.id.trim().is_empty() || !ids.insert(utterance.id.as_str()) {
            return Err(ContentError::Evidence(format!(
                "原始 Evidence 的 utterance id 在第 {} 项无效",
                index + 1
            )));
        }
        if utterance.text.trim().is_empty() || utterance.end_ms < utterance.start_ms {
            return Err(ContentError::Evidence(format!(
                "原始 Evidence 的第 {} 项时间或文本无效",
                index + 1
            )));
        }
        if utterance.text.chars().count() > CHUNK_MAX_CHARS {
            return Err(ContentError::Evidence(format!(
                "原始 Evidence 的第 {} 项超过单片段长度上限",
                index + 1
            )));
        }
        if utterance.end_ms > duration {
            return Err(ContentError::Evidence(format!(
                "原始 Evidence 的第 {} 项超出视频时长",
                index + 1
            )));
        }
        if index > 0 && utterance.start_ms < previous_start {
            return Err(ContentError::Evidence("原始 Evidence 时间顺序无效".into()));
        }
        previous_start = utterance.start_ms;
    }
    let mut descriptor = EvidenceDescriptorV1 {
        identity: String::new(),
        job_id: job.id,
        platform: EVIDENCE_PLATFORM.into(),
        bvid: job.bvid.clone(),
        page: job.page,
        cid: job.cid.unwrap_or_default(),
        duration_ms: duration,
        raw_sha256: sha256_hex(&bytes),
        utterance_count: utterances.len(),
    };
    descriptor.identity = descriptor.recompute_identity()?;
    descriptor.validate()?;
    Ok(ValidatedEvidenceViewV1 {
        descriptor,
        utterances,
    })
}

fn current_matches_job(
    current: &ContentCurrentV1,
    job: &crate::jobs::Job,
    evidence: &ValidatedEvidenceViewV1,
    context: &SourceContextSnapshotV1,
    setup: &ContentSetupV1,
) -> Result<(), ContentSchemaError> {
    current.validate()?;
    if current.job_id != job.id
        || current.evidence != evidence.descriptor
        || current.source_context != *context
        || current.setup != *setup
    {
        return Err(ContentSchemaError::Invariant(
            "current does not match the Job, Evidence, context, or setup".into(),
        ));
    }
    for (kind, slot) in current.slots.iter() {
        if let Some(record) = &slot.current {
            validate_record_against_evidence(record, &evidence.utterances)?;
            if record.kind != kind {
                return Err(ContentSchemaError::Invariant(
                    "current slot record kind mismatch".into(),
                ));
            }
        }
    }
    Ok(())
}

fn load_matching_current(
    store: &ContentStore<'_>,
    job: &crate::jobs::Job,
    evidence: &ValidatedEvidenceViewV1,
    context: &SourceContextSnapshotV1,
    setup: &ContentSetupV1,
) -> Result<Option<ContentCurrentV1>, ContentError> {
    let loaded = match store.load() {
        Ok(value) => value,
        // A corrupt current is deliberately rebuildable by explicit Initial
        // / RepairContent; ordinary `current()` maps the same error to
        // RawOnly without writing.
        Err(ContentError::Schema(_)) => None,
        Err(error) => return Err(error),
    };
    match loaded {
        Some(current) if current_matches_job(&current, job, evidence, context, setup).is_ok() => {
            Ok(Some(current))
        }
        Some(_) | None => Ok(None),
    }
}

fn validate_current_snapshot(
    current: &ContentCurrentV1,
    job: &crate::jobs::Job,
    evidence: &ValidatedEvidenceViewV1,
    context: &SourceContextSnapshotV1,
    setup: &ContentSetupV1,
) -> Result<(), ContentError> {
    current_matches_job(current, job, evidence, context, setup)?;
    Ok(())
}

fn validate_source_refs(
    refs: &[SourceRefV1],
    utterances: &[Utterance],
    positions: &HashMap<String, usize>,
) -> Result<Vec<usize>, String> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let mut used = Vec::new();
    let mut last_position = None;
    for source_ref in refs {
        if source_ref.utterance_ids.is_empty() || source_ref.end_ms < source_ref.start_ms {
            return Err("source ref is empty or has reversed time".into());
        }
        let first = positions
            .get(&source_ref.utterance_ids[0])
            .copied()
            .ok_or_else(|| "source ref references an unknown utterance".to_string())?;
        let last = positions
            .get(source_ref.utterance_ids.last().unwrap())
            .copied()
            .ok_or_else(|| "source ref references an unknown utterance".to_string())?;
        if last < first || last_position.is_some_and(|previous| first <= previous) {
            return Err("source refs are out of order or overlap".into());
        }
        for (offset, id) in source_ref.utterance_ids.iter().enumerate() {
            let position = positions
                .get(id)
                .copied()
                .ok_or_else(|| "source ref references an unknown utterance".to_string())?;
            if position != first + offset {
                return Err("source ref skips or reorders utterances".into());
            }
            used.push(position);
        }
        if source_ref.start_ms != utterances[first].start_ms
            || source_ref.end_ms != utterances[last].end_ms
        {
            return Err("source ref time does not match Evidence".into());
        }
        last_position = Some(last);
    }
    Ok(used)
}

fn normalized_lexical_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut ascii_word = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            ascii_word.push(character.to_ascii_lowercase());
        } else {
            if !ascii_word.is_empty() {
                tokens.push(std::mem::take(&mut ascii_word));
            }
            if character.is_alphanumeric() {
                tokens.push(character.to_string());
            }
        }
    }
    if !ascii_word.is_empty() {
        tokens.push(ascii_word);
    }
    tokens
}

fn protected_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < text.len() {
        let rest = &text[index..];
        let url_scheme_len = if ascii_prefix_case_insensitive(rest, b"https://") {
            Some(8)
        } else if ascii_prefix_case_insensitive(rest, b"http://") {
            Some(7)
        } else {
            None
        };
        if let Some(url_scheme_len) = url_scheme_len {
            let end = protected_url_end(text, index);
            let value = text[index..end].to_string();
            if !value.is_empty() {
                let scheme = if url_scheme_len == 8 {
                    "https://"
                } else {
                    "http://"
                };
                tokens.push(format!("{scheme}{}", &value[url_scheme_len..]));
            }
            index = end;
            continue;
        }
        let character = rest.chars().next().unwrap();
        if is_currency_symbol(character) {
            let mut number_start = index + character.len_utf8();
            while number_start < text.len()
                && text[number_start..].chars().next().unwrap().is_whitespace()
            {
                number_start += text[number_start..].chars().next().unwrap().len_utf8();
            }
            if number_start < text.len()
                && text[number_start..]
                    .chars()
                    .next()
                    .unwrap()
                    .is_ascii_digit()
            {
                let end = protected_number_end(text, number_start);
                tokens.push(format!("{character}{}", &text[number_start..end]));
                index = end;
                continue;
            }
        }
        if character.is_ascii_digit() {
            let mut end = protected_number_end(text, index);
            if text[end..].starts_with("元") || text[end..].starts_with("円") {
                end += text[end..].chars().next().unwrap().len_utf8();
            } else if text[end..].starts_with("美元") {
                end += "美元".len();
            }
            tokens.push(text[index..end].to_string());
            index = end;
        } else {
            index += character.len_utf8();
        }
    }
    tokens
}

fn is_currency_symbol(character: char) -> bool {
    matches!(character, '$' | '¥' | '￥' | '€' | '£' | '₩' | '₹')
}

fn ascii_prefix_case_insensitive(value: &str, prefix: &[u8]) -> bool {
    value.as_bytes().get(..prefix.len()).is_some_and(|bytes| {
        bytes
            .iter()
            .zip(prefix)
            .all(|(value, prefix)| value.eq_ignore_ascii_case(prefix))
    })
}

fn protected_number_end(text: &str, start: usize) -> usize {
    let mut end = start + text[start..].chars().next().unwrap().len_utf8();
    while end < text.len() {
        let next = text[end..].chars().next().unwrap();
        if next.is_ascii_digit() || matches!(next, '.' | '-' | '/' | ':' | '%' | ',') {
            end += next.len_utf8();
        } else {
            break;
        }
    }
    end
}

fn protected_url_end(text: &str, start: usize) -> usize {
    let mut end = start;
    while end < text.len() {
        let character = text[end..].chars().next().unwrap();
        if character.is_ascii()
            && (character.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(character))
        {
            end += character.len_utf8();
        } else {
            break;
        }
    }
    end
}

fn token_counts(tokens: &[String]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for token in tokens {
        *counts.entry(token.clone()).or_insert(0) += 1;
    }
    counts
}

fn is_ordered_subsequence(needles: &[String], haystack: &[String]) -> bool {
    let mut cursor = 0usize;
    for needle in needles {
        let Some(offset) = haystack[cursor..]
            .iter()
            .position(|candidate| candidate == needle)
        else {
            return false;
        };
        cursor += offset + 1;
    }
    true
}

fn validate_faithful_text_pair(source: &str, output: &str) -> Result<(), String> {
    let source_tokens = normalized_lexical_tokens(source);
    let output_tokens = normalized_lexical_tokens(output);
    if output.trim().is_empty() || output_tokens != source_tokens {
        return Err("faithful output changed the normalized lexical sequence".into());
    }
    if protected_tokens(source) != protected_tokens(output) {
        return Err("faithful output changed the ordered number or URL multiset".into());
    }
    Ok(())
}

fn source_text_for_refs(
    refs: &[SourceRefV1],
    utterances: &[Utterance],
    positions: &HashMap<String, usize>,
) -> Result<String, String> {
    let mut text = String::new();
    for source_ref in refs {
        for id in &source_ref.utterance_ids {
            let position = positions
                .get(id)
                .copied()
                .ok_or_else(|| "source ref references an unknown utterance".to_string())?;
            text.push_str(&utterances[position].text);
            text.push('\n');
        }
    }
    Ok(text)
}

fn validate_supporting_block(
    kind: ArtifactKindV1,
    block: &ContentBlockV1,
    utterances: &[Utterance],
    positions: &HashMap<String, usize>,
) -> Result<(), String> {
    if block.source_status == SourceStatusV1::Limited {
        if !block.source_refs.is_empty() {
            return Err("limited supporting block must have empty refs".into());
        }
        return Ok(());
    }
    let source = source_text_for_refs(&block.source_refs, utterances, positions)?;
    let source_tokens = normalized_lexical_tokens(&source);
    let title = block.title.as_deref().unwrap_or_default();
    let title = if is_fixed_support_label(kind, title) {
        ""
    } else {
        title
    };
    let output = format!("{title}\n{}", block.text);
    let output_tokens = normalized_lexical_tokens(&output);
    if output_tokens.is_empty() {
        return Err("mapped supporting block has no lexical content".into());
    }
    if !is_ordered_subsequence(&output_tokens, &source_tokens) {
        return Err("mapped supporting block has no conservative lexical support".into());
    }
    let source_protected = protected_tokens(&source);
    let output_protected = protected_tokens(&output);
    let source_counts = token_counts(&source_protected);
    for (token, count) in token_counts(&output_protected) {
        if count > source_counts.get(&token).copied().unwrap_or(0) {
            return Err("supporting block introduced a number or URL outside refs".into());
        }
    }
    if !is_ordered_subsequence(&output_protected, &source_protected) {
        return Err("supporting block reordered protected values outside refs".into());
    }
    Ok(())
}

fn is_fixed_support_label(kind: ArtifactKindV1, title: &str) -> bool {
    match kind {
        ArtifactKindV1::DefaultSummary => {
            matches!(title, "摘要" | "总结") || title.eq_ignore_ascii_case("summary")
        }
        ArtifactKindV1::ShortSummary | ArtifactKindV1::LongSummary | ArtifactKindV1::Highlights => {
            matches!(title, "重点" | "要点") || title.eq_ignore_ascii_case("highlight")
        }
        _ => false,
    }
}

fn validate_chapter_partitions(
    blocks: &[ContentBlockV1],
    utterances: &[Utterance],
    positions: &HashMap<String, usize>,
    all_used: &[usize],
) -> Result<(), String> {
    if all_used.len() != utterances.len()
        || all_used
            .iter()
            .enumerate()
            .any(|(index, position)| *position != index)
    {
        return Err("chapter refs contain a gap, overlap, or reorder".into());
    }
    let mut cursor = 0usize;
    for block in blocks {
        if block.source_status != SourceStatusV1::Mapped || block.source_refs.is_empty() {
            return Err("mapped chapter must have refs".into());
        }
        let used = validate_source_refs(&block.source_refs, utterances, positions)?;
        if used.first().copied() != Some(cursor) {
            return Err("chapter boundaries are not contiguous".into());
        }
        cursor = used.last().copied().unwrap() + 1;
    }
    if cursor != utterances.len() {
        return Err("chapter boundaries do not cover all Evidence".into());
    }
    Ok(())
}

/// A limited chapter record may still contain mapped chapters. The mapped
/// blocks must retain valid, non-overlapping source refs, while limited blocks
/// explicitly carry no refs; unlike an all-mapped record, gaps are honest
/// limitations rather than schema corruption.
fn validate_chapter_mixed_sources(
    blocks: &[ContentBlockV1],
    utterances: &[Utterance],
    positions: &HashMap<String, usize>,
    all_used: &[usize],
) -> Result<(), String> {
    if all_used.windows(2).any(|window| window[0] >= window[1]) {
        return Err("mapped chapter refs overlap or are out of order".into());
    }
    for block in blocks {
        match block.source_status {
            SourceStatusV1::Mapped if block.source_refs.is_empty() => {
                return Err("mapped chapter must have refs".into());
            }
            SourceStatusV1::Mapped => {
                validate_source_refs(&block.source_refs, utterances, positions)?;
            }
            SourceStatusV1::Limited if !block.source_refs.is_empty() => {
                return Err("limited chapter must not have refs".into());
            }
            SourceStatusV1::Limited => {}
        }
    }
    Ok(())
}

fn validate_processing_chunks(
    record: &DerivationRecordV1,
    utterances: &[Utterance],
) -> Result<(), String> {
    let expected_chunks = chunk_utterances(utterances);
    if record.validation.processing_chunks.len() != expected_chunks.len() {
        return Err("processing chunk count does not match Evidence".into());
    }
    let mut output_block_ids = Vec::new();
    for (index, chunk) in record.validation.processing_chunks.iter().enumerate() {
        let expected_input_ids: Vec<String> = expected_chunks[index]
            .iter()
            .map(|utterance| utterance.id.clone())
            .collect();
        if chunk.chunk_index != index || chunk.input_utterance_ids != expected_input_ids {
            return Err("processing chunk input ledger does not match Evidence".into());
        }
        if chunk.output_block_ids.is_empty() {
            return Err("processing chunk has no output blocks".into());
        }
        output_block_ids.extend(chunk.output_block_ids.iter().cloned());
    }
    let record_block_ids: Vec<String> =
        record.blocks.iter().map(|block| block.id.clone()).collect();
    if output_block_ids != record_block_ids {
        return Err("processing chunk output ledger does not match record blocks".into());
    }
    Ok(())
}

fn validate_record_against_evidence(
    record: &DerivationRecordV1,
    utterances: &[Utterance],
) -> Result<(), ContentSchemaError> {
    if record.validation.total_utterance_count != utterances.len()
        || record.validation.processed_utterance_count != utterances.len()
        || !record.validation.processing_coverage_complete
    {
        return Err(ContentSchemaError::Invariant(
            "record coverage counts do not match the validated Evidence".into(),
        ));
    }
    validate_record_shape(record)?;
    validate_processing_chunks(record, utterances).map_err(ContentSchemaError::Invariant)?;
    let positions: HashMap<String, usize> = utterances
        .iter()
        .enumerate()
        .map(|(index, utterance)| (utterance.id.clone(), index))
        .collect();
    let mut all_used = Vec::new();
    for block in &record.blocks {
        let used = validate_source_refs(&block.source_refs, utterances, &positions)
            .map_err(ContentSchemaError::Invariant)?;
        if block.source_status == SourceStatusV1::Mapped {
            all_used.extend(used.iter().copied());
        }
        if record.kind == ArtifactKindV1::FaithfulText {
            let source = source_text_for_refs(&block.source_refs, utterances, &positions)
                .map_err(ContentSchemaError::Invariant)?;
            validate_faithful_text_pair(&source, &block.text)
                .map_err(ContentSchemaError::Invariant)?;
        } else if matches!(
            record.kind,
            ArtifactKindV1::DefaultSummary
                | ArtifactKindV1::ShortSummary
                | ArtifactKindV1::LongSummary
                | ArtifactKindV1::Highlights
        ) {
            validate_supporting_block(record.kind, block, utterances, &positions)
                .map_err(ContentSchemaError::Invariant)?;
        }
    }
    match record.kind {
        ArtifactKindV1::FaithfulText
            if record.validation.record_source_status == SourceStatusV1::Mapped =>
        {
            if all_used.len() != utterances.len()
                || all_used
                    .iter()
                    .enumerate()
                    .any(|(index, position)| *position != index)
            {
                return Err(ContentSchemaError::Invariant(
                    "faithful refs do not cover Evidence in order".into(),
                ));
            }
        }
        ArtifactKindV1::FaithfulText => {
            return Err(ContentSchemaError::Invariant(
                "faithful text cannot use limited source status".into(),
            ));
        }
        ArtifactKindV1::Chapters
            if record.validation.record_source_status == SourceStatusV1::Mapped =>
        {
            validate_chapter_partitions(&record.blocks, utterances, &positions, &all_used)
                .map_err(ContentSchemaError::Invariant)?;
        }
        ArtifactKindV1::Chapters => {
            validate_chapter_mixed_sources(&record.blocks, utterances, &positions, &all_used)
                .map_err(ContentSchemaError::Invariant)?
        }
        ArtifactKindV1::DefaultSummary
        | ArtifactKindV1::ShortSummary
        | ArtifactKindV1::LongSummary
        | ArtifactKindV1::Highlights => {}
    }
    Ok(())
}

fn build_provenance(
    kind: ArtifactKindV1,
    target: Option<&ReadyGenerationTargetV1>,
    effective_path: EffectivePathV1,
    fallback_reason: Option<FallbackReasonV1>,
) -> DerivationProvenanceV1 {
    DerivationProvenanceV1 {
        recipe_id: recipe_id(kind).into(),
        recipe_version: RECIPE_VERSION,
        prompt_id: target.map(|_| recipe_id(kind).into()),
        prompt_version: target.map(|_| PROMPT_VERSION),
        rules_id: recipe_id(kind).into(),
        rules_version: RULES_VERSION,
        provider: target.map(|target| target.provider),
        api_format: target.map(|target| target.api_format),
        endpoint: target.map(|target| target.endpoint.clone()),
        model: target.map(|target| target.model.clone()),
        parameters: target.map(|target| target.parameters.clone()),
        effective_path,
        fallback_reason,
    }
}

#[allow(clippy::too_many_arguments)] // One private constructor validates the complete persisted record material.
fn build_record(
    kind: ArtifactKindV1,
    evidence: &EvidenceDescriptorV1,
    context: &SourceContextSnapshotV1,
    blocks: Vec<ContentBlockV1>,
    processing_chunks: Vec<ProcessingChunkV1>,
    target: Option<&ReadyGenerationTargetV1>,
    effective_path: EffectivePathV1,
    fallback_reason: Option<FallbackReasonV1>,
) -> Result<DerivationRecordV1, ContentSchemaError> {
    let source_status = if blocks
        .iter()
        .any(|block| block.source_status == SourceStatusV1::Limited)
    {
        SourceStatusV1::Limited
    } else {
        SourceStatusV1::Mapped
    };
    let record = DerivationRecordV1 {
        schema_version: CONTENT_SCHEMA_VERSION,
        revision: 1,
        kind,
        evidence_identity: evidence.identity.clone(),
        source_context: context.clone(),
        provenance: build_provenance(kind, target, effective_path, fallback_reason),
        blocks,
        validation: ValidationReportV1 {
            processed_utterance_count: evidence.utterance_count,
            total_utterance_count: evidence.utterance_count,
            processing_coverage_complete: true,
            processing_chunks,
            record_source_status: source_status,
            checks: vec![
                ValidationCheckV1 {
                    name: "schema".into(),
                    passed: true,
                    message: None,
                },
                ValidationCheckV1 {
                    name: "processing_coverage".into(),
                    passed: true,
                    message: None,
                },
                ValidationCheckV1 {
                    name: "source_refs".into(),
                    passed: true,
                    message: None,
                },
                ValidationCheckV1 {
                    name: required_validation_checks(kind)[3].into(),
                    passed: true,
                    message: None,
                },
            ],
        },
        created_at: Utc::now(),
    };
    Ok(record)
}

fn deterministic_faithful_record(
    evidence: &ValidatedEvidenceViewV1,
    context: &SourceContextSnapshotV1,
    effective_path: EffectivePathV1,
    target: Option<&ReadyGenerationTargetV1>,
    fallback_reason: Option<FallbackReasonV1>,
) -> Result<DerivationRecordV1, ContentSchemaError> {
    let chunks = chunk_utterances(&evidence.utterances);
    let results = chunks
        .iter()
        .enumerate()
        .map(|(chunk_index, chunk)| {
            let blocks = chunk
                .iter()
                .enumerate()
                .map(|(offset, utterance)| ContentBlockV1 {
                    id: format!("chunk-{chunk_index}-faithful-{offset:04}"),
                    role: ContentBlockRoleV1::Paragraph,
                    title: None,
                    text: utterance.text.clone(),
                    source_refs: vec![SourceRefV1 {
                        utterance_ids: vec![utterance.id.clone()],
                        start_ms: utterance.start_ms,
                        end_ms: utterance.end_ms,
                    }],
                    source_status: SourceStatusV1::Mapped,
                })
                .collect();
            ChunkResult {
                chunk_index,
                input_ids: chunk.iter().map(|utterance| utterance.id.clone()).collect(),
                blocks,
            }
        })
        .collect();
    merge_chunk_results(
        ArtifactKindV1::FaithfulText,
        evidence,
        context,
        target,
        effective_path,
        fallback_reason,
        results,
    )
    .map_err(|_| ContentSchemaError::Invariant("faithful rules merge failed".into()))
}

fn convert_generated_blocks(
    kind: ArtifactKindV1,
    chunk_index: usize,
    generated: GeneratedChunk,
) -> Result<Vec<ContentBlockV1>, GenerationPortError> {
    if generated.blocks.is_empty() {
        return Err(GenerationPortError::ResponseInvalid);
    }
    let role = expected_role(kind);
    generated
        .blocks
        .into_iter()
        .map(|block| {
            if block.id.trim().is_empty() || block.text.trim().is_empty() {
                return Err(GenerationPortError::ResponseInvalid);
            }
            if kind == ArtifactKindV1::FaithfulText && block.title.is_some() {
                return Err(GenerationPortError::ResponseInvalid);
            }
            if block.declared_role.is_some_and(|declared| declared != role) {
                return Err(GenerationPortError::ResponseInvalid);
            }
            let status = if block.source_refs.is_empty() {
                SourceStatusV1::Limited
            } else {
                SourceStatusV1::Mapped
            };
            if block
                .declared_status
                .is_some_and(|declared| declared != status)
            {
                return Err(GenerationPortError::ResponseInvalid);
            }
            Ok(ContentBlockV1 {
                id: format!("chunk-{chunk_index}-{}", block.id),
                role,
                title: block.title,
                text: block.text,
                source_refs: block.source_refs,
                source_status: status,
            })
        })
        .collect()
}

fn validate_chunk_result(
    kind: ArtifactKindV1,
    chunk_index: usize,
    chunk: &[Utterance],
    mut blocks: Vec<ContentBlockV1>,
) -> Result<ChunkResult, GenerationPortError> {
    if blocks.is_empty() {
        return Err(GenerationPortError::ResponseInvalid);
    }
    let positions: HashMap<String, usize> = chunk
        .iter()
        .enumerate()
        .map(|(index, utterance)| (utterance.id.clone(), index))
        .collect();
    for block in &mut blocks {
        validate_source_refs(&block.source_refs, chunk, &positions)
            .map_err(|_| GenerationPortError::ResponseInvalid)?;
        if kind == ArtifactKindV1::Chapters
            && block
                .title
                .as_deref()
                .is_none_or(|title| title.trim().is_empty())
        {
            return Err(GenerationPortError::ResponseInvalid);
        }
        if matches!(
            kind,
            ArtifactKindV1::DefaultSummary
                | ArtifactKindV1::ShortSummary
                | ArtifactKindV1::LongSummary
                | ArtifactKindV1::Highlights
        ) && block.source_status == SourceStatusV1::Mapped
            && validate_supporting_block(kind, block, chunk, &positions).is_err()
        {
            // The refs are structurally valid but do not support the claim.
            // Preserve the text honestly as a limited block; malformed refs
            // were rejected above and never reach this downgrade.
            block.source_refs.clear();
            block.source_status = SourceStatusV1::Limited;
        }
    }
    Ok(ChunkResult {
        chunk_index,
        input_ids: chunk.iter().map(|utterance| utterance.id.clone()).collect(),
        blocks,
    })
}

fn merge_chunk_results(
    kind: ArtifactKindV1,
    evidence: &ValidatedEvidenceViewV1,
    context: &SourceContextSnapshotV1,
    target: Option<&ReadyGenerationTargetV1>,
    effective_path: EffectivePathV1,
    fallback_reason: Option<FallbackReasonV1>,
    chunks: Vec<ChunkResult>,
) -> Result<DerivationRecordV1, GenerationPortError> {
    let expected_chunks = chunk_utterances(&evidence.utterances);
    if chunks.len() != expected_chunks.len() {
        return Err(GenerationPortError::ResponseInvalid);
    }
    let processing_chunks: Vec<ProcessingChunkV1> = chunks
        .iter()
        .map(|result| ProcessingChunkV1 {
            chunk_index: result.chunk_index,
            input_utterance_ids: result.input_ids.clone(),
            output_block_ids: result.blocks.iter().map(|block| block.id.clone()).collect(),
        })
        .collect();
    let mut ledger = HashSet::new();
    let mut blocks = Vec::new();
    for (index, result) in chunks.into_iter().enumerate() {
        let expected_ids: Vec<String> = expected_chunks[index]
            .iter()
            .map(|utterance| utterance.id.clone())
            .collect();
        if result.chunk_index != index || result.input_ids != expected_ids {
            return Err(GenerationPortError::ResponseInvalid);
        }
        for id in &result.input_ids {
            if !ledger.insert(id.clone()) {
                return Err(GenerationPortError::ResponseInvalid);
            }
        }
        blocks.extend(result.blocks);
    }
    let expected_ids: HashSet<String> = evidence
        .utterances
        .iter()
        .map(|utterance| utterance.id.clone())
        .collect();
    if ledger != expected_ids {
        return Err(GenerationPortError::ResponseInvalid);
    }
    let record = build_record(
        kind,
        &evidence.descriptor,
        context,
        blocks,
        processing_chunks,
        target,
        effective_path,
        fallback_reason,
    )
    .map_err(|_| GenerationPortError::ResponseInvalid)?;
    validate_record_against_evidence(&record, &evidence.utterances)
        .map_err(|_| GenerationPortError::ResponseInvalid)?;
    // Final tier-level length contract: chunks are no longer a tier boundary.
    validate_summary_tier_length(kind, &record.blocks)?;
    Ok(record)
}

fn generate_llm_record(
    port: &dyn GenerationPort,
    target: &ReadyGenerationTargetV1,
    kind: ArtifactKindV1,
    evidence: &ValidatedEvidenceViewV1,
    context: &SourceContextSnapshotV1,
    cancel: &CancellationToken,
) -> Result<DerivationRecordV1, GenerationPortError> {
    let chunks = chunk_utterances(&evidence.utterances);
    let mut results = Vec::new();
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(GenerationPortError::Transport);
        }
        let expected_ids: Vec<String> =
            chunk.iter().map(|utterance| utterance.id.clone()).collect();
        let request = GenerationRequest {
            kind,
            chunk_index,
            chunk_count: chunks.len(),
            utterances: chunk.clone(),
            prompt: make_prompt(kind, context, chunk, chunk_index, chunks.len()),
        };
        let generated = port.generate(target, &request)?;
        if generated.processed_utterance_ids != expected_ids {
            return Err(GenerationPortError::ResponseInvalid);
        }
        let blocks = convert_generated_blocks(kind, chunk_index, generated)?;
        results.push(validate_chunk_result(kind, chunk_index, chunk, blocks)?);
    }
    merge_chunk_results(
        kind,
        evidence,
        context,
        Some(target),
        EffectivePathV1::Llm,
        None,
        results,
    )
}

fn failure_for_port(
    error: GenerationPortError,
) -> (FailureCodeV1, FallbackReasonCodeV1, &'static str) {
    match error {
        GenerationPortError::Transport => (
            FailureCodeV1::GenerationFailed,
            FallbackReasonCodeV1::GenerationFailed,
            "本地 AI 请求失败",
        ),
        GenerationPortError::ResponseInvalid => (
            FailureCodeV1::ResponseInvalid,
            FallbackReasonCodeV1::ResponseInvalid,
            "本地 AI 返回内容未通过校验",
        ),
    }
}

fn make_failure(code: FailureCodeV1, message: &'static str) -> DerivationFailureV1 {
    DerivationFailureV1 {
        code,
        message: message.into(),
        retryable: true,
        occurred_at: Utc::now(),
    }
}

fn next_revision(slot: &ArtifactSlotV1) -> Result<u32, ContentError> {
    slot.current
        .as_ref()
        .map(|record| record.revision.checked_add(1))
        .unwrap_or(Some(1))
        .ok_or_else(|| ContentError::Generation("结果 revision 已达到上限".into()))
}

fn save_snapshot(
    store: &ContentStore<'_>,
    current: &ContentCurrentV1,
    job: &crate::jobs::Job,
    evidence: &ValidatedEvidenceViewV1,
    context: &SourceContextSnapshotV1,
    setup: &ContentSetupV1,
) -> Result<ContentSnapshotV1, ContentError> {
    validate_current_snapshot(current, job, evidence, context, setup)?;
    store.save(current)?;
    Ok(ContentSnapshotV1 {
        current: current.clone(),
        evidence: evidence.clone(),
    })
}

fn build_empty_current(
    job: &crate::jobs::Job,
    evidence: &ValidatedEvidenceViewV1,
    context: &SourceContextSnapshotV1,
    setup: &ContentSetupV1,
) -> ContentCurrentV1 {
    ContentCurrentV1 {
        schema_version: CONTENT_SCHEMA_VERSION,
        job_id: job.id,
        evidence: evidence.descriptor.clone(),
        source_context: context.clone(),
        setup: setup.clone(),
        slots: ContentSlotsV1::default(),
    }
}

fn execute_initial(
    job: &crate::jobs::Job,
    setup: &ContentSetupV1,
    evidence: &ValidatedEvidenceViewV1,
    context: &SourceContextSnapshotV1,
    store: &ContentStore<'_>,
    port: &dyn GenerationPort,
    cancel: &CancellationToken,
) -> Result<ContentSnapshotV1, ContentError> {
    let existing = load_matching_current(store, job, evidence, context, setup)?;
    let mut current = existing
        .clone()
        .unwrap_or_else(|| build_empty_current(job, evidence, context, setup));
    // v1 -> v2 migration: any successfully loaded current must be upgraded to
    // the on-disk writer version so the next publish writes v2 without
    // mutating evidence identity, revisions, or provenance.
    if current.schema_version != CONTENT_SCHEMA_VERSION {
        current.schema_version = CONTENT_SCHEMA_VERSION;
    }
    // Upgrade each record's stuck v1 schema_version without touching content.
    for (_, slot) in current.slots.iter_mut_compat() {
        if let Some(record) = slot.current.as_mut() {
            if record.schema_version != CONTENT_SCHEMA_VERSION {
                record.schema_version = CONTENT_SCHEMA_VERSION;
            }
        }
    }
    let v1_migrated = existing
        .as_ref()
        .is_some_and(|loaded| loaded.schema_version != CONTENT_SCHEMA_VERSION);
    let mut dirty = existing.is_none() || v1_migrated;

    if cancel.is_cancelled() {
        return Err(ContentError::Cancelled);
    }
    if current.slots.faithful_text.current.is_none() {
        let (mut record, failure) = match &setup.initial_target {
            InitialTargetV1::Disabled => (
                deterministic_faithful_record(
                    evidence,
                    context,
                    EffectivePathV1::Rules,
                    None,
                    None,
                )
                .map_err(ContentError::Schema)?,
                None,
            ),
            InitialTargetV1::Unavailable(target) => (
                deterministic_faithful_record(
                    evidence,
                    context,
                    EffectivePathV1::RulesFallback,
                    None,
                    Some(FallbackReasonV1 {
                        code: FallbackReasonCodeV1::TargetUnavailable,
                        message: target.message.clone(),
                    }),
                )
                .map_err(ContentError::Schema)?,
                None,
            ),
            InitialTargetV1::Ready(target) => match generate_llm_record(
                port,
                target,
                ArtifactKindV1::FaithfulText,
                evidence,
                context,
                cancel,
            ) {
                Ok(record) => (record, None),
                Err(_) if cancel.is_cancelled() => return Err(ContentError::Cancelled),
                Err(error) => {
                    let (_, reason_code, message) = failure_for_port(error);
                    let fallback = deterministic_faithful_record(
                        evidence,
                        context,
                        EffectivePathV1::RulesFallback,
                        Some(target),
                        Some(FallbackReasonV1 {
                            code: reason_code,
                            message: message.into(),
                        }),
                    )
                    .map_err(ContentError::Schema)?;
                    (fallback, None)
                }
            },
        };
        record.revision = next_revision(&current.slots.faithful_text)?;
        current.slots.faithful_text.current = Some(record);
        current.slots.faithful_text.last_failure = failure;
        dirty = true;
    }

    if setup.enhancements_requested {
        let target = match &setup.initial_target {
            InitialTargetV1::Ready(target) => Some(target),
            InitialTargetV1::Unavailable(_) | InitialTargetV1::Disabled => None,
        };
        for kind in [
            ArtifactKindV1::DefaultSummary,
            ArtifactKindV1::Highlights,
            ArtifactKindV1::Chapters,
        ] {
            if cancel.is_cancelled() {
                return Err(ContentError::Cancelled);
            }
            if current.slots.get(kind).current.is_some() {
                continue;
            }
            let generated = match target {
                None => Err((FailureCodeV1::TargetUnavailable, "本地 AI 配置不可用")),
                Some(target) => {
                    match generate_llm_record(port, target, kind, evidence, context, cancel) {
                        Ok(mut record) => {
                            let revision = next_revision(current.slots.get(kind))?;
                            record.revision = revision;
                            let slot = current.slots.get_mut(kind);
                            slot.current = Some(record);
                            slot.last_failure = None;
                            dirty = true;
                            Ok(())
                        }
                        Err(_) if cancel.is_cancelled() => return Err(ContentError::Cancelled),
                        Err(error) => {
                            let (code, _, message) = failure_for_port(error);
                            Err((code, message))
                        }
                    }
                }
            };
            if let Err((code, message)) = generated {
                current.slots.get_mut(kind).last_failure = Some(make_failure(code, message));
                dirty = true;
            }
        }
    }

    if cancel.is_cancelled() {
        return Err(ContentError::Cancelled);
    }
    if !dirty {
        return Ok(ContentSnapshotV1 {
            current,
            evidence: evidence.clone(),
        });
    }
    save_snapshot(store, &current, job, evidence, context, setup)
}

#[allow(clippy::too_many_arguments)] // Keeps the public ContentResults interface narrow; inputs are internal state.
fn execute_regenerate(
    job: &crate::jobs::Job,
    setup: &ContentSetupV1,
    evidence: &ValidatedEvidenceViewV1,
    context: &SourceContextSnapshotV1,
    store: &ContentStore<'_>,
    port: &dyn GenerationPort,
    cancel: &CancellationToken,
    kind: RegenerableKind,
    target: ReadyGenerationTargetV1,
) -> Result<ContentSnapshotV1, ContentError> {
    if !setup.enhancements_requested {
        return Err(ContentError::InvalidIntent("当前任务未请求 AI 增强".into()));
    }
    if !job.status.is_terminal() {
        return Err(ContentError::InvalidIntent("任务尚未进入终态".into()));
    }
    target.validate()?;
    let mut current = load_matching_current(store, job, evidence, context, setup)?
        .ok_or_else(|| ContentError::InvalidIntent("当前可信结果不可用".into()))?;
    if current.slots.faithful_text.current.is_none() {
        return Err(ContentError::InvalidIntent("忠实正文尚未生成".into()));
    }
    if cancel.is_cancelled() {
        return Err(ContentError::Cancelled);
    }
    let artifact_kind = kind.artifact_kind();
    let outcome = generate_llm_record(port, &target, artifact_kind, evidence, context, cancel);
    if cancel.is_cancelled() {
        return Err(ContentError::Cancelled);
    }
    match outcome {
        Ok(mut record) => {
            let slot = current.slots.get_mut(artifact_kind);
            record.revision = next_revision(slot)?;
            slot.current = Some(record);
            slot.last_failure = None;
        }
        Err(_) if cancel.is_cancelled() => return Err(ContentError::Cancelled),
        Err(error) => {
            let (code, _, message) = failure_for_port(error);
            current.slots.get_mut(artifact_kind).last_failure = Some(make_failure(code, message));
        }
    }
    save_snapshot(store, &current, job, evidence, context, setup)
}

/// Read the latest structured result. It never cleans a temporary file,
/// repairs JSON, or rebuilds from Markdown.
pub(crate) fn current(job: &crate::jobs::Job) -> Result<ContentViewV1, ContentError> {
    let Some(setup) = &job.content_setup else {
        return Ok(ContentViewV1::Legacy);
    };
    setup.validate()?;
    let evidence = read_evidence(job)?;
    let context = expected_context(job);
    let store = ContentStore::new(
        job.work_dir
            .as_deref()
            .ok_or_else(|| ContentError::Evidence("任务没有工作目录".into()))?,
    )?;
    let loaded = match store.load() {
        Ok(value) => value,
        Err(ContentError::Schema(_)) => {
            return Ok(ContentViewV1::RawOnly {
                evidence,
                current_issue: CurrentIssueV1::Corrupt,
            });
        }
        Err(error) => return Err(error),
    };
    let Some(current) = loaded else {
        return Ok(ContentViewV1::RawOnly {
            evidence,
            current_issue: CurrentIssueV1::NotGenerated,
        });
    };
    if current_matches_job(&current, job, &evidence, &context, setup).is_err() {
        return Ok(ContentViewV1::RawOnly {
            evidence,
            current_issue: CurrentIssueV1::Corrupt,
        });
    }
    Ok(ContentViewV1::Current(ContentSnapshotV1 {
        current,
        evidence,
    }))
}

/// Generate or regenerate content and atomically publish one whole current.
pub(crate) fn execute(
    job: &crate::jobs::Job,
    intent: Intent,
    cancel: &CancellationToken,
) -> Result<ContentSnapshotV1, ContentError> {
    execute_with_port(job, intent, cancel, &HttpGenerationPort)
}

fn execute_with_port(
    job: &crate::jobs::Job,
    intent: Intent,
    cancel: &CancellationToken,
    port: &dyn GenerationPort,
) -> Result<ContentSnapshotV1, ContentError> {
    let Some(setup) = job.content_setup.as_ref() else {
        return Err(ContentError::Legacy);
    };
    setup.validate()?;
    // Validate user-clicked regeneration input before even the explicit temp
    // cleanup step, so an invalid target is a true zero-mutation rejection.
    if let Intent::Regenerate { target, .. } = &intent {
        target.validate()?;
        if !setup.enhancements_requested {
            return Err(ContentError::InvalidIntent("当前任务未请求 AI 增强".into()));
        }
        if !job.status.is_terminal() {
            return Err(ContentError::InvalidIntent("任务尚未进入终态".into()));
        }
    }
    if cancel.is_cancelled() {
        return Err(ContentError::Cancelled);
    }
    let evidence = read_evidence(job)?;
    let context = expected_context(job);
    let work_dir = job
        .work_dir
        .as_deref()
        .ok_or_else(|| ContentError::Evidence("任务没有工作目录".into()))?;
    let store = ContentStore::new(work_dir)?;
    if let Err(error) = store.cleanup_temp() {
        log::warn!("failed to clean stale content current temp: {error}");
    }
    match intent {
        Intent::Initial => execute_initial(job, setup, &evidence, &context, &store, port, cancel),
        Intent::Regenerate { kind, target } => execute_regenerate(
            job, setup, &evidence, &context, &store, port, cancel, kind, target,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bimyscribe-content-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn evidence(job_id: Uuid) -> EvidenceDescriptorV1 {
        let mut descriptor = EvidenceDescriptorV1 {
            identity: String::new(),
            job_id,
            platform: EVIDENCE_PLATFORM.into(),
            bvid: "BV1fixture".into(),
            page: 1,
            cid: 7,
            duration_ms: 1000,
            raw_sha256: "b".repeat(64),
            utterance_count: 1,
        };
        descriptor.identity = descriptor.recompute_identity().unwrap();
        descriptor
    }

    fn record(job_id: Uuid, kind: ArtifactKindV1) -> DerivationRecordV1 {
        DerivationRecordV1 {
            schema_version: CONTENT_SCHEMA_VERSION,
            revision: 1,
            kind,
            evidence_identity: evidence(job_id).identity,
            source_context: SourceContextSnapshotV1 {
                main_title: "主标题".into(),
                part_title: Some("分P标题".into()),
                terms: vec!["术语".into()],
            },
            provenance: DerivationProvenanceV1 {
                recipe_id: "faithful-text".into(),
                recipe_version: 1,
                prompt_id: None,
                prompt_version: None,
                rules_id: "faithful-text".into(),
                rules_version: 1,
                provider: None,
                api_format: None,
                endpoint: None,
                model: None,
                parameters: None,
                effective_path: EffectivePathV1::Rules,
                fallback_reason: None,
            },
            blocks: vec![ContentBlockV1 {
                id: "b1".into(),
                role: ContentBlockRoleV1::Paragraph,
                title: None,
                text: "内容".into(),
                source_refs: vec![SourceRefV1 {
                    utterance_ids: vec!["u1".into()],
                    start_ms: 0,
                    end_ms: 1000,
                }],
                source_status: SourceStatusV1::Mapped,
            }],
            validation: ValidationReportV1 {
                processed_utterance_count: 1,
                total_utterance_count: 1,
                processing_coverage_complete: true,
                processing_chunks: vec![ProcessingChunkV1 {
                    chunk_index: 0,
                    input_utterance_ids: vec!["u1".into()],
                    output_block_ids: vec!["b1".into()],
                }],
                record_source_status: SourceStatusV1::Mapped,
                checks: required_validation_checks(kind)
                    .into_iter()
                    .map(|name| ValidationCheckV1 {
                        name: name.into(),
                        passed: true,
                        message: None,
                    })
                    .collect(),
            },
            created_at: Utc::now(),
        }
    }

    fn fixture_current(job_id: Uuid) -> ContentCurrentV1 {
        let mut slots = ContentSlotsV1::default();
        slots.faithful_text.current = Some(record(job_id, ArtifactKindV1::FaithfulText));
        ContentCurrentV1 {
            schema_version: CONTENT_SCHEMA_VERSION,
            job_id,
            evidence: evidence(job_id),
            source_context: SourceContextSnapshotV1 {
                main_title: "主标题".into(),
                part_title: Some("分P标题".into()),
                terms: vec!["术语".into()],
            },
            setup: ContentSetupV1::disabled(),
            slots,
        }
    }

    #[test]
    fn setup_roundtrip_preserves_disabled_ready_and_unavailable() {
        let ready = ReadyGenerationTargetV1 {
            provider: GenerationProviderV1::OpenAiCompatible,
            api_format: GenerationApiFormatV1::OpenAiChatCompletions,
            endpoint: "http://127.0.0.1:1234".into(),
            model: "local-model".into(),
            parameters: GenerationParametersV1::openai_default(),
        };
        let values = [
            ContentSetupV1::disabled(),
            ContentSetupV1::ready(ready),
            ContentSetupV1::unavailable(TargetUnavailableV1 {
                code: TargetUnavailableCodeV1::ConnectionMissing,
                message: "本地 AI 不可用".into(),
                invalid_field: None,
            }),
        ];
        for value in values {
            value.validate().unwrap();
            let json = serde_json::to_vec(&value).unwrap();
            let decoded: ContentSetupV1 = serde_json::from_slice(&json).unwrap();
            assert_eq!(decoded, value);
        }

        let mut wrong_openai = ContentSetupV1::ready(ReadyGenerationTargetV1 {
            provider: GenerationProviderV1::OpenAiCompatible,
            api_format: GenerationApiFormatV1::OpenAiChatCompletions,
            endpoint: "http://127.0.0.1:1234".into(),
            model: "local-model".into(),
            parameters: GenerationParametersV1::OpenAiChatCompletions { temperature: 0.3 },
        });
        assert!(wrong_openai.validate().is_err());
        wrong_openai.initial_target = InitialTargetV1::Ready(ReadyGenerationTargetV1 {
            provider: GenerationProviderV1::AnthropicCompatible,
            api_format: GenerationApiFormatV1::AnthropicMessages,
            endpoint: "http://127.0.0.1:1234".into(),
            model: "local-model".into(),
            parameters: GenerationParametersV1::AnthropicMessages { max_tokens: 2048 },
        });
        assert!(wrong_openai.validate().is_err());
    }

    #[test]
    fn creation_helper_derives_the_three_closed_setup_states() {
        let ready_target = ReadyGenerationTargetV1 {
            provider: GenerationProviderV1::OpenAiCompatible,
            api_format: GenerationApiFormatV1::OpenAiChatCompletions,
            endpoint: "http://127.0.0.1:1234".into(),
            model: "local-model".into(),
            parameters: GenerationParametersV1::openai_default(),
        };
        let unavailable = TargetUnavailableV1 {
            code: TargetUnavailableCodeV1::EndpointInvalid,
            message: "本地 AI 地址不可用".into(),
            invalid_field: Some("endpoint".into()),
        };
        let states = [
            InitialTargetV1::Disabled,
            InitialTargetV1::Ready(ready_target),
            InitialTargetV1::Unavailable(unavailable),
        ];
        let requested = [false, true, true];
        for (state, requested) in states.into_iter().zip(requested) {
            let setup = ContentSetupV1::from_initial_target(state);
            assert_eq!(setup.enhancements_requested, requested);
            setup.validate().unwrap();
        }
    }

    #[test]
    fn generation_wire_names_and_fixed_typed_parameters_are_stable() {
        assert_eq!(
            serde_json::to_value(GenerationApiFormatV1::OpenAiChatCompletions).unwrap(),
            serde_json::json!("openai_chat_completions")
        );
        assert_eq!(
            serde_json::to_value(GenerationParametersV1::openai_default()).unwrap(),
            serde_json::json!({
                "kind": "openai_chat_completions",
                "temperature": 0.2
            })
        );
        assert_eq!(
            serde_json::to_value(GenerationParametersV1::anthropic_default()).unwrap(),
            serde_json::json!({
                "kind": "anthropic_messages",
                "max_tokens": 4096
            })
        );

        assert!(
            GenerationParametersV1::OpenAiChatCompletions { temperature: 0.3 }
                .validate()
                .is_err()
        );
        assert!(
            GenerationParametersV1::AnthropicMessages { max_tokens: 4095 }
                .validate()
                .is_err()
        );
        assert!(GenerationParametersV1::OpenAiChatCompletions {
            temperature: f64::NAN
        }
        .validate()
        .is_err());
    }

    #[test]
    fn local_endpoint_normalization_is_shared_by_ready_and_provenance_validation() {
        assert_eq!(
            canonical_local_endpoint("http://LOCALHOST:80/v1/chat/completions").unwrap(),
            "http://localhost"
        );
        assert_eq!(
            canonical_local_endpoint("https://[::1]:9443/base/v1/messages/").unwrap(),
            "https://[::1]:9443/base"
        );

        let invalid = [
            "https://remote.example",
            "http://user@localhost",
            "http://localhost/?token=secret",
            "http://localhost/#fragment",
        ];
        for endpoint in invalid {
            assert!(canonical_local_endpoint(endpoint).is_err(), "{endpoint}");
        }

        let mut ready = ReadyGenerationTargetV1 {
            provider: GenerationProviderV1::OpenAiCompatible,
            api_format: GenerationApiFormatV1::OpenAiChatCompletions,
            endpoint: "http://localhost/v1".into(),
            model: "local-model".into(),
            parameters: GenerationParametersV1::openai_default(),
        };
        assert!(ready.validate().is_err());
        ready.endpoint = canonical_local_endpoint(&ready.endpoint).unwrap();
        ready.validate().unwrap();

        let mut provenance = DerivationProvenanceV1 {
            recipe_id: "faithful-text".into(),
            recipe_version: 1,
            prompt_id: Some("faithful-text".into()),
            prompt_version: Some(1),
            rules_id: "faithful-text".into(),
            rules_version: 1,
            provider: Some(GenerationProviderV1::OpenAiCompatible),
            api_format: Some(GenerationApiFormatV1::OpenAiChatCompletions),
            endpoint: Some("http://localhost/".into()),
            model: Some("local-model".into()),
            parameters: Some(GenerationParametersV1::openai_default()),
            effective_path: EffectivePathV1::Llm,
            fallback_reason: None,
        };
        assert!(provenance.validate(ArtifactKindV1::FaithfulText).is_err());
        provenance.endpoint = Some("http://localhost".into());
        provenance.validate(ArtifactKindV1::FaithfulText).unwrap();
    }

    #[test]
    fn unavailable_reason_is_bounded_and_cannot_persist_sensitive_detail() {
        let valid = TargetUnavailableV1 {
            code: TargetUnavailableCodeV1::ConnectionMissing,
            message: "x".repeat(MAX_UNAVAILABLE_MESSAGE_CHARS),
            invalid_field: Some("endpoint".into()),
        };
        valid.validate().unwrap();

        let mut too_long = valid.clone();
        too_long.message.push('x');
        assert!(too_long.validate().is_err());

        for message in [
            "http://127.0.0.1:1234/v1",
            "request failed: api_key=secret",
            "request failed?token=secret",
        ] {
            let mut leaked = valid.clone();
            leaked.message = message.into();
            assert!(
                leaked.validate().is_err(),
                "message should be rejected: {message}"
            );
        }
        let mut long_field = valid;
        long_field.invalid_field = Some("x".repeat(MAX_UNAVAILABLE_FIELD_CHARS + 1));
        assert!(long_field.validate().is_err());
    }

    #[test]
    fn evidence_identity_is_fixed_material_and_lowercase_only() {
        let id = Uuid::new_v4();
        let descriptor = evidence(id);
        descriptor.validate().unwrap();
        assert_eq!(
            descriptor.recompute_identity().unwrap(),
            descriptor.identity
        );

        let mut mismatched = descriptor.clone();
        mismatched.duration_ms += 1;
        assert!(mismatched.validate().is_err());

        let mut uppercase = descriptor;
        uppercase.raw_sha256 = "A".repeat(64);
        assert!(uppercase.recompute_identity().is_err());
        assert!(uppercase.validate().is_err());
    }

    #[test]
    fn source_context_fixture_keeps_part_title_and_terms() {
        let value = fixture_current(Uuid::new_v4());
        let json = serde_json::to_vec(&value).unwrap();
        let decoded: ContentCurrentV1 = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.source_context.main_title, "主标题");
        assert_eq!(
            decoded.source_context.part_title.as_deref(),
            Some("分P标题")
        );
        assert_eq!(decoded.source_context.terms, vec!["术语"]);
        decoded.validate().unwrap();
    }

    #[test]
    fn persisted_provenance_rejects_a_non_fixed_typed_parameter() {
        let job_id = Uuid::new_v4();
        let mut value = fixture_current(job_id);
        let provenance = &mut value
            .slots
            .faithful_text
            .current
            .as_mut()
            .unwrap()
            .provenance;
        provenance.provider = Some(GenerationProviderV1::OpenAiCompatible);
        provenance.api_format = Some(GenerationApiFormatV1::OpenAiChatCompletions);
        provenance.endpoint = Some("http://127.0.0.1:1234".into());
        provenance.model = Some("local-model".into());
        provenance.parameters =
            Some(GenerationParametersV1::OpenAiChatCompletions { temperature: 0.3 });
        provenance.effective_path = EffectivePathV1::Llm;
        assert!(value.validate().is_err());
    }

    #[test]
    fn current_rejects_zero_revision_and_mismatched_source_context() {
        let job_id = Uuid::new_v4();
        let mut zero_revision = fixture_current(job_id);
        zero_revision
            .slots
            .faithful_text
            .current
            .as_mut()
            .unwrap()
            .revision = 0;
        assert!(zero_revision.validate().is_err());

        let mut mismatched_context = fixture_current(job_id);
        mismatched_context
            .slots
            .faithful_text
            .current
            .as_mut()
            .unwrap()
            .source_context
            .terms = vec!["另一个术语".into()];
        assert!(mismatched_context.validate().is_err());
    }

    #[test]
    fn current_store_roundtrip_and_temp_cleanup() {
        let dir = temp_dir("roundtrip");
        let store = ContentStore::new(&dir).unwrap();
        let value = fixture_current(Uuid::new_v4());
        store.save(&value).unwrap();
        assert_eq!(store.load().unwrap(), Some(value));
        assert!(!dir.join(format!("{CONTENT_CURRENT_FILE}.tmp")).exists());
        fs::write(
            dir.join(format!("{CONTENT_CURRENT_FILE}.tmp")),
            b"stale temporary data",
        )
        .unwrap();
        assert!(store.load().is_ok());
        assert!(dir.join(format!("{CONTENT_CURRENT_FILE}.tmp")).exists());
        store.cleanup_temp().unwrap();
        assert!(!dir.join(format!("{CONTENT_CURRENT_FILE}.tmp")).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn damaged_current_is_rejected_and_unknown_kind_is_rejected() {
        let dir = temp_dir("damaged");
        let store = ContentStore::new(&dir).unwrap();
        fs::write(dir.join(CONTENT_CURRENT_FILE), b"not json").unwrap();
        assert!(matches!(
            store.load(),
            Err(ContentError::Schema(ContentSchemaError::Json(_)))
        ));

        let mut json = serde_json::to_value(fixture_current(Uuid::new_v4())).unwrap();
        json["slots"]["faithful_text"]["current"]["kind"] =
            serde_json::Value::String("unknown_kind".into());
        fs::write(
            dir.join(CONTENT_CURRENT_FILE),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            store.load(),
            Err(ContentError::Schema(ContentSchemaError::Json(_)))
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failed_temp_write_keeps_the_previous_current_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("failed-replace");
        let store = ContentStore::new(&dir).unwrap();
        let first = fixture_current(Uuid::new_v4());
        store.save(&first).unwrap();

        let mut permissions = fs::metadata(&dir).unwrap().permissions();
        permissions.set_mode(0o500);
        fs::set_permissions(&dir, permissions).unwrap();
        let second = fixture_current(Uuid::new_v4());
        assert!(store.save(&second).is_err());

        let mut permissions = fs::metadata(&dir).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&dir, permissions).unwrap();
        assert_eq!(store.load().unwrap(), Some(first));
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn existing_temp_symlink_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("temp-symlink");
        let store = ContentStore::new(&dir).unwrap();
        let outside = dir.join("outside.json");
        let tmp = dir.join(format!("{CONTENT_CURRENT_FILE}.tmp"));
        fs::write(&outside, b"keep me").unwrap();
        symlink(&outside, &tmp).unwrap();

        assert!(store.save(&fixture_current(Uuid::new_v4())).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep me");
        assert!(
            fs::symlink_metadata(&tmp).is_err(),
            "failed save removes only the symlink itself"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_store_path_never_creates_file() {
        let missing =
            std::env::temp_dir().join(format!("bimyscribe-content-missing-{}", Uuid::new_v4()));
        assert!(ContentStore::new(&missing).is_err());
        assert!(!missing.exists());
    }

    struct FakeGenerationPort {
        fail_kind: Option<ArtifactKindV1>,
        limited_kind: Option<ArtifactKindV1>,
        title_kind: Option<ArtifactKindV1>,
        calls: std::cell::RefCell<Vec<(ArtifactKindV1, usize, usize)>>,
    }

    impl FakeGenerationPort {
        fn new() -> Self {
            Self {
                fail_kind: None,
                limited_kind: None,
                title_kind: None,
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn with_failure(kind: ArtifactKindV1) -> Self {
            Self {
                fail_kind: Some(kind),
                ..Self::new()
            }
        }

        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl GenerationPort for FakeGenerationPort {
        fn generate(
            &self,
            _target: &ReadyGenerationTargetV1,
            request: &GenerationRequest,
        ) -> Result<GeneratedChunk, GenerationPortError> {
            self.calls
                .borrow_mut()
                .push((request.kind, request.chunk_index, request.chunk_count));
            if self.fail_kind == Some(request.kind) {
                return Err(GenerationPortError::Transport);
            }
            let (target_utterances, text) = match request.kind {
                ArtifactKindV1::DefaultSummary => {
                    let utts: Vec<&Utterance> = request.utterances.iter().take(2).collect();
                    let text = utts.iter().map(|u| u.text.as_str()).collect::<String>();
                    (utts, text)
                }
                ArtifactKindV1::FaithfulText => {
                    let utts: Vec<&Utterance> = request.utterances.iter().collect();
                    let text = utts.iter().map(|u| u.text.as_str()).collect::<String>();
                    (utts, text)
                }
                ArtifactKindV1::Highlights => {
                    let utts: Vec<&Utterance> = request.utterances.iter().take(1).collect();
                    (utts, "原始文本 123。".into())
                }
                ArtifactKindV1::Chapters => {
                    let utts: Vec<&Utterance> = request.utterances.iter().collect();
                    (utts, "章节：完整覆盖本段内容。".into())
                }
                ArtifactKindV1::ShortSummary => {
                    let utts: Vec<&Utterance> = request.utterances.iter().take(1).collect();
                    (utts, "短摘要结论。".into())
                }
                ArtifactKindV1::LongSummary => {
                    let utts: Vec<&Utterance> = request.utterances.iter().collect();
                    let text = format!(
                        "{}{}",
                        "长摘要概述。".repeat(90),
                        "本段内容已按章节展开并保留来源映射，覆盖全部输入片段。"
                    );
                    (utts, text)
                }
            };
            let source_refs = if self.limited_kind == Some(request.kind) {
                Vec::new()
            } else {
                vec![SourceRefV1 {
                    utterance_ids: target_utterances.iter().map(|u| u.id.clone()).collect(),
                    start_ms: target_utterances.first().unwrap().start_ms,
                    end_ms: target_utterances.last().unwrap().end_ms,
                }]
            };
            let all_ids: Vec<String> = request
                .utterances
                .iter()
                .map(|utterance| utterance.id.clone())
                .collect();
            Ok(GeneratedChunk {
                processed_utterance_ids: all_ids,
                blocks: vec![GeneratedBlock {
                    id: format!("fake-{}", request.chunk_index),
                    title: if self.title_kind == Some(request.kind) {
                        Some("模型标题".into())
                    } else {
                        (request.kind == ArtifactKindV1::Chapters)
                            .then(|| format!("第{}块", request.chunk_index + 1))
                    },
                    text,
                    declared_role: None,
                    source_refs,
                    declared_status: None,
                }],
            })
        }
    }

    fn test_target() -> ReadyGenerationTargetV1 {
        ReadyGenerationTargetV1 {
            provider: GenerationProviderV1::OpenAiCompatible,
            api_format: GenerationApiFormatV1::OpenAiChatCompletions,
            endpoint: "http://127.0.0.1:1234".into(),
            model: "fixture-model".into(),
            parameters: GenerationParametersV1::openai_default(),
        }
    }

    fn test_job(label: &str, setup: ContentSetupV1, count: usize) -> crate::jobs::Job {
        let id = Uuid::new_v4();
        let dir = temp_dir(label);
        let utterances: Vec<Utterance> = (0..count)
            .map(|index| Utterance {
                id: format!("u{index:04}"),
                text: format!(
                    "第 {index} 段原始文本，这是用于测试可信文字结果生成的完整段落描述信息，核对来源映射与正文字数规模，保证摘要档位长度契约能够正确校验通过并且保留所有可追溯引用关系与对应内容。数字 123，链接 https://example.com/{index}"
                ),
                start_ms: index as u64 * 1_000,
                end_ms: index as u64 * 1_000 + 900,
                speaker_id: (index % 2) as u32,
            })
            .collect();
        fs::write(
            dir.join("transcript.raw.json"),
            serde_json::to_vec_pretty(&utterances).unwrap(),
        )
        .unwrap();
        let mut job = crate::jobs::Job::new(id, "BV1fixture".into(), 1);
        job.title = "主标题".into();
        job.part_title = Some("分P标题".into());
        job.work_dir = Some(dir);
        job.cid = Some(7);
        job.duration_ms = Some(count as u64 * 1_000);
        job.status = crate::jobs::JobStatus::Completed;
        job.content_setup = Some(setup);
        job
    }

    fn ready_setup() -> ContentSetupV1 {
        ContentSetupV1::ready(test_target())
    }

    #[test]
    #[ignore = "requires an explicitly configured Docker Runtime and audio sample"]
    fn docker_funasr_evidence_content_presentation_full_chain() {
        let required_path = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_else(|| panic!("{name} must be set for this ignored test"))
        };
        let project_dir = required_path("BIMYSCRIBE_RUNTIME_PROJECT");
        let runtime_data = required_path("BIMYSCRIBE_RUNTIME_DATA_DIR");
        let normalized_wav = required_path("BIMYSCRIBE_RUNTIME_SMOKE_WAV");
        let smoke_root = required_path("BIMYSCRIBE_RUNTIME_SMOKE_ROOT");

        let ready = crate::funasr::install_runtime(&project_dir, &runtime_data).unwrap();
        assert_eq!(ready.device, "Docker CPU");
        let description = crate::funasr::describe_runtime(
            &project_dir,
            &runtime_data,
            crate::jobs::RuntimeSource::External,
        )
        .unwrap();
        assert_eq!(
            description.backend,
            crate::jobs::RuntimeBackend::DockerCompose
        );

        let selection = crate::jobs::TranscriptionSelection::new(
            crate::jobs::RuntimeSource::External,
            description.project.clone(),
            description.data_dir.clone(),
            description.identity.clone(),
            description.backend,
            description.model_description.clone(),
            crate::jobs::SourceLanguage::En,
            crate::jobs::CreatedFrom::Cli,
        );
        let job_id = Uuid::new_v4();
        let job_dir = smoke_root.join(format!("content-chain-job-{job_id}"));
        let output_dir = smoke_root.join(format!("content-chain-output-{job_id}"));
        fs::create_dir_all(&job_dir).unwrap();

        let started = std::time::Instant::now();
        let outcome = crate::funasr::run(crate::funasr::TranscribeRequest {
            job_dir: &job_dir,
            normalized_wav: &normalized_wav,
            selection: &selection,
            duration_ms: None,
            log_path: &job_dir.join("runtime.log"),
            job_id: &job_id,
            instance_nonce: "content-chain-runtime",
            cancel_token: &CancellationToken::new(),
        })
        .unwrap();
        let utterance_count = outcome.utterances.len();
        let duration_ms = outcome
            .utterances
            .iter()
            .map(|utterance| utterance.end_ms)
            .max()
            .unwrap_or(0);
        assert!(utterance_count > 0);
        assert!(job_dir.join("transcript.raw.json").is_file());
        let raw_bytes = fs::read(job_dir.join("transcript.raw.json")).unwrap();
        let raw_sha256 = sha256_hex(&raw_bytes);

        // Use the existing deterministic GenerationPort seam so this ignored
        // test proves the real Docker Evidence feeds all four ContentResults
        // recipes without introducing a live LLM service or host uv.
        let mut job = crate::jobs::Job::new(job_id, "BV1dockerfixture".into(), 1);
        job.title = "Docker full-chain fixture".into();
        job.part_title = Some("Docker part".into());
        job.source_url = Some("https://www.bilibili.com/video/BV1dockerfixture".into());
        job.cid = Some(1);
        job.duration_ms = Some(duration_ms);
        job.work_dir = Some(job_dir.clone());
        job.final_output_dir = Some(output_dir.clone());
        job.status = crate::jobs::JobStatus::Completed;
        job.stage = crate::jobs::Stage::Completed;
        job.stage_progress = 100;
        job.transcription_selection = Some(selection.clone());
        job.transcription_result = Some(outcome.result);
        job.content_setup = Some(ready_setup());

        let fake = FakeGenerationPort::new();
        let snapshot =
            execute_with_port(&job, Intent::Initial, &CancellationToken::new(), &fake).unwrap();
        for kind in ArtifactKindV1::INITIAL_ALL {
            assert!(
                snapshot.current.slots.get(kind).current.is_some(),
                "Docker Evidence should produce a current record for {kind:?}"
            );
        }
        assert_eq!(snapshot.evidence.utterances.len(), utterance_count);
        let current_path = job_dir.join(CONTENT_CURRENT_FILE);
        let current_bytes = fs::read(&current_path).unwrap();
        assert!(matches!(current(&job).unwrap(), ContentViewV1::Current(_)));

        crate::document::rebuild_presentation(&job, &snapshot).unwrap();
        let raw_md = job_dir.join("transcript.raw.md");
        let readable_md = job_dir.join("transcript.readable.md");
        let full_md = output_dir.join("full.md");
        assert!(raw_md.is_file());
        assert!(readable_md.is_file());
        assert!(full_md.is_file());
        let full_bytes = fs::read(&full_md).unwrap();
        assert!(String::from_utf8_lossy(&full_bytes).contains("Docker full-chain fixture"));

        eprintln!(
            "Docker full chain: identity={} backend={} model={:?} utterances={} elapsed_ms={} raw_sha256={} current_sha256={} full_sha256={} output={}",
            selection.runtime_identity,
            selection.runtime_backend.as_str(),
            selection.model_description,
            utterance_count,
            started.elapsed().as_millis(),
            raw_sha256,
            sha256_hex(&current_bytes),
            sha256_hex(&full_bytes),
            full_md.display(),
        );

        fs::remove_dir_all(job_dir).ok();
        fs::remove_dir_all(output_dir).ok();
    }

    #[test]
    fn production_request_shapes_are_typed_and_have_no_auth_fields() {
        assert_eq!(
            api_path(GenerationApiFormatV1::OpenAiChatCompletions),
            "/v1/chat/completions"
        );
        assert_eq!(
            api_path(GenerationApiFormatV1::AnthropicMessages),
            "/v1/messages"
        );
        assert_eq!(
            request_url(&test_target(), GenerationApiFormatV1::OpenAiChatCompletions),
            "http://127.0.0.1:1234/v1/chat/completions"
        );
        assert_eq!(HTTP_CONTENT_TYPE, "application/json");
        assert_eq!(HTTP_USER_AGENT, "bimyscribe/0.5");
        let request = GenerationRequest {
            kind: ArtifactKindV1::DefaultSummary,
            chunk_index: 0,
            chunk_count: 1,
            utterances: Vec::new(),
            prompt: "fixture prompt".into(),
        };
        let openai = request_body(&test_target(), &request);
        assert_eq!(openai["model"], "fixture-model");
        assert_eq!(openai["temperature"], 0.2);
        assert!(openai.get("max_tokens").is_none());
        assert!(openai.get("Authorization").is_none());
        let mut anthropic_target = test_target();
        anthropic_target.provider = GenerationProviderV1::AnthropicCompatible;
        anthropic_target.api_format = GenerationApiFormatV1::AnthropicMessages;
        anthropic_target.parameters = GenerationParametersV1::anthropic_default();
        let anthropic = request_body(&anthropic_target, &request);
        assert_eq!(anthropic["model"], "fixture-model");
        assert_eq!(anthropic["max_tokens"], 4096);
        assert!(anthropic.get("temperature").is_none());
        assert!(anthropic.get("Authorization").is_none());
    }

    #[test]
    fn shared_config_resolver_freezes_disabled_ready_and_unavailable() {
        assert!(matches!(
            resolve_content_setup(false, None).initial_target,
            InitialTargetV1::Disabled
        ));
        assert!(matches!(
            resolve_content_setup(true, None).initial_target,
            InitialTargetV1::Unavailable(TargetUnavailableV1 {
                code: TargetUnavailableCodeV1::ConnectionMissing,
                ..
            })
        ));
        let ready = LlmConnection {
            id: "default".into(),
            name: "fixture".into(),
            api_format: ApiFormat::OpenAiChatCompletions,
            base_url: "http://LOCALHOST:80/v1/chat/completions/".into(),
            model: "  fixture-model  ".into(),
        };
        let setup = resolve_content_setup(true, Some(&ready));
        match setup.initial_target {
            InitialTargetV1::Ready(target) => {
                assert_eq!(target.endpoint, "http://localhost");
                assert_eq!(target.model, "fixture-model");
            }
            other => panic!("expected ready target, got {other:?}"),
        }
        for base_url in [
            "https://remote.example",
            "http://user@localhost",
            "http://localhost/?token=secret",
        ] {
            let invalid = LlmConnection {
                base_url: base_url.into(),
                ..ready.clone()
            };
            assert!(matches!(
                resolve_content_setup(true, Some(&invalid)).initial_target,
                InitialTargetV1::Unavailable(TargetUnavailableV1 {
                    code: TargetUnavailableCodeV1::EndpointInvalid,
                    ..
                })
            ));
        }
    }

    #[test]
    fn production_adapter_rejects_remote_and_authenticated_targets_before_http() {
        let request = GenerationRequest {
            kind: ArtifactKindV1::DefaultSummary,
            chunk_index: 0,
            chunk_count: 1,
            utterances: Vec::new(),
            prompt: "fixture prompt".into(),
        };
        for endpoint in [
            "https://remote.example",
            "http://user@localhost",
            "http://localhost/?token=secret",
        ] {
            let mut target = test_target();
            target.endpoint = endpoint.into();
            assert!(matches!(
                HttpGenerationPort.generate(&target, &request),
                Err(GenerationPortError::ResponseInvalid)
            ));
        }
    }

    #[test]
    fn disabled_initial_publishes_rules_faithful_and_current_is_read_only() {
        let mut job = test_job("disabled", ContentSetupV1::disabled(), 2);
        let before = current(&job).unwrap();
        assert!(matches!(
            before,
            ContentViewV1::RawOnly {
                current_issue: CurrentIssueV1::NotGenerated,
                ..
            }
        ));
        let fake = FakeGenerationPort::new();
        let snapshot =
            execute_with_port(&job, Intent::Initial, &CancellationToken::new(), &fake).unwrap();
        assert_eq!(fake.call_count(), 0);
        assert_eq!(
            snapshot
                .current
                .slots
                .faithful_text
                .current
                .as_ref()
                .unwrap()
                .provenance
                .effective_path,
            EffectivePathV1::Rules
        );
        assert!(snapshot.current.slots.default_summary.current.is_none());
        assert!(snapshot.current.slots.chapters.current.is_none());
        let bytes = fs::read(job.work_dir.as_ref().unwrap().join(CONTENT_CURRENT_FILE)).unwrap();
        let view = current(&job).unwrap();
        assert!(matches!(view, ContentViewV1::Current(_)));
        assert_eq!(
            bytes,
            fs::read(job.work_dir.as_ref().unwrap().join(CONTENT_CURRENT_FILE)).unwrap()
        );
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn unavailable_setup_has_zero_calls_and_independent_failures() {
        let setup = ContentSetupV1::unavailable(TargetUnavailableV1 {
            code: TargetUnavailableCodeV1::EndpointInvalid,
            message: "本地 AI 地址不可用".into(),
            invalid_field: Some("llm_connection.base_url".into()),
        });
        let mut job = test_job("unavailable", setup, 2);
        let fake = FakeGenerationPort::new();
        let snapshot =
            execute_with_port(&job, Intent::Initial, &CancellationToken::new(), &fake).unwrap();
        assert_eq!(fake.call_count(), 0);
        assert_eq!(
            snapshot
                .current
                .slots
                .faithful_text
                .current
                .as_ref()
                .unwrap()
                .provenance
                .fallback_reason
                .as_ref()
                .unwrap()
                .code,
            FallbackReasonCodeV1::TargetUnavailable
        );
        for kind in [
            ArtifactKindV1::DefaultSummary,
            ArtifactKindV1::Highlights,
            ArtifactKindV1::Chapters,
        ] {
            assert_eq!(
                snapshot
                    .current
                    .slots
                    .get(kind)
                    .last_failure
                    .as_ref()
                    .unwrap()
                    .code,
                FailureCodeV1::TargetUnavailable
            );
        }
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn ready_initial_uses_all_four_independent_recipes_and_long_coverage() {
        let mut job = test_job("ready-long", ready_setup(), 45);
        let fake = FakeGenerationPort::new();
        let snapshot =
            execute_with_port(&job, Intent::Initial, &CancellationToken::new(), &fake).unwrap();
        assert!(
            fake.call_count() >= 12,
            "four recipes must process all chunks"
        );
        for kind in ArtifactKindV1::INITIAL_ALL {
            let slot = snapshot.current.slots.get(kind);
            let record = slot.current.as_ref().unwrap();
            assert_eq!(record.provenance.effective_path, EffectivePathV1::Llm);
            assert_eq!(record.validation.processed_utterance_count, 45);
            assert!(record.validation.processing_coverage_complete);
        }
        let previous = snapshot.current.clone();
        let before_summary_revision = previous
            .slots
            .default_summary
            .current
            .as_ref()
            .unwrap()
            .revision;
        let regenerated = execute_with_port(
            &job,
            Intent::Regenerate {
                kind: RegenerableKind::DefaultSummary,
                target: test_target(),
            },
            &CancellationToken::new(),
            &fake,
        )
        .unwrap();
        assert_eq!(
            regenerated
                .current
                .slots
                .default_summary
                .current
                .as_ref()
                .unwrap()
                .revision,
            before_summary_revision + 1
        );
        assert_eq!(
            regenerated.current.slots.faithful_text.current,
            previous.slots.faithful_text.current
        );
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn enhancement_failure_does_not_hide_other_results() {
        let mut job = test_job("failure-isolation", ready_setup(), 2);
        let fake = FakeGenerationPort::with_failure(ArtifactKindV1::Highlights);
        let snapshot =
            execute_with_port(&job, Intent::Initial, &CancellationToken::new(), &fake).unwrap();
        assert!(snapshot.current.slots.faithful_text.current.is_some());
        assert!(snapshot.current.slots.default_summary.current.is_some());
        assert!(snapshot.current.slots.chapters.current.is_some());
        assert_eq!(
            snapshot
                .current
                .slots
                .highlights
                .last_failure
                .as_ref()
                .unwrap()
                .code,
            FailureCodeV1::GenerationFailed
        );
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn faithful_generation_failure_publishes_rules_fallback() {
        let mut job = test_job("faithful-fallback", ready_setup(), 2);
        let snapshot = execute_with_port(
            &job,
            Intent::Initial,
            &CancellationToken::new(),
            &FakeGenerationPort::with_failure(ArtifactKindV1::FaithfulText),
        )
        .unwrap();
        let faithful = snapshot
            .current
            .slots
            .faithful_text
            .current
            .as_ref()
            .unwrap();
        assert_eq!(
            faithful.provenance.effective_path,
            EffectivePathV1::RulesFallback
        );
        assert_eq!(
            faithful.provenance.fallback_reason.as_ref().unwrap().code,
            FallbackReasonCodeV1::GenerationFailed
        );
        assert!(snapshot.current.slots.default_summary.current.is_some());
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn faithful_model_title_is_response_invalid_and_falls_back_to_rules() {
        let mut job = test_job("faithful-title-response", ready_setup(), 2);
        let fake = FakeGenerationPort {
            title_kind: Some(ArtifactKindV1::FaithfulText),
            ..FakeGenerationPort::new()
        };
        let snapshot =
            execute_with_port(&job, Intent::Initial, &CancellationToken::new(), &fake).unwrap();
        let faithful = snapshot
            .current
            .slots
            .faithful_text
            .current
            .as_ref()
            .unwrap();
        assert_eq!(
            faithful.provenance.effective_path,
            EffectivePathV1::RulesFallback
        );
        assert_eq!(
            faithful.provenance.fallback_reason.as_ref().unwrap().code,
            FallbackReasonCodeV1::ResponseInvalid
        );
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn limited_block_is_allowed_only_with_empty_refs_and_is_reported() {
        let mut job = test_job("limited", ready_setup(), 2);
        let fake = FakeGenerationPort {
            limited_kind: Some(ArtifactKindV1::DefaultSummary),
            ..FakeGenerationPort::new()
        };
        let snapshot =
            execute_with_port(&job, Intent::Initial, &CancellationToken::new(), &fake).unwrap();
        let summary = snapshot
            .current
            .slots
            .default_summary
            .current
            .as_ref()
            .unwrap();
        assert_eq!(
            summary.validation.record_source_status,
            SourceStatusV1::Limited
        );
        assert!(summary.blocks.iter().all(|block| {
            block.source_status == SourceStatusV1::Limited && block.source_refs.is_empty()
        }));
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn current_tamper_is_raw_only_and_raw_tamper_is_error() {
        let mut job = test_job("tamper", ContentSetupV1::disabled(), 2);
        execute_with_port(
            &job,
            Intent::Initial,
            &CancellationToken::new(),
            &FakeGenerationPort::new(),
        )
        .unwrap();
        let current_path = job.work_dir.as_ref().unwrap().join(CONTENT_CURRENT_FILE);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&current_path).unwrap()).unwrap();
        value["job_id"] = serde_json::Value::String(Uuid::new_v4().to_string());
        fs::write(&current_path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            current(&job).unwrap(),
            ContentViewV1::RawOnly {
                current_issue: CurrentIssueV1::Corrupt,
                ..
            }
        ));
        fs::write(
            job.work_dir.as_ref().unwrap().join("transcript.raw.json"),
            b"not json",
        )
        .unwrap();
        assert!(matches!(current(&job), Err(ContentError::Evidence(_))));
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn persisted_faithful_title_tamper_is_raw_only() {
        let mut job = test_job("faithful-title-tamper", ContentSetupV1::disabled(), 2);
        execute_with_port(
            &job,
            Intent::Initial,
            &CancellationToken::new(),
            &FakeGenerationPort::new(),
        )
        .unwrap();
        let path = job.work_dir.as_ref().unwrap().join(CONTENT_CURRENT_FILE);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["slots"]["faithful_text"]["current"]["blocks"][0]["title"] = serde_json::json!("");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            current(&job).unwrap(),
            ContentViewV1::RawOnly {
                current_issue: CurrentIssueV1::Corrupt,
                ..
            }
        ));
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn cancellation_before_execute_does_not_create_current() {
        let mut job = test_job("cancel", ContentSetupV1::disabled(), 2);
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            execute_with_port(&job, Intent::Initial, &token, &FakeGenerationPort::new()),
            Err(ContentError::Cancelled)
        ));
        assert!(!job
            .work_dir
            .as_ref()
            .unwrap()
            .join(CONTENT_CURRENT_FILE)
            .exists());
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn faithful_alignment_rejects_reordering_hallucination_and_number_loss() {
        assert!(validate_faithful_text_pair("甲乙", "乙甲").is_err());
        assert!(validate_faithful_text_pair("甲乙", "甲丙").is_err());
        assert!(validate_faithful_text_pair("数量 12 和 12", "数量 12 和 13").is_err());
        assert!(
            validate_faithful_text_pair("甲乙 12 https://a.test", "甲乙 12 https://a.test").is_ok()
        );
        let long_source: String = (0..100).map(|index| format!("词{index} ")).collect();
        let long_replaced = long_source.replacen("词5", "幻觉5", 1);
        assert!(validate_faithful_text_pair(&long_source, &long_replaced).is_err());
        assert!(validate_faithful_text_pair(
            "金额 $12.50，网址 HTTP://example.test/a",
            "金额 $12.50，网址 http://example.test/a"
        )
        .is_ok());
    }

    #[test]
    fn supporting_claims_require_refs_to_support_text_and_protected_values() {
        let mut job = test_job("supporting-claims", ContentSetupV1::disabled(), 1);
        let evidence = read_evidence(&job).unwrap();
        let positions: HashMap<String, usize> = evidence
            .utterances
            .iter()
            .enumerate()
            .map(|(index, utterance)| (utterance.id.clone(), index))
            .collect();
        let block = ContentBlockV1 {
            id: "unsupported".into(),
            role: ContentBlockRoleV1::Summary,
            title: None,
            text: "火星生活 999".into(),
            source_refs: vec![SourceRefV1 {
                utterance_ids: vec![evidence.utterances[0].id.clone()],
                start_ms: evidence.utterances[0].start_ms,
                end_ms: evidence.utterances[0].end_ms,
            }],
            source_status: SourceStatusV1::Mapped,
        };
        assert!(validate_supporting_block(
            ArtifactKindV1::DefaultSummary,
            &block,
            &evidence.utterances,
            &positions
        )
        .is_err());
        let partial = ContentBlockV1 {
            id: "partial".into(),
            role: ContentBlockRoleV1::Summary,
            title: Some("摘要".into()),
            text: "原始文本 火星火星火星".into(),
            source_refs: block.source_refs.clone(),
            source_status: SourceStatusV1::Mapped,
        };
        let chunk = validate_chunk_result(
            ArtifactKindV1::DefaultSummary,
            0,
            &evidence.utterances,
            vec![partial],
        )
        .unwrap();
        assert_eq!(chunk.blocks[0].source_status, SourceStatusV1::Limited);
        assert!(chunk.blocks[0].source_refs.is_empty());
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn chapters_require_titles_and_allow_mapped_limited_mix() {
        let mut job = test_job("chapter-partition", ContentSetupV1::disabled(), 3);
        let evidence = read_evidence(&job).unwrap();
        let positions: HashMap<String, usize> = evidence
            .utterances
            .iter()
            .enumerate()
            .map(|(index, utterance)| (utterance.id.clone(), index))
            .collect();
        let ref_for = |index: usize| SourceRefV1 {
            utterance_ids: vec![evidence.utterances[index].id.clone()],
            start_ms: evidence.utterances[index].start_ms,
            end_ms: evidence.utterances[index].end_ms,
        };
        let gap_blocks = vec![
            ContentBlockV1 {
                id: "c0".into(),
                role: ContentBlockRoleV1::Chapter,
                title: Some("第一章".into()),
                text: "第一段".into(),
                source_refs: vec![ref_for(0)],
                source_status: SourceStatusV1::Mapped,
            },
            ContentBlockV1 {
                id: "c1".into(),
                role: ContentBlockRoleV1::Chapter,
                title: Some("第三章".into()),
                text: "第三段".into(),
                source_refs: vec![ref_for(2)],
                source_status: SourceStatusV1::Mapped,
            },
        ];
        assert!(validate_chapter_partitions(
            &gap_blocks,
            &evidence.utterances,
            &positions,
            &[0, 2]
        )
        .is_err());
        let no_title = DerivationRecordV1 {
            schema_version: CONTENT_SCHEMA_VERSION,
            revision: 1,
            kind: ArtifactKindV1::Chapters,
            evidence_identity: evidence.descriptor.identity.clone(),
            source_context: expected_context(&job),
            provenance: build_provenance(
                ArtifactKindV1::Chapters,
                None,
                EffectivePathV1::Rules,
                None,
            ),
            blocks: vec![ContentBlockV1 {
                id: "no-title".into(),
                role: ContentBlockRoleV1::Chapter,
                title: None,
                text: "章节".into(),
                source_refs: vec![ref_for(0)],
                source_status: SourceStatusV1::Mapped,
            }],
            validation: ValidationReportV1 {
                processed_utterance_count: 3,
                total_utterance_count: 3,
                processing_coverage_complete: true,
                processing_chunks: vec![ProcessingChunkV1 {
                    chunk_index: 0,
                    input_utterance_ids: evidence
                        .utterances
                        .iter()
                        .map(|utterance| utterance.id.clone())
                        .collect(),
                    output_block_ids: vec!["no-title".into()],
                }],
                record_source_status: SourceStatusV1::Mapped,
                checks: required_validation_checks(ArtifactKindV1::Chapters)
                    .into_iter()
                    .map(|name| ValidationCheckV1 {
                        name: name.into(),
                        passed: true,
                        message: None,
                    })
                    .collect(),
            },
            created_at: Utc::now(),
        };
        assert!(validate_record_shape(&no_title).is_err());
        let limited = build_record(
            ArtifactKindV1::Chapters,
            &evidence.descriptor,
            &expected_context(&job),
            vec![ContentBlockV1 {
                id: "limited".into(),
                role: ContentBlockRoleV1::Chapter,
                title: Some("章节".into()),
                text: "无法精确关联的章节".into(),
                source_refs: Vec::new(),
                source_status: SourceStatusV1::Limited,
            }],
            vec![ProcessingChunkV1 {
                chunk_index: 0,
                input_utterance_ids: evidence
                    .utterances
                    .iter()
                    .map(|utterance| utterance.id.clone())
                    .collect(),
                output_block_ids: vec!["limited".into()],
            }],
            None,
            EffectivePathV1::Rules,
            None,
        )
        .unwrap();
        assert!(validate_record_against_evidence(&limited, &evidence.utterances).is_ok());

        let mixed = build_record(
            ArtifactKindV1::Chapters,
            &evidence.descriptor,
            &expected_context(&job),
            vec![
                ContentBlockV1 {
                    id: "mapped-first".into(),
                    role: ContentBlockRoleV1::Chapter,
                    title: Some("第一章".into()),
                    text: evidence.utterances[0].text.clone(),
                    source_refs: vec![ref_for(0)],
                    source_status: SourceStatusV1::Mapped,
                },
                ContentBlockV1 {
                    id: "limited-middle".into(),
                    role: ContentBlockRoleV1::Chapter,
                    title: Some("第二章".into()),
                    text: "无法精确关联的章节".into(),
                    source_refs: Vec::new(),
                    source_status: SourceStatusV1::Limited,
                },
                ContentBlockV1 {
                    id: "mapped-last".into(),
                    role: ContentBlockRoleV1::Chapter,
                    title: Some("第三章".into()),
                    text: evidence.utterances[2].text.clone(),
                    source_refs: vec![ref_for(2)],
                    source_status: SourceStatusV1::Mapped,
                },
            ],
            vec![ProcessingChunkV1 {
                chunk_index: 0,
                input_utterance_ids: evidence
                    .utterances
                    .iter()
                    .map(|utterance| utterance.id.clone())
                    .collect(),
                output_block_ids: vec![
                    "mapped-first".into(),
                    "limited-middle".into(),
                    "mapped-last".into(),
                ],
            }],
            None,
            EffectivePathV1::Rules,
            None,
        )
        .unwrap();
        assert_eq!(
            mixed.validation.record_source_status,
            SourceStatusV1::Limited
        );
        assert!(validate_record_against_evidence(&mixed, &evidence.utterances).is_ok());
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn chunk_ledger_rejects_missing_chunks_and_cross_chunk_refs() {
        let mut job = test_job("chunk-ledger", ready_setup(), 25);
        let evidence = read_evidence(&job).unwrap();
        let context = expected_context(&job);
        let chunks = chunk_utterances(&evidence.utterances);
        let fake = FakeGenerationPort::new();
        let mut results = Vec::new();
        for (chunk_index, chunk) in chunks.iter().enumerate() {
            let request = GenerationRequest {
                kind: ArtifactKindV1::DefaultSummary,
                chunk_index,
                chunk_count: chunks.len(),
                utterances: chunk.clone(),
                prompt: make_prompt(
                    ArtifactKindV1::DefaultSummary,
                    &context,
                    chunk,
                    chunk_index,
                    chunks.len(),
                ),
            };
            let generated = fake.generate(&test_target(), &request).unwrap();
            let blocks =
                convert_generated_blocks(ArtifactKindV1::DefaultSummary, chunk_index, generated)
                    .unwrap();
            results.push(
                validate_chunk_result(ArtifactKindV1::DefaultSummary, chunk_index, chunk, blocks)
                    .unwrap(),
            );
        }
        let mut missing = results.clone();
        missing.pop();
        assert!(merge_chunk_results(
            ArtifactKindV1::DefaultSummary,
            &evidence,
            &context,
            Some(&test_target()),
            EffectivePathV1::Llm,
            None,
            missing,
        )
        .is_err());
        let mut cross = results[0].clone();
        cross.blocks[0].source_refs[0].utterance_ids = vec![chunks[1][0].id.clone()];
        assert!(
            validate_chunk_result(ArtifactKindV1::DefaultSummary, 0, &chunks[0], cross.blocks,)
                .is_err()
        );
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn deep_load_rejects_tampered_coverage_and_provenance_checks() {
        let mut job = test_job("deep-tamper", ContentSetupV1::disabled(), 2);
        execute_with_port(
            &job,
            Intent::Initial,
            &CancellationToken::new(),
            &FakeGenerationPort::new(),
        )
        .unwrap();
        let path = job.work_dir.as_ref().unwrap().join(CONTENT_CURRENT_FILE);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["slots"]["faithful_text"]["current"]["validation"]["total_utterance_count"] =
            serde_json::json!(1);
        value["slots"]["faithful_text"]["current"]["provenance"]["recipe_id"] =
            serde_json::json!("wrong-recipe");
        value["slots"]["faithful_text"]["current"]["validation"]["checks"][0]["name"] =
            serde_json::json!("wrong-check");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            current(&job).unwrap(),
            ContentViewV1::RawOnly {
                current_issue: CurrentIssueV1::Corrupt,
                ..
            }
        ));
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn deep_load_rejects_deleted_tail_block_with_forged_full_counts() {
        let mut job = test_job("tail-block-tamper", ContentSetupV1::disabled(), 45);
        execute_with_port(
            &job,
            Intent::Initial,
            &CancellationToken::new(),
            &FakeGenerationPort::new(),
        )
        .unwrap();
        let path = job.work_dir.as_ref().unwrap().join(CONTENT_CURRENT_FILE);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let chunks = value["slots"]["faithful_text"]["current"]["validation"]["processing_chunks"]
            .as_array_mut()
            .unwrap();
        chunks.last_mut().unwrap()["output_block_ids"] = serde_json::json!([]);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            current(&job).unwrap(),
            ContentViewV1::RawOnly {
                current_issue: CurrentIssueV1::Corrupt,
                ..
            }
        ));
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn content_setup_cross_checks_faithful_and_enhancement_provenance() {
        let target = test_target();
        let disabled = ContentSetupV1::disabled();
        let ready = ContentSetupV1::ready(target.clone());
        let unavailable = ContentSetupV1::unavailable(TargetUnavailableV1 {
            code: TargetUnavailableCodeV1::EndpointInvalid,
            message: "本地 AI 地址不可用".into(),
            invalid_field: Some("endpoint".into()),
        });
        let rules = build_provenance(
            ArtifactKindV1::FaithfulText,
            None,
            EffectivePathV1::Rules,
            None,
        );
        assert!(validate_record_provenance_for_setup(
            ArtifactKindV1::FaithfulText,
            &rules,
            &disabled
        )
        .is_ok());
        assert!(
            validate_record_provenance_for_setup(ArtifactKindV1::FaithfulText, &rules, &ready)
                .is_err()
        );
        let ready_llm = build_provenance(
            ArtifactKindV1::FaithfulText,
            Some(&target),
            EffectivePathV1::Llm,
            None,
        );
        assert!(validate_record_provenance_for_setup(
            ArtifactKindV1::FaithfulText,
            &ready_llm,
            &ready
        )
        .is_ok());
        let unavailable_fallback = build_provenance(
            ArtifactKindV1::FaithfulText,
            None,
            EffectivePathV1::RulesFallback,
            Some(FallbackReasonV1 {
                code: FallbackReasonCodeV1::TargetUnavailable,
                message: "错误配置".into(),
            }),
        );
        assert!(validate_record_provenance_for_setup(
            ArtifactKindV1::FaithfulText,
            &unavailable_fallback,
            &unavailable
        )
        .is_err());
        let enhancement_rules = build_provenance(
            ArtifactKindV1::DefaultSummary,
            None,
            EffectivePathV1::Rules,
            None,
        );
        assert!(validate_record_provenance_for_setup(
            ArtifactKindV1::DefaultSummary,
            &enhancement_rules,
            &ready
        )
        .is_err());
    }

    #[test]
    fn evidence_rejects_overlong_single_utterance_but_accepts_chunk_budget_boundary() {
        let mut job = test_job("utterance-limit", ContentSetupV1::disabled(), 1);
        let path = job.work_dir.as_ref().unwrap().join("transcript.raw.json");
        let mut raw: Vec<Utterance> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        raw[0].text = "中".repeat(CHUNK_MAX_CHARS);
        fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        assert!(read_evidence(&job).is_ok());
        raw[0].text.push('中');
        fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        assert!(matches!(
            read_evidence(&job),
            Err(ContentError::Evidence(_))
        ));
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn model_spelling_is_canonical_and_legacy_current_is_zero_read() {
        let mut target = test_target();
        target.model = " fixture-model".into();
        assert!(target.validate().is_err());
        target.model = "fixture-model ".into();
        assert!(target.validate().is_err());
        let legacy = crate::jobs::Job::new(Uuid::new_v4(), "BVlegacy".into(), 1);
        assert!(matches!(current(&legacy).unwrap(), ContentViewV1::Legacy));
        assert!(matches!(
            execute(&legacy, Intent::Initial, &CancellationToken::new()),
            Err(ContentError::Legacy)
        ));
    }

    #[test]
    fn raw_duplicate_and_time_tamper_are_rejected_before_content_generation() {
        for (label, mutate) in [
            ("raw-duplicate", 0u8),
            ("raw-reversed", 1u8),
            ("raw-out-of-bounds", 2u8),
        ] {
            let mut job = test_job(label, ContentSetupV1::disabled(), 2);
            let path = job.work_dir.as_ref().unwrap().join("transcript.raw.json");
            let mut raw: Vec<Utterance> =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            match mutate {
                0 => raw[1].id = raw[0].id.clone(),
                1 => {
                    raw[0].start_ms = 1_000;
                    raw[0].end_ms = 1_900;
                    raw[1].start_ms = 0;
                }
                _ => raw[1].end_ms = job.duration_ms.unwrap() + 1,
            }
            fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
            assert!(matches!(current(&job), Err(ContentError::Evidence(_))));
            fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
        }
    }

    #[test]
    fn each_regeneration_only_changes_target_and_failure_keeps_old_revision() {
        let mut job = test_job("regen-matrix", ready_setup(), 2);
        let initial = execute_with_port(
            &job,
            Intent::Initial,
            &CancellationToken::new(),
            &FakeGenerationPort::new(),
        )
        .unwrap();
        let mut current_snapshot = initial.current.clone();
        for kind in [
            RegenerableKind::DefaultSummary,
            RegenerableKind::Highlights,
            RegenerableKind::Chapters,
        ] {
            let before = current_snapshot.clone();
            let regenerated = execute_with_port(
                &job,
                Intent::Regenerate {
                    kind,
                    target: test_target(),
                },
                &CancellationToken::new(),
                &FakeGenerationPort::new(),
            )
            .unwrap();
            let artifact = kind.artifact_kind();
            assert_eq!(
                regenerated
                    .current
                    .slots
                    .get(artifact)
                    .current
                    .as_ref()
                    .unwrap()
                    .revision,
                before
                    .slots
                    .get(artifact)
                    .current
                    .as_ref()
                    .unwrap()
                    .revision
                    + 1
            );
            for other in ArtifactKindV1::INITIAL_ALL {
                if other != artifact {
                    assert_eq!(
                        regenerated.current.slots.get(other),
                        before.slots.get(other)
                    );
                }
            }
            current_snapshot = regenerated.current;
        }
        let before_failure = current(&job).unwrap();
        let before_failure = match before_failure {
            ContentViewV1::Current(snapshot) => snapshot.current,
            _ => panic!("expected current"),
        };
        let failed = execute_with_port(
            &job,
            Intent::Regenerate {
                kind: RegenerableKind::Highlights,
                target: test_target(),
            },
            &CancellationToken::new(),
            &FakeGenerationPort::with_failure(ArtifactKindV1::Highlights),
        )
        .unwrap();
        assert_eq!(
            failed.current.slots.highlights.current,
            before_failure.slots.highlights.current
        );
        assert_eq!(
            failed
                .current
                .slots
                .highlights
                .last_failure
                .as_ref()
                .unwrap()
                .code,
            FailureCodeV1::GenerationFailed
        );
        let bytes = fs::read(job.work_dir.as_ref().unwrap().join(CONTENT_CURRENT_FILE)).unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            execute_with_port(
                &job,
                Intent::Regenerate {
                    kind: RegenerableKind::Chapters,
                    target: test_target(),
                },
                &token,
                &FakeGenerationPort::new(),
            ),
            Err(ContentError::Cancelled)
        ));
        assert_eq!(
            bytes,
            fs::read(job.work_dir.as_ref().unwrap().join(CONTENT_CURRENT_FILE)).unwrap()
        );
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    // ---- v0.6: summary tiers (schema v2) ----

    #[test]
    fn v1_bytes_decode_as_v2_with_empty_tier_slots_and_identity_unchanged() {
        let job_id = Uuid::new_v4();
        let mut value = fixture_current(job_id);
        value.schema_version = 1;
        let json_value = serde_json::to_value(&value).unwrap();
        // Simulate genuine v1 bytes: no summary-tier keys exist on disk.
        let mut json_value = json_value;
        json_value["slots"]
            .as_object_mut()
            .unwrap()
            .remove("short_summary");
        json_value["slots"]
            .as_object_mut()
            .unwrap()
            .remove("long_summary");
        let json = serde_json::to_vec(&json_value).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert!(raw["slots"].get("short_summary").is_none());
        assert!(raw["slots"].get("long_summary").is_none());

        let decoded: ContentCurrentV1 = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.schema_version, 1);
        decoded.validate().unwrap();
        assert!(decoded.slots.short_summary.current.is_none());
        assert!(decoded.slots.long_summary.current.is_none());
        // Evidence identity, revisions and provenance are untouched.
        assert_eq!(decoded.evidence.identity, value.evidence.identity);
        assert_eq!(
            decoded
                .slots
                .faithful_text
                .current
                .as_ref()
                .unwrap()
                .revision,
            value.slots.faithful_text.current.as_ref().unwrap().revision
        );
    }

    #[test]
    fn future_schema_version_is_rejected() {
        let job_id = Uuid::new_v4();
        let mut value = fixture_current(job_id);
        value.schema_version = CONTENT_SCHEMA_VERSION + 1;
        let json = serde_json::to_vec(&value).unwrap();
        let decoded: ContentCurrentV1 = serde_json::from_slice(&json).unwrap();
        assert!(matches!(
            decoded.validate(),
            Err(ContentSchemaError::SchemaVersion { .. })
        ));
    }

    #[test]
    fn initial_pipeline_never_calls_short_or_long_tiers() {
        let mut job = test_job("tiers-initial", ready_setup(), 2);
        let fake = FakeGenerationPort::new();
        let snapshot =
            execute_with_port(&job, Intent::Initial, &CancellationToken::new(), &fake).unwrap();
        assert_eq!(
            snapshot.current.slots.short_summary.current, None,
            "initial pipeline must not generate the short tier"
        );
        assert_eq!(snapshot.current.slots.long_summary.current, None);
        let called_kinds: Vec<ArtifactKindV1> = fake
            .calls
            .borrow()
            .iter()
            .map(|(kind, _, _)| *kind)
            .collect();
        assert!(!called_kinds.contains(&ArtifactKindV1::ShortSummary));
        assert!(!called_kinds.contains(&ArtifactKindV1::LongSummary));
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn first_generation_via_regenerate_writes_revision_one_and_keeps_other_slots() {
        let mut job = test_job("tiers-first", ready_setup(), 2);
        let initial = execute_with_port(
            &job,
            Intent::Initial,
            &CancellationToken::new(),
            &FakeGenerationPort::new(),
        )
        .unwrap();
        let before = initial.current.clone();
        let generated = execute_with_port(
            &job,
            Intent::Regenerate {
                kind: RegenerableKind::ShortSummary,
                target: test_target(),
            },
            &CancellationToken::new(),
            &FakeGenerationPort::new(),
        )
        .unwrap();
        let short = generated
            .current
            .slots
            .short_summary
            .current
            .as_ref()
            .expect("first tier generation writes revision 1");
        assert_eq!(short.revision, 1);
        assert_eq!(
            short.provenance.recipe_id,
            recipe_id(ArtifactKindV1::ShortSummary)
        );
        // Every other slot is untouched.
        for kind in [
            ArtifactKindV1::FaithfulText,
            ArtifactKindV1::DefaultSummary,
            ArtifactKindV1::Highlights,
            ArtifactKindV1::Chapters,
        ] {
            assert_eq!(generated.current.slots.get(kind), before.slots.get(kind));
        }
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn tier_failure_only_updates_that_tier_and_keeps_old_current() {
        let mut job = test_job("tiers-failure", ready_setup(), 2);
        execute_with_port(
            &job,
            Intent::Initial,
            &CancellationToken::new(),
            &FakeGenerationPort::new(),
        )
        .unwrap();
        let first_short = execute_with_port(
            &job,
            Intent::Regenerate {
                kind: RegenerableKind::ShortSummary,
                target: test_target(),
            },
            &CancellationToken::new(),
            &FakeGenerationPort::new(),
        )
        .unwrap()
        .current
        .slots
        .short_summary
        .clone();
        let failed = execute_with_port(
            &job,
            Intent::Regenerate {
                kind: RegenerableKind::LongSummary,
                target: test_target(),
            },
            &CancellationToken::new(),
            &FakeGenerationPort::with_failure(ArtifactKindV1::LongSummary),
        )
        .unwrap();
        // Short tier keeps its current; long tier records only its own failure.
        assert_eq!(failed.current.slots.short_summary, first_short);
        assert!(failed.current.slots.short_summary.last_failure.is_none());
        assert_eq!(
            failed
                .current
                .slots
                .long_summary
                .last_failure
                .as_ref()
                .unwrap()
                .code,
            FailureCodeV1::GenerationFailed
        );
        assert!(failed.current.slots.highlights.last_failure.is_none());
        fs::remove_dir_all(job.work_dir.take().unwrap()).unwrap();
    }

    #[test]
    fn tier_length_contract_rejects_oversized_and_undersized_responses() {
        // Short tier hard ceiling: 300 CJK chars (200 × 1.5).
        let oversized = "短".repeat(301);
        let blocks = |text: &str| {
            vec![ContentBlockV1 {
                id: "tier".into(),
                role: ContentBlockRoleV1::Summary,
                title: None,
                text: text.into(),
                source_refs: Vec::new(),
                source_status: SourceStatusV1::Limited,
            }]
        };
        assert!(
            validate_summary_tier_length(ArtifactKindV1::ShortSummary, &blocks(&oversized))
                .is_err()
        );
        let fitting = "短".repeat(300);
        assert!(
            validate_summary_tier_length(ArtifactKindV1::ShortSummary, &blocks(&fitting)).is_ok()
        );

        // Long tier soft floor: 400 CJK chars (800 ÷ 2).
        let undersized = "长".repeat(399);
        assert!(
            validate_summary_tier_length(ArtifactKindV1::LongSummary, &blocks(&undersized))
                .is_err()
        );
        let adequate = "长".repeat(400);
        assert!(
            validate_summary_tier_length(ArtifactKindV1::LongSummary, &blocks(&adequate)).is_ok()
        );

        // Standard tier length contract (150–750 CJK chars).
        let standard_valid = "标".repeat(350);
        assert!(validate_summary_tier_length(
            ArtifactKindV1::DefaultSummary,
            &blocks(&standard_valid)
        )
        .is_ok());
        let standard_oversized = "标".repeat(751);
        assert!(validate_summary_tier_length(
            ArtifactKindV1::DefaultSummary,
            &blocks(&standard_oversized)
        )
        .is_err());
    }

    #[test]
    fn tier_prompts_differ_from_the_standard_recipe_prompt() {
        let context = SourceContextSnapshotV1 {
            main_title: "标题".into(),
            part_title: None,
            terms: Vec::new(),
        };
        let chunk = vec![Utterance {
            id: "u1".into(),
            text: "内容".into(),
            start_ms: 0,
            end_ms: 1000,
            speaker_id: 0,
        }];
        let standard = make_prompt(ArtifactKindV1::DefaultSummary, &context, &chunk, 0, 1);
        let short = make_prompt(ArtifactKindV1::ShortSummary, &context, &chunk, 0, 1);
        let long = make_prompt(ArtifactKindV1::LongSummary, &context, &chunk, 0, 1);
        assert!(short.contains("summary-short"));
        assert!(standard.contains("summary-standard"));
        assert!(long.contains("summary-long"));
        assert!(!standard.contains("summary-short") && !standard.contains("summary-long"));
        assert_ne!(short, standard);
        assert_ne!(long, standard);
    }

    #[test]
    fn english_summary_tier_contracts_follow_word_counts() {
        let blocks = |text: &str| {
            vec![ContentBlockV1 {
                id: "tier-en".into(),
                role: ContentBlockRoleV1::Summary,
                title: None,
                text: text.into(),
                source_refs: Vec::new(),
                source_status: SourceStatusV1::Limited,
            }]
        };
        // Short tier English limit: 180 words.
        let short_oversized = vec!["word"; 181].join(" ");
        assert!(validate_summary_tier_length(
            ArtifactKindV1::ShortSummary,
            &blocks(&short_oversized)
        )
        .is_err());

        let short_valid = vec!["word"; 120].join(" ");
        assert!(
            validate_summary_tier_length(ArtifactKindV1::ShortSummary, &blocks(&short_valid))
                .is_ok()
        );

        // Long tier English minimum: 240 words.
        let long_undersized = vec!["word"; 239].join(" ");
        assert!(validate_summary_tier_length(
            ArtifactKindV1::LongSummary,
            &blocks(&long_undersized)
        )
        .is_err());

        let long_valid = vec!["word"; 300].join(" ");
        assert!(
            validate_summary_tier_length(ArtifactKindV1::LongSummary, &blocks(&long_valid)).is_ok()
        );
    }

    #[test]
    fn parse_search_query_supports_plain_text_and_time_formats() {
        assert_eq!(parse_search_query(""), None);
        assert_eq!(parse_search_query("   "), None);
        assert_eq!(
            parse_search_query("普通文本"),
            Some(SearchQuery::Text("普通文本".into()))
        );
        assert_eq!(
            parse_search_query("01:23"),
            Some(SearchQuery::TimeRange(53_000, 113_000))
        );
        assert_eq!(
            parse_search_query("01:00-02:30"),
            Some(SearchQuery::TimeRange(60_000, 150_000))
        );

        // Huge time inputs that would overflow checked arithmetic safely return None or Text.
        assert_eq!(
            parse_search_query("18446744073709551615:00"),
            Some(SearchQuery::Text("18446744073709551615:00".into()))
        );
        assert_eq!(
            parse_search_query("999999999999999999:59:59"),
            Some(SearchQuery::Text("999999999999999999:59:59".into()))
        );
        assert_eq!(
            parse_search_query("01:00-999999999999999999:00"),
            Some(SearchQuery::Text("01:00-999999999999999999:00".into()))
        );
    }

    #[test]
    fn model_missing_404_error_matching_recognizes_model_not_found() {
        let body = serde_json::json!({
            "error": {
                "message": "model 'llama3' not found, try pulling it first",
                "type": "invalid_request_error"
            }
        });
        assert!(is_model_missing_error(404, Some(&body)));

        let unknown_body = serde_json::json!({
            "error": {
                "message": "route not found"
            }
        });
        assert!(!is_model_missing_error(404, Some(&unknown_body)));
    }

    #[test]
    fn highlight_snippet_case_insensitive_and_preserves_casing() {
        let text = "学习 Rust 语言非常有趣。Rust 是系统级编程语言。";
        let snippet = highlight_snippet(text, "rust");
        assert!(snippet.contains("【Rust】"));

        let en_text = "This is a fast python script.";
        let en_snippet = highlight_snippet(en_text, "Python");
        assert!(en_snippet.contains("【python】"));

        // Unicode character expansion test: "aİx" where 'İ' expands under lowercasing
        let turkish_text = "aİx";
        let turkish_snippet = highlight_snippet(turkish_text, "x");
        assert_eq!(turkish_snippet, "aİ【x】");

        // Unicode case-fold test: "Straße" matches "STRASSE"
        let german_text = "Die Straße ist lang.";
        let german_snippet = highlight_snippet(german_text, "STRASSE");
        assert!(german_snippet.contains("【Straße】"));
    }

    #[test]
    fn classify_probe_and_test_connection_covers_all_failure_categories() {
        assert_eq!(classify_probe(ProbeOutcome::Success), Ok(()));
        assert_eq!(
            classify_probe(ProbeOutcome::Transport),
            Err(ConnectionTestFailure::Unreachable)
        );
        assert_eq!(
            classify_probe(ProbeOutcome::Timeout),
            Err(ConnectionTestFailure::Timeout)
        );
        assert_eq!(
            classify_probe(ProbeOutcome::MalformedBody),
            Err(ConnectionTestFailure::ProtocolMismatch)
        );

        // Model not found (404 with structured error)
        let model_404 = serde_json::json!({
            "error": { "code": "model_not_found", "message": "The model `qwen` does not exist" }
        })
        .to_string();
        assert_eq!(
            classify_probe(ProbeOutcome::HttpError(404, model_404)),
            Err(ConnectionTestFailure::ModelUnavailable)
        );

        // Protocol mismatch (404 without model error)
        assert_eq!(
            classify_probe(ProbeOutcome::HttpError(404, "404 Not Found".into())),
            Err(ConnectionTestFailure::ProtocolMismatch)
        );

        // Address validation before network request
        let mut invalid_target = test_target();
        invalid_target.endpoint = "https://api.openai.com/v1".into(); // remote rejected for local-only
        assert_eq!(
            test_connection(&invalid_target),
            Err(ConnectionTestFailure::InvalidAddress)
        );
    }
}
