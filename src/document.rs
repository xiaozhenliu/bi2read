//! Markdown document generation.
//!
//! Produces `transcript.raw.md` (the untouched FunASR output) and `full.md`
//! (the readable document). The raw markdown never goes through an LLM and is
//! never deleted if the readable version fails. Each `.md`
//! embeds the job id so `state.json` recovery can verify it exists for the right
//! job.

use std::collections::BTreeSet;
use std::path::Path;

use crate::funasr::Utterance;
use crate::jobs::Job;

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
        canonical_video_url(&self.bvid, self.page)
    }
}

/// Build a stable Bilibili video URL from the identifier resolved onto a job.
fn canonical_video_url(video_id: &str, page: u32) -> String {
    let base = format!("https://www.bilibili.com/video/{}", video_id);
    if page > 1 {
        format!("{base}?p={page}")
    } else {
        base
    }
}

/// Build a seek link using Bilibili's supported `p=N&t=seconds` parameters.
fn timestamp_url(video_id: &str, page: u32, start_ms: u64) -> String {
    format!(
        "https://www.bilibili.com/video/{video_id}?p={}&t={}",
        page.max(1),
        start_ms / 1000
    )
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
            timestamp_url(&job.bvid, job.page, u.start_ms),
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
            timestamp_url(&job.bvid, job.page, first.start_ms),
            text
        ));
    }

    body
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
}
