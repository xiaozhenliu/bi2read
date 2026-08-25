//! Versioned reading-time estimation (v0.6 F5, `reading-profile v1`).
//!
//! One small crate-private module serves every caller (result header caption,
//! job-detail fields, and the `full.md` metadata block) so the three surfaces
//! can never disagree. The profile is deliberately frozen: zh 400 chars/min,
//! en 240 words/min, 1.0x playback. When no reliable language profile exists
//! the estimate reports `None` instead of inventing a number.

use crate::jobs::SourceLanguage;

/// Frozen estimation profile identity written to `full.md` and shown by the
/// "如何计算" explainer.
pub(crate) const READING_PROFILE_VERSION: &str = "reading-profile v1";

const ZH_CHARS_PER_MINUTE: f64 = 400.0;
const EN_WORDS_PER_MINUTE: f64 = 240.0;
const MS_PER_MINUTE: u64 = 60_000;

/// Raw text scale measured from structured plain text (slot blocks or
/// utterance text). Inputs never contain Markdown syntax, metadata, time
/// links, or resource paths — TIME-02 holds by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TextScale {
    pub cjk_chars: u64,
    pub latin_words: u64,
}

impl TextScale {
    /// Formatted scale expression: pure Chinese "N 字", pure English "N 词",
    /// or mixed Chinese + English "N 字 + M 词".
    pub(crate) fn format_scale(&self) -> String {
        match (self.cjk_chars, self.latin_words) {
            (cjk, words) if cjk > 0 && words > 0 => format!("{cjk} 字 + {words} 词"),
            (cjk, 0) if cjk > 0 => format!("{cjk} 字"),
            (0, words) if words > 0 => format!("{words} 词"),
            _ => "0 字".into(),
        }
    }
}

/// One complete reading-time estimate for a primary consumption artifact.
/// `None` durations mean "cannot be estimated" and must render as nothing,
/// never as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ReadingEstimate {
    pub scale: TextScale,
    pub video_ms: Option<u64>,
    /// Reading duration; `None` when no language profile applies.
    pub reading_ms: Option<u64>,
    /// Saved time versus watching at 1.0x; floored at zero. `None` when
    /// reading time is unavailable.
    pub saved_ms: Option<u64>,
    /// saved / video in percent; `None` when either side is unavailable.
    pub saved_ratio: Option<u8>,
}

/// Count CJK ideographs character-by-character and non-CJK alphanumeric runs
/// as words. Whitespace and punctuation split word runs; digits join the
/// surrounding run; URLs collapse into a single word because `/`, `:`, `.`
/// are ASCII splitters — acceptable, since only scale (not fidelity) matters.
pub(crate) fn measure(text: &str) -> TextScale {
    let mut scale = TextScale::default();
    let mut latin_run = 0usize;
    let flush = |run: &mut usize, words: &mut u64| {
        if *run > 0 {
            *words += 1;
            *run = 0;
        }
    };
    for character in text.chars() {
        if is_cjk_ideograph(character) {
            flush(&mut latin_run, &mut scale.latin_words);
            scale.cjk_chars += 1;
        } else if character.is_ascii_alphanumeric() {
            latin_run += 1;
        } else if character.is_whitespace() {
            flush(&mut latin_run, &mut scale.latin_words);
        } else {
            // Punctuation and symbols split words without counting. Non-ASCII
            // letters outside the CJK ranges (e.g. é) also land here and act
            // as splitters — conservative and deterministic for zh/en inputs.
            flush(&mut latin_run, &mut scale.latin_words);
        }
    }
    flush(&mut latin_run, &mut scale.latin_words);
    scale
}

fn is_cjk_ideograph(character: char) -> bool {
    matches!(character as u32,
        0x4E00..=0x9FFF       // CJK Unified Ideographs
        | 0x3400..=0x4DBF     // Extension A
        | 0x20000..=0x2A6DF   // Extension B
        | 0xF900..=0xFAFF     // Compatibility Ideographs
    )
}

impl SourceLanguage {
    /// Whether this language has a frozen reading profile. Auto and unknown
    /// languages have none — the estimator must not guess.
    fn reading_profile(self) -> Option<f64> {
        match self {
            Self::Zh => Some(ZH_CHARS_PER_MINUTE),
            Self::En => Some(EN_WORDS_PER_MINUTE),
            Self::Auto => None,
        }
    }
}

/// Estimate reading time for one artifact's plain text given the video
/// duration and the best-known source language (`reported_language` falling
/// back to `requested_language`; `None` when neither is recorded).
pub(crate) fn estimate(
    text: &str,
    video_ms: Option<u64>,
    language: Option<SourceLanguage>,
) -> ReadingEstimate {
    let scale = measure(text);
    let supports_profile = language.is_some_and(|language| language.reading_profile().is_some());
    let reading_ms = supports_profile
        .then(|| {
            (scale.cjk_chars as f64 / ZH_CHARS_PER_MINUTE
                + scale.latin_words as f64 / EN_WORDS_PER_MINUTE)
                * MS_PER_MINUTE as f64
        })
        .map(|ms| ms.ceil() as u64);
    let saved_ms = match (video_ms, reading_ms) {
        (Some(video), Some(reading)) => Some(video.saturating_sub(reading)),
        _ => None,
    };
    let saved_ratio = match (saved_ms, video_ms) {
        (Some(saved), Some(video)) if video > 0 => Some((saved * 100 / video) as u8),
        _ => None,
    };
    ReadingEstimate {
        scale,
        video_ms,
        reading_ms,
        saved_ms,
        saved_ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_counts_cjk_and_latin_words_separately() {
        assert_eq!(measure("").cjk_chars, 0);
        assert_eq!(measure("").latin_words, 0);

        let zh = measure("这是一个测试句子。");
        assert_eq!(zh.cjk_chars, 8);
        assert_eq!(zh.latin_words, 0);

        let en = measure("hello world foo");
        assert_eq!(en.cjk_chars, 0);
        assert_eq!(en.latin_words, 3);

        let mixed = measure("使用 Rust 编写");
        assert_eq!(mixed.cjk_chars, 4);
        assert_eq!(mixed.latin_words, 1);
    }

    #[test]
    fn measure_handles_punctuation_numbers_and_urls() {
        let value = measure("访问 https://example.com/a 查看第 42 条。");
        // URL + digits collapse into word runs: "https", "example", "com",
        // "a", "42" → 5; CJK ideographs 访问查看第条 = 6 chars.
        assert_eq!(value.latin_words, 5);
        assert_eq!(value.cjk_chars, 6);
    }

    #[test]
    fn estimate_zh_profile_sums_segments() {
        // 800 chars @ 400/min = 2 minutes.
        let text = "中".repeat(800);
        let estimate = estimate(&text, Some(10 * MS_PER_MINUTE), Some(SourceLanguage::Zh));
        assert_eq!(estimate.reading_ms, Some(2 * MS_PER_MINUTE));
        assert_eq!(estimate.saved_ms, Some(8 * MS_PER_MINUTE));
        assert_eq!(estimate.saved_ratio, Some(80));
    }

    #[test]
    fn estimate_en_profile_counts_words() {
        // 480 words @ 240/min = 2 minutes.
        let text = "word ".repeat(480);
        let estimate = estimate(&text, Some(10 * MS_PER_MINUTE), Some(SourceLanguage::En));
        assert_eq!(estimate.reading_ms, Some(2 * MS_PER_MINUTE));
        assert_eq!(estimate.scale.format_scale(), "480 词");
    }

    #[test]
    fn missing_or_auto_language_yields_no_estimate() {
        let text = "中文内容若干";
        for language in [None, Some(SourceLanguage::Auto)] {
            let estimate = estimate(text, Some(60_000), language);
            assert_eq!(estimate.reading_ms, None);
            assert_eq!(estimate.saved_ms, None);
            assert_eq!(estimate.saved_ratio, None);
            // Scale is still reported honestly.
            assert!(estimate.scale.cjk_chars > 0);
        }
    }

    #[test]
    fn reading_over_video_floors_saved_at_zero() {
        let text = "中".repeat(10_000); // 25 min reading
        let estimate = estimate(&text, Some(60_000), Some(SourceLanguage::Zh));
        assert_eq!(estimate.reading_ms, Some(25 * MS_PER_MINUTE));
        assert_eq!(estimate.saved_ms, Some(0));
        assert_eq!(estimate.saved_ratio, Some(0));
    }

    #[test]
    fn missing_video_has_ratio_but_no_saved_even_when_reading_known() {
        let text = "中".repeat(400);
        let estimate = estimate(&text, None, Some(SourceLanguage::Zh));
        assert_eq!(estimate.reading_ms, Some(MS_PER_MINUTE));
        assert_eq!(estimate.saved_ms, None);
        assert_eq!(estimate.saved_ratio, None);
    }

    #[test]
    fn format_scale_supports_chinese_english_and_mixed() {
        let zh_dominant = measure("中文内容占多数 with some english");
        assert_eq!(zh_dominant.format_scale(), "7 字 + 3 词");
        let en_dominant = measure("plain english content here");
        assert_eq!(en_dominant.format_scale(), "4 词");
        let pure_zh = measure("纯中文正文内容");
        assert_eq!(pure_zh.format_scale(), "7 字");
    }
}
