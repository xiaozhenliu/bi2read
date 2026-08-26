//! Markdown document generation.
//!
//! Produces `transcript.raw.md` (the untouched FunASR output) and `full.md`
//! (the readable document). The raw markdown never goes through an LLM and is
//! never deleted if the readable version fails. Each `.md`
//! embeds the job id so `state.json` recovery can verify it exists for the right
//! job.

use std::collections::BTreeSet;
use std::path::Path;

use crate::content_results::{
    ArtifactKindV1, ContentBlockV1, ContentSnapshotV1, EffectivePathV1, SourceStatusV1,
};
use crate::funasr::Utterance;
use crate::jobs::Job;

/// Which summary tier a rendered/exported document contains. The default is
/// the standard tier; the UI passes the tier currently selected in the result
/// view (Spec §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryTier {
    Short,
    Standard,
    Long,
}

impl SummaryTier {
    pub(crate) fn artifact_kind(self) -> ArtifactKindV1 {
        match self {
            Self::Short => ArtifactKindV1::ShortSummary,
            Self::Standard => ArtifactKindV1::DefaultSummary,
            Self::Long => ArtifactKindV1::LongSummary,
        }
    }

    /// Resolve the tier actually rendered: only renders the selected tier
    /// when present, without falling back to a different summary tier.
    fn resolve(&self, snapshot: &ContentSnapshotV1) -> Option<ArtifactKindV1> {
        let selected = self.artifact_kind();
        if snapshot.current.slots.get(selected).current.is_some() {
            Some(selected)
        } else {
            None
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentError {
    #[error("document I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("document snapshot: {0}")]
    Snapshot(String),
}

/// Metadata needed to render the document header. Populated at the `metadata`
/// stage and stored in `metadata.json`; passed here so the renderer is pure.
#[derive(Debug, Clone)]
pub struct DocMeta {
    pub title: String,
    pub up_name: String,
    pub duration_ms: u64,
    pub bvid: String,
    pub page: u32,
    /// Original input retained for persisted metadata compatibility. Document
    /// links use the resolved `bvid` and `page` above so bare ids, legacy av
    /// ids, full URLs and resolved short links all produce the same URL shape.
    pub source_url: Option<String>,
}

impl DocMeta {
    /// The canonical bilibili video URL, including the page if > 1.
    pub fn video_url(&self) -> String {
        crate::bilibili::video_url(&self.bvid, self.page, None)
    }
}

fn render_utterances(job: &Job, utterances: &[Utterance], refined: Option<&[String]>) -> String {
    let mut s = String::new();
    let refined = refined.filter(|items| items.len() == utterances.len());
    for (index, u) in utterances.iter().enumerate() {
        let text = refined
            .and_then(|items| items.get(index))
            .filter(|text| !text.trim().is_empty())
            .map(String::as_str)
            .unwrap_or(&u.text);
        s.push_str(&format!(
            "**[{}]** [{}]({})\n{}\n\n",
            job.speaker_name(u.speaker_id),
            fmt_ts(u.start_ms),
            crate::bilibili::video_url(&job.bvid, job.page, Some(u.start_ms)),
            text
        ));
    }
    s
}

/// Generate `transcript.raw.md` from raw utterances.
///
/// Untouched FunASR output: per utterance, the speaker label (mapped through the
/// job's speaker_map), a start timestamp, and the full text. No LLM, no edits.
/// Embeds the job id for recovery verification.
pub fn render_raw_markdown(job: &Job, utterances: &[Utterance]) -> String {
    let mut s = String::new();
    s.push_str("# 原始逐字稿\n\n");
    s.push_str(&format!("<!-- job-id: {} -->\n\n", job.id));
    s.push_str("> 本稿未经过 LLM，是 FunASR 的完整输出，不删减。\n\n");
    if utterances.is_empty() {
        s.push_str("（无转写内容）\n");
        return s;
    }
    s.push_str(&render_utterances(job, utterances, None));
    s
}

/// Generate the readable transcript body.
///
/// If LLM-refined `readable` text is provided and passes validation, it is used;
/// otherwise the raw utterances are rendered with speaker labels and timestamps
/// as a rule-based fallback. LLM failure must never prevent document output.
pub fn render_readable_body(
    job: &Job,
    utterances: &[Utterance],
    refined: Option<&[String]>,
) -> String {
    let refined = refined.filter(|items| items.len() == utterances.len());
    let mut body = String::new();
    let mut index = 0;

    while index < utterances.len() {
        let first = &utterances[index];
        let speaker_id = first.speaker_id;
        let mut text = String::new();

        while index < utterances.len() && utterances[index].speaker_id == speaker_id {
            let utterance = &utterances[index];
            let segment = refined
                .and_then(|items| items.get(index))
                .filter(|candidate| !candidate.trim().is_empty())
                .map(String::as_str)
                .unwrap_or(&utterance.text);
            // Deliberately add no separator or punctuation: the readable body
            // must preserve the selected segment text byte-for-byte and only
            // remove repeated metadata between adjacent segments.
            text.push_str(segment);
            index += 1;
        }

        body.push_str(&format!(
            "**[{}]** [{}]({})\n{}\n\n",
            job.speaker_name(speaker_id),
            fmt_ts(first.start_ms),
            crate::bilibili::video_url(&job.bvid, job.page, Some(first.start_ms)),
            text
        ));
    }

    body
}

/// Render a v0.5 Presentation exclusively from the validated snapshot.
///
/// The snapshot already contains the raw Evidence view and the validated
/// faithful/AI records. This function intentionally has no filesystem reads
/// other than writing its three output files, so a stale or hand-edited raw
/// transcript cannot silently become the displayed document.
pub(crate) fn rebuild_presentation(
    job: &Job,
    snapshot: &ContentSnapshotV1,
) -> Result<(), DocumentError> {
    rebuild_presentation_with_tier(job, snapshot, SummaryTier::Standard)
}

/// Same as [`rebuild_presentation`] but renders the given summary tier into
/// `full.md`. When the selected tier has not been generated, no summary section
/// is written at all (never silently writes another tier).
pub(crate) fn rebuild_presentation_with_tier(
    job: &Job,
    snapshot: &ContentSnapshotV1,
    summary_tier: SummaryTier,
) -> Result<(), DocumentError> {
    let Some(work_dir) = job.work_dir.as_deref() else {
        return Err(DocumentError::Snapshot("任务没有工作目录".into()));
    };
    if snapshot.current.job_id != job.id {
        return Err(DocumentError::Snapshot("snapshot 与任务 ID 不匹配".into()));
    }
    if snapshot.current.evidence != snapshot.evidence.descriptor {
        return Err(DocumentError::Snapshot(
            "snapshot Evidence descriptor 不匹配".into(),
        ));
    }
    let faithful = snapshot
        .current
        .slots
        .get(ArtifactKindV1::FaithfulText)
        .current
        .as_ref()
        .ok_or_else(|| DocumentError::Snapshot("snapshot 缺少忠实正文".into()))?;

    let raw = render_raw_markdown(job, &snapshot.evidence.utterances);
    let readable = render_faithful_body(
        job,
        faithful.blocks.as_slice(),
        &snapshot.evidence.utterances,
    );
    let raw_path = work_dir.join("transcript.raw.md");
    let meta = DocMeta {
        title: job.title.clone(),
        up_name: job.up_name.clone().unwrap_or_default(),
        duration_ms: job.duration_ms.unwrap_or(0),
        bvid: job.bvid.clone(),
        page: job.page,
        source_url: job.source_url.clone(),
    };
    let full =
        render_snapshot_full_markdown(job, &meta, snapshot, &readable, &raw_path, summary_tier);

    crate::jobs::atomic_write(&raw_path, raw.as_bytes())?;
    crate::jobs::atomic_write(
        &work_dir.join("transcript.readable.md"),
        readable.as_bytes(),
    )?;

    let output_dir = job
        .final_output_dir
        .as_deref()
        .ok_or_else(|| DocumentError::Snapshot("任务没有最终输出目录".into()))?;
    std::fs::create_dir_all(output_dir)?;
    crate::jobs::atomic_write(&output_dir.join("full.md"), full.as_bytes())?;
    Ok(())
}

/// Render the faithful record while preserving source links for mapped
/// blocks. A limited block is shown honestly without inventing a timestamp.
fn render_faithful_body(job: &Job, blocks: &[ContentBlockV1], utterances: &[Utterance]) -> String {
    let by_id = utterances
        .iter()
        .map(|utterance| (utterance.id.as_str(), utterance))
        .collect::<std::collections::HashMap<_, _>>();
    let mut body = String::new();
    for block in blocks {
        let first = block
            .source_refs
            .first()
            .and_then(|source_ref| source_ref.utterance_ids.first())
            .and_then(|id| by_id.get(id.as_str()).copied());
        let speaker = first
            .map(|utterance| job.speaker_name(utterance.speaker_id))
            .unwrap_or_else(|| "来源受限".into());
        if block.source_status == SourceStatusV1::Mapped {
            if let Some(utterance) = first {
                body.push_str(&format!(
                    "**[{}]** [{}]({})\n{}\n\n",
                    speaker,
                    fmt_ts(utterance.start_ms),
                    crate::bilibili::video_url(&job.bvid, job.page, Some(utterance.start_ms)),
                    block.text
                ));
                continue;
            }
        }
        body.push_str(&format!("> 来源受限\n{}\n\n", block.text));
    }
    body
}

fn render_snapshot_full_markdown(
    job: &Job,
    meta: &DocMeta,
    snapshot: &ContentSnapshotV1,
    faithful_body: &str,
    raw_path: &Path,
    summary_tier: SummaryTier,
) -> String {
    // Reuse the stable legacy header and faithful body formatting, then insert
    // the structured v0.5 records before the full transcript section.
    let mut document = render_full_markdown(
        job,
        meta,
        &snapshot.evidence.utterances,
        Some(faithful_body),
        raw_path,
    );
    document = rewrite_reading_time_metadata(document, job, snapshot, summary_tier);
    if let Some(index) = document.find("## 全文") {
        document.insert_str(
            index,
            &render_snapshot_sections(job, snapshot, summary_tier),
        );
    }
    document
}

/// Replace the legacy char-count/5k heuristic in the metadata block with the
/// versioned `reading-profile v1` estimate computed from the same primary
/// consumption artifact the App UI uses. Missing profile keeps video duration
/// and scale but omits reading/saved lines — never fabricated numbers.
fn rewrite_reading_time_metadata(
    mut document: String,
    job: &Job,
    snapshot: &ContentSnapshotV1,
    summary_tier: SummaryTier,
) -> String {
    let language = job
        .transcription_result
        .as_ref()
        .and_then(|result| result.reported_language)
        .or_else(|| {
            job.transcription_selection
                .as_ref()
                .map(|selection| selection.requested_language)
        });
    let record = crate::content_results::primary_consumption_record(
        snapshot,
        Some(summary_tier.artifact_kind()),
    );
    let text = record
        .map(crate::content_results::record_plain_text)
        .unwrap_or_default();
    let estimate = crate::reading_time::estimate(&text, job.duration_ms, language);

    let legacy_start = match document.find("- 字符数: ") {
        Some(index) => index,
        None => return document,
    };
    let legacy_end = document[legacy_start..]
        .find("\n")
        .map(|offset| legacy_start + offset + 1)
        .unwrap_or(document.len());
    // The legacy block is exactly two lines (字符数 + 阅读时间).
    let legacy_end = if document[legacy_start..].contains("阅读时间")
        && document[legacy_end..].starts_with("- 阅读时间")
    {
        document[legacy_end..]
            .find('\n')
            .map(|offset| legacy_end + offset + 1)
            .unwrap_or(legacy_end)
    } else {
        legacy_end
    };
    let scale = estimate.scale;
    let mut replacement = format!("- 正文规模: 约 {}\n", scale.format_scale());
    if let (Some(reading), Some(saved)) = (estimate.reading_ms, estimate.saved_ms) {
        replacement.push_str(&format!(
            "- 估算口径: {}（中文 400 字/分、英文 240 词/分、1.0x 倍速；依据 Brysbaert 2019 与 2024 中文阅读实验）\n",
            crate::reading_time::READING_PROFILE_VERSION
        ));
        if let Some(video_ms) = estimate.video_ms {
            replacement.push_str(&format!("- 视频时长: {}\n", fmt_duration(video_ms)));
        }
        replacement.push_str(&format!(
            "- 预计阅读: 约 {} 分钟\n- 预计节省: 约 {} 分钟\n",
            (reading as f64 / 60_000.0).ceil() as u64,
            (saved as f64 / 60_000.0).ceil() as u64
        ));
    } else if let Some(video_ms) = estimate.video_ms {
        replacement.push_str(&format!("- 视频时长: {}\n", fmt_duration(video_ms)));
    }
    document.replace_range(legacy_start..legacy_end, &replacement);
    document
}

fn render_snapshot_sections(
    job: &Job,
    snapshot: &ContentSnapshotV1,
    summary_tier: SummaryTier,
) -> String {
    let mut sections = String::new();
    sections.push_str("## 内容来源说明\n\n");
    sections.push_str(
        "- 原始稿：FunASR Evidence 的完整输出，未经过 LLM；原始 Markdown 位于任务工作目录的 `transcript.raw.md`。\n",
    );
    sections.push_str(&format!(
        "- Evidence identity：`{}`；来源片段数：{}。\n\n",
        snapshot.current.evidence.identity, snapshot.current.evidence.utterance_count
    ));

    let faithful_slot = snapshot.current.slots.get(ArtifactKindV1::FaithfulText);
    if let Some(record) = faithful_slot.current.as_ref() {
        sections.push_str("## 忠实整理\n\n");
        sections.push_str(&format_provenance(record));
        sections
            .push_str("忠实正文只允许对 Evidence 做保序、可核对的整理；正文主体见下方“全文”。\n\n");
    }
    // `full.md` renders exactly one summary tier: the resolved selection.
    // Other already-generated tiers stay in the structured result and appear
    // after switching tiers and re-exporting; ungenerated tiers leave no
    // placeholder section at all.
    let summary_kinds: Vec<ArtifactKindV1> = summary_tier.resolve(snapshot).into_iter().collect();
    for kind in summary_kinds {
        let slot = snapshot.current.slots.get(kind);
        if let Some(record) = slot.current.as_ref() {
            sections.push_str("## 摘要\n\n");
            sections.push_str(&format_provenance(record));
            sections.push_str(&format!(
                "> 来源{}\n\n",
                source_status_label(record.validation.record_source_status)
            ));
            for block in &record.blocks {
                if let Some(title) = block.title.as_deref() {
                    sections.push_str(&format!("### {}\n\n", title.trim()));
                }
                if block.source_status == SourceStatusV1::Limited {
                    sections.push_str("> 来源受限\n\n");
                }
                sections.push_str(block.text.trim());
                sections.push_str("\n\n");
                if block.source_status == SourceStatusV1::Mapped {
                    let source = render_block_sources(job, snapshot, block);
                    if !source.is_empty() {
                        sections.push_str(&format!("来源：{}\n\n", source));
                    }
                }
            }
        }
    }
    for (kind, heading) in [
        (ArtifactKindV1::Highlights, "## 重点"),
        (ArtifactKindV1::Chapters, "## 章节"),
    ] {
        let slot = snapshot.current.slots.get(kind);
        let Some(record) = slot.current.as_ref() else {
            if let Some(failure) = slot.last_failure.as_ref() {
                sections.push_str(&format!(
                    "{heading}\n\n> 当前未生成：{}\n\n",
                    failure.message
                ));
            }
            continue;
        };
        sections.push_str(&format!(
            "{heading}\n\n{}> 来源{}\n\n",
            format_provenance(record),
            source_status_label(record.validation.record_source_status),
        ));
        for block in &record.blocks {
            if let Some(title) = block.title.as_deref() {
                sections.push_str(&format!("### {}\n\n", title.trim()));
            }
            if block.source_status == SourceStatusV1::Limited {
                sections.push_str("> 来源受限\n\n");
            }
            sections.push_str(block.text.trim());
            sections.push_str("\n\n");
            if block.source_status == SourceStatusV1::Mapped {
                let source = render_block_sources(job, snapshot, block);
                if !source.is_empty() {
                    sections.push_str(&format!("来源：{}\n\n", source));
                }
            }
        }
    }
    sections
}

fn format_provenance(record: &crate::content_results::DerivationRecordV1) -> String {
    let provenance = &record.provenance;
    let mut line = format!(
        "> 类型：{}；路径：{}；revision {}；生成时间：{}；recipe {} v{}；rules {} v{}。\n",
        artifact_kind_label(record.kind),
        effective_path_label(provenance.effective_path),
        record.revision,
        record.created_at.to_rfc3339(),
        provenance.recipe_id,
        provenance.recipe_version,
        provenance.rules_id,
        provenance.rules_version,
    );
    if let (Some(prompt_id), Some(prompt_version)) =
        (provenance.prompt_id.as_deref(), provenance.prompt_version)
    {
        line.push_str(&format!("> prompt {prompt_id} v{prompt_version}。\n"));
    }
    if let Some(provider) = provenance.provider {
        line.push_str(&format!(
            "> AI：{} / {}；endpoint：`{}`；模型：`{}`；参数：{}。\n",
            provider_label(provider),
            api_format_label(provenance.api_format),
            provenance.endpoint.as_deref().unwrap_or("未提供"),
            provenance.model.as_deref().unwrap_or("未提供"),
            parameters_label(provenance.parameters.as_ref()),
        ));
    }
    if let Some(reason) = provenance.fallback_reason.as_ref() {
        line.push_str(&format!(
            "> 回退原因：{}（{}）。\n",
            fallback_reason_code_label(reason.code),
            reason.message
        ));
    }
    line
}

fn fallback_reason_code_label(code: crate::content_results::FallbackReasonCodeV1) -> &'static str {
    match code {
        crate::content_results::FallbackReasonCodeV1::TargetUnavailable => {
            "target_unavailable / AI 配置不可用"
        }
        crate::content_results::FallbackReasonCodeV1::GenerationFailed => {
            "generation_failed / AI 生成失败"
        }
        crate::content_results::FallbackReasonCodeV1::ResponseInvalid => {
            "response_invalid / AI 响应未通过校验"
        }
    }
}

fn render_block_sources(job: &Job, snapshot: &ContentSnapshotV1, block: &ContentBlockV1) -> String {
    let by_id = snapshot
        .evidence
        .utterances
        .iter()
        .map(|utterance| (utterance.id.as_str(), utterance))
        .collect::<std::collections::HashMap<_, _>>();
    block
        .source_refs
        .iter()
        .filter_map(|source_ref| {
            let first = source_ref
                .utterance_ids
                .first()
                .and_then(|id| by_id.get(id.as_str()).copied())?;
            let last = source_ref
                .utterance_ids
                .last()
                .and_then(|id| by_id.get(id.as_str()).copied())?;
            let span =
                crate::bilibili::source_span(std::iter::once((first.start_ms, last.end_ms)))?;
            Some(format!(
                "[{}]({})–[{}]({})",
                fmt_ts(span.start_ms),
                crate::bilibili::video_url(&job.bvid, job.page, Some(span.start_ms)),
                fmt_ts(span.end_ms),
                crate::bilibili::video_url(&job.bvid, job.page, Some(span.end_ms)),
            ))
        })
        .collect::<Vec<_>>()
        .join("、")
}

fn artifact_kind_label(kind: ArtifactKindV1) -> &'static str {
    match kind {
        ArtifactKindV1::FaithfulText => "忠实正文",
        ArtifactKindV1::DefaultSummary => "默认摘要",
        ArtifactKindV1::ShortSummary => "短摘要",
        ArtifactKindV1::LongSummary => "长摘要",
        ArtifactKindV1::Highlights => "重点",
        ArtifactKindV1::Chapters => "章节",
    }
}

fn provider_label(provider: crate::content_results::GenerationProviderV1) -> &'static str {
    match provider {
        crate::content_results::GenerationProviderV1::OpenAiCompatible => "OpenAI-compatible",
        crate::content_results::GenerationProviderV1::AnthropicCompatible => "Anthropic-compatible",
    }
}

fn api_format_label(
    api_format: Option<crate::content_results::GenerationApiFormatV1>,
) -> &'static str {
    match api_format {
        Some(crate::content_results::GenerationApiFormatV1::OpenAiChatCompletions) => {
            "chat-completions"
        }
        Some(crate::content_results::GenerationApiFormatV1::AnthropicMessages) => "messages",
        None => "未使用",
    }
}

fn parameters_label(parameters: Option<&crate::content_results::GenerationParametersV1>) -> String {
    match parameters {
        Some(crate::content_results::GenerationParametersV1::OpenAiChatCompletions {
            temperature,
        }) => format!("temperature={temperature}"),
        Some(crate::content_results::GenerationParametersV1::AnthropicMessages { max_tokens }) => {
            format!("max_tokens={max_tokens}")
        }
        None => "none".into(),
    }
}

fn effective_path_label(path: EffectivePathV1) -> &'static str {
    match path {
        EffectivePathV1::Rules => "规则生成",
        EffectivePathV1::Llm => "AI 生成",
        EffectivePathV1::RulesFallback => "规则回退",
    }
}

fn source_status_label(status: SourceStatusV1) -> &'static str {
    match status {
        SourceStatusV1::Mapped => "可映射",
        SourceStatusV1::Limited => "受限",
    }
}

/// Generate `full.md`. Embeds the job id for recovery verification.
pub fn render_full_markdown(
    job: &Job,
    meta: &DocMeta,
    utterances: &[Utterance],
    readable: Option<&str>,
    raw_md_path: &Path,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {}\n\n", meta.title));
    s.push_str(&format!("<!-- job-id: {} -->\n\n", job.id));
    s.push_str(&format!("- UP主: {}\n", meta.up_name));
    s.push_str(&format!("- 时长: {}\n", fmt_duration(meta.duration_ms)));
    let video_url = meta.video_url();
    s.push_str(&format!("- 原视频链接: [{video_url}]({video_url})\n"));
    if let Some(selection) = &job.transcription_selection {
        s.push_str(&format!(
            "- Runtime: {}\n",
            selection.runtime_source.label()
        ));
        s.push_str(&format!(
            "- Runtime source: {}\n",
            selection.runtime_source.as_str()
        ));
        s.push_str(&format!(
            "- Runtime backend: {}\n",
            selection.runtime_backend.as_str()
        ));
        s.push_str(&format!(
            "- Runtime identity: {}\n",
            selection.runtime_identity
        ));
        s.push_str(&format!(
            "- 模型说明: {}\n",
            selection.model_description.as_deref().unwrap_or("未提供")
        ));
        s.push_str(&format!(
            "- 请求语言: {}\n",
            selection.requested_language.label()
        ));
    } else {
        s.push_str("- Runtime: 未记录（legacy-unrecorded）\n");
        s.push_str("- 请求语言: 未记录\n");
    }
    if let Some(result) = &job.transcription_result {
        s.push_str(&format!(
            "- 实际语言: {}\n",
            result
                .reported_language
                .map(crate::jobs::SourceLanguage::label)
                .unwrap_or("未报告")
        ));
        s.push_str(&format!(
            "- 实际模型: {}\n",
            result.reported_model.as_deref().unwrap_or("未报告")
        ));
        s.push_str(&format!(
            "- 实际 Runtime identity: {}\n",
            result
                .reported_runtime_identity
                .as_deref()
                .unwrap_or("未报告")
        ));
    } else {
        s.push_str("- 实际语言: 未报告\n");
        s.push_str("- 实际模型: 未报告\n");
        s.push_str("- 实际 Runtime identity: 未报告\n");
    }

    let body = readable
        .filter(|body| !body.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| render_readable_body(job, utterances, None));
    let char_count = body.chars().count();
    s.push_str(&format!("- 字符数: {}\n", char_count));
    let read_mins = (char_count as f64 / 5000.0).ceil() as u64;
    s.push_str(&format!(
        "- 阅读时间(估算, 5000字/分钟): {} 分钟\n",
        read_mins.max(1)
    ));
    s.push_str("- Speaker 名称说明: ");
    let speakers: BTreeSet<u32> = utterances.iter().map(|u| u.speaker_id).collect();
    if speakers.is_empty() {
        s.push_str("无说话者标注\n");
    } else {
        let labels: Vec<String> = speakers.iter().map(|id| job.speaker_name(*id)).collect();
        s.push_str(&format!("{}\n", labels.join(" / ")));
    }
    s.push_str(&format!("- 原始稿本地路径: {}\n\n", raw_md_path.display()));

    s.push_str("## 全文\n\n");
    s.push_str(&body);
    s
}

/// Format milliseconds as `HH:MM:SS`.
fn fmt_ts(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Format milliseconds as a human duration like `47分13秒` or `1时02分03秒`.
fn fmt_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs / 60) % 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{}时{:02}分{:02}秒", h, m, s)
    } else {
        format!("{}分{:02}秒", m, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{
        CreatedFrom, Job, RuntimeBackend, RuntimeSource, SourceLanguage, StageState,
        TranscriptionResult, TranscriptionSelection,
    };
    use uuid::Uuid;

    fn sample_job() -> Job {
        let mut j = Job::new(Uuid::new_v4(), "BV1test".into(), 1);
        j.speaker_map.insert(0, "Alice".into());
        j.speaker_map.insert(1, "Bob".into());
        j
    }

    fn sample_utts() -> Vec<Utterance> {
        vec![
            Utterance {
                id: "u001".into(),
                text: "你好世界".into(),
                start_ms: 0,
                end_ms: 1500,
                speaker_id: 0,
            },
            Utterance {
                id: "u002".into(),
                text: "第二句话。".into(),
                start_ms: 1500,
                end_ms: 3000,
                speaker_id: 1,
            },
        ]
    }

    #[test]
    fn raw_markdown_uses_speaker_map_and_embeds_job_id() {
        let job = sample_job();
        let md = render_raw_markdown(&job, &sample_utts());
        assert!(md.contains(&format!("<!-- job-id: {} -->", job.id)));
        assert!(md.contains("[Alice]"));
        assert!(md.contains("[Bob]"));
        assert!(md.contains("你好世界"));
        assert!(md.contains("[00:00:00](https://www.bilibili.com/video/BV1test?p=1&t=0)"));
    }

    #[test]
    fn full_markdown_has_header_and_link() {
        let job = sample_job();
        let meta = DocMeta {
            title: "测试视频".into(),
            up_name: "测试UP".into(),
            duration_ms: 47 * 60 * 1000,
            bvid: "BV1test".into(),
            page: 1,
            source_url: None,
        };
        let raw_path = std::path::Path::new("/tmp/transcript.raw.md");
        let md = render_full_markdown(&job, &meta, &sample_utts(), None, raw_path);
        assert!(md.contains("# 测试视频"));
        assert!(md.contains("UP主: 测试UP"));
        assert!(md.contains(
            "原视频链接: [https://www.bilibili.com/video/BV1test](https://www.bilibili.com/video/BV1test)"
        ));
        assert!(md.contains("阅读时间"));
        assert!(md.contains("原始稿本地路径: /tmp/transcript.raw.md"));
        assert!(md.contains(&format!("<!-- job-id: {} -->", job.id)));
    }

    #[test]
    fn full_markdown_page_gt_1_includes_p() {
        let job = Job::new(Uuid::new_v4(), "BV1test".into(), 2);
        let meta = DocMeta {
            title: "t".into(),
            up_name: "u".into(),
            duration_ms: 1000,
            bvid: "BV1test".into(),
            page: 2,
            source_url: None,
        };
        let md = render_full_markdown(&job, &meta, &[], None, Path::new("/x.md"));
        assert!(md.contains("?p=2"));
    }

    #[test]
    fn full_markdown_records_requested_and_reported_runtime_identity() {
        let mut job = sample_job();
        job.transcription_selection = Some(TranscriptionSelection::new(
            RuntimeSource::External,
            Path::new("/runtime/project").to_path_buf(),
            Path::new("/runtime/data").to_path_buf(),
            "runtime-v2-fingerprint".into(),
            RuntimeBackend::DockerCompose,
            Some("English model".into()),
            SourceLanguage::En,
            CreatedFrom::Cli,
        ));
        job.transcription_result = Some(TranscriptionResult::new(
            Some(SourceLanguage::En),
            Some("reported-model".into()),
            Some("runtime-v2-fingerprint".into()),
        ));
        let meta = DocMeta {
            title: "t".into(),
            up_name: "u".into(),
            duration_ms: 1000,
            bvid: "BV1test".into(),
            page: 1,
            source_url: None,
        };
        let md = render_full_markdown(&job, &meta, &sample_utts(), None, Path::new("/raw.md"));
        assert!(md.contains("Runtime source: external"));
        assert!(md.contains("Runtime backend: docker-compose"));
        assert!(md.contains("Runtime identity: runtime-v2-fingerprint"));
        assert!(md.contains("请求语言: 英文"));
        assert!(md.contains("实际语言: 英文"));
        assert!(md.contains("实际模型: reported-model"));
    }

    #[test]
    fn readable_body_falls_back_when_empty() {
        let job = sample_job();
        let refined = vec!["   ".to_string(), String::new()];
        let body = render_readable_body(&job, &sample_utts(), Some(&refined));
        assert!(body.contains("你好世界"));
        assert!(body.contains("[00:00:01](https://www.bilibili.com/video/BV1test?p=1&t=1)"));
    }

    #[test]
    fn readable_body_uses_refined_text_without_losing_links() {
        let job = sample_job();
        let refined = vec![
            "整理后的第一句。".to_string(),
            "整理后的第二句。".to_string(),
        ];
        let body = render_readable_body(&job, &sample_utts(), Some(&refined));
        assert!(body.contains("整理后的第一句。"));
        assert!(body.contains("整理后的第二句。"));
        assert!(body.contains("[00:00:00](https://www.bilibili.com/video/BV1test?p=1&t=0)"));
        assert!(body.contains("[00:00:01](https://www.bilibili.com/video/BV1test?p=1&t=1)"));
    }

    #[test]
    fn readable_body_groups_only_adjacent_segments_from_the_same_speaker() {
        let job = sample_job();
        let utterances = vec![
            Utterance {
                id: "u001".into(),
                text: "第一段，".into(),
                start_ms: 1_000,
                end_ms: 2_000,
                speaker_id: 0,
            },
            Utterance {
                id: "u002".into(),
                text: "紧接第二段。".into(),
                start_ms: 2_000,
                end_ms: 3_000,
                speaker_id: 0,
            },
            Utterance {
                id: "u003".into(),
                text: "Bob 发言。".into(),
                start_ms: 3_000,
                end_ms: 4_000,
                speaker_id: 1,
            },
            Utterance {
                id: "u004".into(),
                text: "Alice 再次发言。".into(),
                start_ms: 4_000,
                end_ms: 5_000,
                speaker_id: 0,
            },
        ];

        let body = render_readable_body(&job, &utterances, None);

        assert!(body.contains("第一段，紧接第二段。"));
        assert!(!body.contains("[00:00:02]"));
        assert_eq!(body.matches("**[Alice]**").count(), 2);
        assert_eq!(body.matches("**[Bob]**").count(), 1);
        assert!(body.contains("[00:00:01]"));
        assert!(body.contains("[00:00:03]"));
        assert!(body.contains("[00:00:04]"));

        let raw = render_raw_markdown(&job, &utterances);
        assert_eq!(raw.matches("**[Alice]**").count(), 3);
        assert!(raw.contains("[00:00:02]"));
    }

    #[test]
    fn doc_meta_video_url_canonicalizes_bare_bvid_input() {
        let meta = DocMeta {
            title: "t".into(),
            up_name: "u".into(),
            duration_ms: 0,
            bvid: "BV1GJ411x7h7".into(),
            page: 1,
            source_url: Some("BV1GJ411x7h7".into()),
        };
        assert_eq!(
            meta.video_url(),
            "https://www.bilibili.com/video/BV1GJ411x7h7"
        );
    }

    #[test]
    fn doc_meta_video_url_canonicalizes_full_url_and_preserves_page() {
        let meta = DocMeta {
            title: "t".into(),
            up_name: "u".into(),
            duration_ms: 0,
            bvid: "BV1GJ411x7h7".into(),
            page: 3,
            source_url: Some(
                "https://www.bilibili.com/video/BV1GJ411x7h7?p=3&spm_id_from=333.1".into(),
            ),
        };
        assert_eq!(
            meta.video_url(),
            "https://www.bilibili.com/video/BV1GJ411x7h7?p=3"
        );
    }

    #[test]
    fn doc_meta_video_url_supports_legacy_av_id() {
        let meta = DocMeta {
            title: "t".into(),
            up_name: "u".into(),
            duration_ms: 0,
            bvid: "av170001".into(),
            page: 1,
            source_url: Some("av170001".into()),
        };
        assert_eq!(meta.video_url(), "https://www.bilibili.com/video/av170001");
    }

    #[test]
    fn doc_meta_video_url_uses_resolved_job_bvid_for_short_link() {
        let meta = DocMeta {
            title: "t".into(),
            up_name: "u".into(),
            duration_ms: 0,
            bvid: "BV1GJ411x7h7".into(),
            page: 2,
            source_url: Some("https://b23.tv/abc123".into()),
        };
        assert_eq!(
            meta.video_url(),
            "https://www.bilibili.com/video/BV1GJ411x7h7?p=2"
        );
    }

    #[test]
    fn utterance_timestamp_links_floor_milliseconds_and_keep_display_time() {
        let job = Job::new(Uuid::new_v4(), "BV1GJ411x7h7".into(), 2);
        let utterances = vec![
            Utterance {
                id: "zero".into(),
                text: "zero".into(),
                start_ms: 0,
                end_ms: 1,
                speaker_id: 0,
            },
            Utterance {
                id: "subsecond".into(),
                text: "subsecond".into(),
                start_ms: 999,
                end_ms: 1000,
                speaker_id: 0,
            },
            Utterance {
                id: "hour".into(),
                text: "hour".into(),
                start_ms: 3_661_999,
                end_ms: 3_662_000,
                speaker_id: 0,
            },
        ];

        let raw = render_raw_markdown(&job, &utterances);
        assert!(raw.contains("[00:00:00](https://www.bilibili.com/video/BV1GJ411x7h7?p=2&t=0)"));
        assert!(raw.contains("[01:01:01](https://www.bilibili.com/video/BV1GJ411x7h7?p=2&t=3661)"));

        let readable = render_readable_body(&job, &utterances, None);
        assert!(
            readable.contains("[00:00:00](https://www.bilibili.com/video/BV1GJ411x7h7?p=2&t=0)")
        );
        assert!(!readable.contains("[01:01:01]"));
    }

    // silence unused import of StageState in test fixture if not referenced
    #[test]
    fn _stage_state_import_kept() {
        let _ = StageState::Pending;
    }

    #[test]
    fn snapshot_renderer_exports_source_and_provenance_without_reading_raw() {
        use crate::content_results::{
            ArtifactSlotV1, ContentBlockRoleV1, ContentCurrentV1, ContentSetupV1, ContentSlotsV1,
            DerivationProvenanceV1, DerivationRecordV1, EffectivePathV1, EvidenceDescriptorV1,
            SourceContextSnapshotV1, SourceRefV1, SourceStatusV1, ValidatedEvidenceViewV1,
            ValidationCheckV1, ValidationReportV1,
        };

        let root = std::env::temp_dir().join(format!("bimyscribe-renderer-{}", Uuid::new_v4()));
        let work = root.join("work");
        let output = root.join("output");
        std::fs::create_dir_all(&work).unwrap();
        let mut job = sample_job();
        job.title = "快照渲染测试".into();
        job.cid = Some(7);
        job.duration_ms = Some(1000);
        job.work_dir = Some(work.clone());
        job.final_output_dir = Some(output.clone());
        job.content_setup = Some(ContentSetupV1::disabled());
        let utterances = sample_utts();
        let mut descriptor = EvidenceDescriptorV1 {
            identity: "a".repeat(64),
            job_id: job.id,
            platform: "bilibili".into(),
            bvid: job.bvid.clone(),
            page: job.page,
            cid: 7,
            duration_ms: 3000,
            raw_sha256: "b".repeat(64),
            utterance_count: utterances.len(),
        };
        descriptor.identity = descriptor.recompute_identity().unwrap();
        let context = SourceContextSnapshotV1 {
            main_title: job.title.clone(),
            part_title: Some("分P测试".into()),
            terms: Vec::new(),
        };
        let record = DerivationRecordV1 {
            schema_version: 1,
            revision: 1,
            kind: ArtifactKindV1::FaithfulText,
            evidence_identity: descriptor.identity.clone(),
            source_context: context.clone(),
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
                id: "block-1".into(),
                role: ContentBlockRoleV1::Paragraph,
                title: None,
                text: "你好世界".into(),
                source_refs: vec![SourceRefV1 {
                    utterance_ids: vec!["u001".into()],
                    start_ms: 0,
                    end_ms: 1500,
                }],
                source_status: SourceStatusV1::Mapped,
            }],
            validation: ValidationReportV1 {
                processed_utterance_count: utterances.len(),
                total_utterance_count: utterances.len(),
                processing_coverage_complete: true,
                processing_chunks: Vec::new(),
                record_source_status: SourceStatusV1::Mapped,
                checks: vec![ValidationCheckV1 {
                    name: "renderer-test".into(),
                    passed: true,
                    message: None,
                }],
            },
            created_at: chrono::Utc::now(),
        };
        let slots = ContentSlotsV1 {
            faithful_text: ArtifactSlotV1 {
                current: Some(record),
                last_failure: None,
            },
            ..ContentSlotsV1::default()
        };
        let snapshot = ContentSnapshotV1 {
            current: ContentCurrentV1 {
                schema_version: 1,
                job_id: job.id,
                evidence: descriptor.clone(),
                source_context: context,
                setup: ContentSetupV1::disabled(),
                slots,
            },
            evidence: ValidatedEvidenceViewV1 {
                descriptor,
                utterances,
            },
        };

        rebuild_presentation(&job, &snapshot).unwrap();
        let full = std::fs::read_to_string(output.join("full.md")).unwrap();
        assert!(full.contains("## 内容来源说明"));
        assert!(full.contains("## 忠实整理"));
        assert!(full.contains("recipe faithful-text v1"));
        assert!(full.contains("Evidence identity"));
        assert!(full.contains("## 全文"));
        assert!(!work.join("full.md").exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn provenance_renderer_includes_frozen_prompt_and_target() {
        use crate::content_results::{
            ContentBlockRoleV1, DerivationProvenanceV1, DerivationRecordV1, EffectivePathV1,
            FallbackReasonCodeV1, FallbackReasonV1, GenerationApiFormatV1, GenerationParametersV1,
            GenerationProviderV1, SourceContextSnapshotV1, SourceStatusV1, ValidationReportV1,
        };

        let record = DerivationRecordV1 {
            schema_version: 1,
            revision: 2,
            kind: ArtifactKindV1::DefaultSummary,
            evidence_identity: "a".repeat(64),
            source_context: SourceContextSnapshotV1 {
                main_title: "测试".into(),
                part_title: None,
                terms: Vec::new(),
            },
            provenance: DerivationProvenanceV1 {
                recipe_id: "default-summary".into(),
                recipe_version: 1,
                prompt_id: Some("default-summary".into()),
                prompt_version: Some(1),
                rules_id: "default-summary".into(),
                rules_version: 1,
                provider: Some(GenerationProviderV1::OpenAiCompatible),
                api_format: Some(GenerationApiFormatV1::OpenAiChatCompletions),
                endpoint: Some("http://localhost:8000".into()),
                model: Some("local-model".into()),
                parameters: Some(GenerationParametersV1::openai_default()),
                effective_path: EffectivePathV1::Llm,
                fallback_reason: None,
            },
            blocks: vec![crate::content_results::ContentBlockV1 {
                id: "summary-1".into(),
                role: ContentBlockRoleV1::Summary,
                title: None,
                text: "摘要".into(),
                source_refs: Vec::new(),
                source_status: SourceStatusV1::Limited,
            }],
            validation: ValidationReportV1 {
                processed_utterance_count: 1,
                total_utterance_count: 1,
                processing_coverage_complete: true,
                processing_chunks: Vec::new(),
                record_source_status: SourceStatusV1::Limited,
                checks: Vec::new(),
            },
            created_at: chrono::Utc::now(),
        };

        let rendered = format_provenance(&record);
        assert!(rendered.contains("prompt default-summary v1"));
        assert!(rendered.contains("OpenAI-compatible / chat-completions"));
        assert!(rendered.contains("http://localhost:8000"));
        assert!(rendered.contains("local-model"));
        assert!(rendered.contains("temperature=0.2"));

        let mut fallback = record;
        fallback.kind = ArtifactKindV1::FaithfulText;
        fallback.provenance = DerivationProvenanceV1 {
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
            effective_path: EffectivePathV1::RulesFallback,
            fallback_reason: Some(FallbackReasonV1 {
                code: FallbackReasonCodeV1::TargetUnavailable,
                message: "本地 AI 配置不可用".into(),
            }),
        };
        let rendered = format_provenance(&fallback);
        assert!(rendered.contains("target_unavailable / AI 配置不可用"));
        assert!(rendered.contains("本地 AI 配置不可用"));
    }

    #[test]
    fn rewrite_reading_time_metadata_omits_profile_when_language_is_auto_or_missing() {
        let id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("bimyscribe-doc-reading-time-{id}"));
        let cfg = crate::config::Config::for_paths(
            &crate::paths::AppPaths::discover().expect("app paths"),
        );
        let mut job = crate::desktop::result_fixture_callback_job(&root, &cfg, "result-success")
            .expect("result fixture job");
        let Ok(crate::content_results::ContentViewV1::Current(snapshot)) =
            crate::content_results::current(&job)
        else {
            panic!("expected current snapshot");
        };

        job.transcription_result = None;
        if let Some(selection) = job.transcription_selection.as_mut() {
            selection.requested_language = SourceLanguage::Auto;
        }

        let initial_doc = "- 字符数: 100\n- 阅读时间: 1分钟\n## 全文\n";
        let rewritten = rewrite_reading_time_metadata(
            initial_doc.to_string(),
            &job,
            &snapshot,
            SummaryTier::Standard,
        );
        assert!(rewritten.contains("- 正文规模: 约"));
        assert!(rewritten.contains("- 视频时长: 1分04秒"));
        assert!(!rewritten.contains("估算口径"));
        assert!(!rewritten.contains("预计阅读"));
        assert!(!rewritten.contains("预计节省"));

        std::fs::remove_dir_all(root).ok();
    }
}
