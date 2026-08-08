//! Bilibili URL / BVID / page (分P) parsing and metadata fetch.
//!
//! This module implements URL parsing and anonymous Bilibili API access.
//! access: parsing full URLs, short links, bare BV ids and `?p=N` page
//! parameters. Real metadata fetching (`fetch_metadata`) is a stub and returns
//! `NotImplemented` until the B站 adapter is wired in a later step.

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedVideo {
    /// Bilibili BV id, e.g. `BV1HuMk61ECq` (without the `BV` stripped).
    pub bvid: String,
    /// CID, if it could be derived from the URL. None means "page 1 / unknown".
    pub cid: Option<u64>,
    /// Page (分P) number, 1-based. None means 1.
    pub page: Option<u32>,
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("empty input")]
    Empty,
    #[error("not a recognizable Bilibili URL or BV id: {0}")]
    Unrecognized(String),
}

/// Parse a Bilibili URL, short link, or bare BV id.
///
/// Accepted forms:
/// - `https://www.bilibili.com/video/BV1HuMk61ECq`
/// - `https://www.bilibili.com/video/BV1HuMk61ECq?p=2`
/// - `https://www.bilibili.com/video/av1700000000?p=2`
/// - `https://b23.tv/abc123` (short link; BVID unknown until resolved)
/// - `BV1HuMk61ECq`
/// - `bv1HuMk61ECq` (case-insensitive)
/// - `av1700000000` (legacy av id; BVID left empty, resolved later)
pub fn parse_url(input: &str) -> Result<ParsedVideo, ParseError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }

    // Page parameter, if present as ?p=N or &p=N.
    let page = extract_page(s);

    // Try to find a BV id anywhere in the string.
    if let Some(bvid) = extract_bvid(s) {
        return Ok(ParsedVideo {
            bvid,
            cid: None,
            page,
        });
    }

    // b23.tv short link: we can't resolve without a network call, but accept it
    // and surface the path segment as a placeholder until metadata fetch.
    if s.contains("b23.tv") {
        // Use the last path segment as a temporary id.
        let id = s
            .split('/')
            .rfind(|part| !part.is_empty())
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("");
        return Ok(ParsedVideo {
            bvid: format!("b23:{}", id),
            cid: None,
            page,
        });
    }

    // Legacy av id, either bare or in a full `/video/av...` URL.
    if let Some(avid) = extract_avid(s) {
        return Ok(ParsedVideo {
            bvid: avid,
            cid: None,
            page,
        });
    }

    Err(ParseError::Unrecognized(s.to_string()))
}

/// Extract a `BV` id (12 chars total, starting with `BV1`) from a string.
/// Case-insensitive on the prefix; returns canonical uppercase form.
fn extract_bvid(s: &str) -> Option<String> {
    let lower = s.to_lowercase();
    let idx = lower.find("bv1")?;
    let candidate = &s[idx..];
    // BVID is 12 chars: BV + 10 base58 chars [A-Za-z0-9].
    let bvid: String = candidate.chars().take(12).collect();
    if bvid.len() != 12 {
        return None;
    }
    // All chars must be base58-safe alphanumeric.
    if !bvid.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    // Normalize prefix to uppercase "BV".
    let mut out = String::with_capacity(12);
    out.push_str("BV");
    out.push_str(&bvid[2..]);
    Some(out)
}

/// Extract a legacy `av` id from a bare id or Bilibili `/video/av...` URL.
fn extract_avid(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    let digits = if let Some(rest) = lower.strip_prefix("av") {
        rest
    } else {
        let marker = "/video/av";
        let start = lower.find(marker)? + marker.len();
        &lower[start..]
    };
    let aid: String = digits
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    (!aid.is_empty()).then(|| format!("av{aid}"))
}

/// Extract the `?p=N` page parameter, if present.
fn extract_page(s: &str) -> Option<u32> {
    // Find `p=` preceded by `?` or `&`.
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if (bytes[i] == b'?' || bytes[i] == b'&')
            && bytes[i + 1] == b'p'
            && i + 2 < bytes.len()
            && bytes[i + 2] == b'='
        {
            let start = i + 3;
            let digits: String = s[start..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u32>() {
                if n >= 1 {
                    return Some(n);
                }
            }
        }
        i += 1;
    }
    None
}

/// Fetch video metadata anonymously.
///
/// Uses `https://api.bilibili.com/x/web-interface/view?bvid=` with only a
/// User-Agent header (no Referer, no cookies - verified to return `code:0` for
/// public UGC videos). Resolves the CID for the requested `page` (1-based) from
/// `data.pages[p-1].cid` (falling back to `data.cid` for page 1).
pub fn fetch_metadata(bvid: &str, page: Option<u32>) -> Result<Metadata, MetadataError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(30))
        .build();
    fetch_metadata_with(&agent, bvid, page)
}

fn fetch_metadata_with(
    agent: &ureq::Agent,
    bvid: &str,
    page: Option<u32>,
) -> Result<Metadata, MetadataError> {
    let url = metadata_api_url(bvid);
    let resp = agent
        .get(&url)
        .set("User-Agent", UA)
        .call()
        .map_err(|e| MetadataError::Network(e.to_string()))?;
    let body: ApiResponse<ViewData> = serde_json::from_reader(resp.into_reader())
        .map_err(|e| MetadataError::Network(format!("decode view response: {e}")))?;
    if body.code != 0 {
        return Err(match body.code {
            -403 => MetadataError::NotAccessible,
            -404 => MetadataError::NotAccessible,
            c => MetadataError::Network(format!("bilibili code {} ({} available)", c, body.code)),
        });
    }
    let data = body
        .data
        .ok_or_else(|| MetadataError::Network("empty data".into()))?;
    let page = page.unwrap_or(1);
    if page > data.videos {
        return Err(MetadataError::NotAccessible);
    }
    // CID for the requested page: pages[p-1].cid, or data.cid for page 1.
    let cid = data
        .pages
        .get((page as usize).saturating_sub(1))
        .map(|p| p.cid)
        .unwrap_or(data.cid);
    Ok(Metadata {
        bvid: data.bvid,
        cid,
        title: data.title,
        up_name: data.owner.name,
        duration_ms: (data.duration as u64) * 1000,
    })
}

fn metadata_api_url(video_id: &str) -> String {
    let lower = video_id.to_ascii_lowercase();
    if let Some(aid) = lower
        .strip_prefix("av")
        .filter(|aid| !aid.is_empty() && aid.chars().all(|character| character.is_ascii_digit()))
    {
        format!("https://api.bilibili.com/x/web-interface/view?aid={aid}")
    } else {
        format!("https://api.bilibili.com/x/web-interface/view?bvid={video_id}")
    }
}

/// Resolve a `b23.tv` short link to a full bilibili.com URL.
///
/// Does a GET with redirects disabled and reads the `Location` header. If the
/// short code does not redirect (e.g. already a full URL), returns the input.
pub fn resolve_short_link(code: &str) -> Result<String, MetadataError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(15))
        .redirects(0)
        .build();
    let url = format!("https://b23.tv/{}", code.trim_start_matches('/'));
    let resp = agent
        .get(&url)
        .set("User-Agent", UA)
        .call()
        .map_err(|e| MetadataError::Network(e.to_string()))?;
    // 301/302 -> follow Location; 200 -> no redirect.
    if (300..400).contains(&resp.status()) {
        resp.header("Location")
            .map(|s| s.to_string())
            .ok_or_else(|| MetadataError::Network("redirect without Location".into()))
    } else if resp.status() == 200 {
        Ok(url)
    } else {
        Err(MetadataError::Network(format!(
            "b23.tv status {}",
            resp.status()
        )))
    }
}

/// Fetch the highest-bandwidth DASH audio stream URL anonymously.
///
/// Returns `(audio_url, content_type)` for the best audio stream. Uses
/// `fnval=16` (DASH, audio separated from video) so no video is downloaded.
pub fn fetch_playurl_audio(bvid: &str, cid: u64) -> Result<String, MetadataError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(30))
        .build();
    let url = format!(
        "https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&fnval=16&qn=64&try_look=1",
        bvid, cid
    );
    let resp = agent
        .get(&url)
        .set("User-Agent", UA)
        .set("Referer", "https://www.bilibili.com")
        .call()
        .map_err(|e| MetadataError::Network(e.to_string()))?;
    let body: ApiResponse<PlayUrlData> = serde_json::from_reader(resp.into_reader())
        .map_err(|e| MetadataError::Network(format!("decode playurl response: {e}")))?;
    if body.code != 0 {
        return Err(match body.code {
            -403 | -404 => MetadataError::NotAccessible,
            c => MetadataError::Network(format!("bilibili code {} for playurl", c)),
        });
    }
    let data = body
        .data
        .ok_or_else(|| MetadataError::Network("empty data".into()))?;
    let dash = data.dash.ok_or(MetadataError::NotAccessible)?;
    // Pick the highest-bandwidth audio stream.
    let best = dash
        .audio
        .iter()
        .max_by_key(|a| a.bandwidth)
        .ok_or(MetadataError::NotAccessible)?;
    // baseUrl is the canonical field; some responses also expose base_url.
    best.base_url
        .clone()
        .or_else(|| best.base_url_snake.clone())
        .ok_or(MetadataError::NotAccessible)
}

/// Download an audio stream to `dest`, reporting byte progress via the callback.
///
/// Sends `User-Agent` + `Referer` (defensive; the CDN does not strictly enforce
/// them for anonymous public content) and a `Range: bytes=0-` to enable
/// resumable/streaming reads. The callback receives the cumulative bytes written
/// and is invoked roughly every 256 KiB.
pub fn download_audio<F: FnMut(u64, u64)>(
    url: &str,
    dest: &std::path::Path,
    expected_bytes: Option<u64>,
    mut on_progress: F,
) -> Result<u64, MetadataError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(60))
        .build();
    let resp = agent
        .get(url)
        .set("User-Agent", UA)
        .set("Referer", "https://www.bilibili.com")
        .set("Range", "bytes=0-")
        .call()
        .map_err(|e| MetadataError::Network(e.to_string()))?;
    // 200 or 206 are both fine; 403/404 mean the URL expired.
    let status = resp.status();
    if status == 403 || status == 404 {
        return Err(MetadataError::Network(format!(
            "audio URL expired/forbidden (status {})",
            status
        )));
    }
    if status != 200 && status != 206 {
        return Err(MetadataError::Network(format!(
            "download status {}",
            status
        )));
    }
    let total = expected_bytes.or_else(|| {
        resp.header("Content-Length")
            .and_then(|s| s.parse::<u64>().ok())
    });
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MetadataError::Network(format!("create dest dir: {e}")))?;
    }
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest)
        .map_err(|e| MetadataError::Network(format!("create dest file: {e}")))?;
    use std::io::{Read, Write};
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    let mut since_report = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| MetadataError::Network(format!("read stream: {e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| MetadataError::Network(format!("write dest: {e}")))?;
        written += n as u64;
        since_report += n as u64;
        if since_report >= 256 * 1024 {
            on_progress(written, total.unwrap_or(0));
            since_report = 0;
        }
    }
    file.sync_all().ok();
    on_progress(written, total.unwrap_or(0));
    Ok(written)
}

/// Browser-like User-Agent (the B站 API and CDN accept anonymous requests with
/// just this header; verified 2026-08-08).
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct Metadata {
    pub bvid: String,
    pub cid: u64,
    pub title: String,
    pub up_name: String,
    pub duration_ms: u64,
}

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("video not accessible anonymously")]
    NotAccessible,
    #[error("network error: {0}")]
    Network(String),
}

// ---- Response types (kept private; the public API returns `Metadata`/`String`) ----

/// Generic B站 API envelope: `{ "code": 0, "message": "0", "data": {...} }`.
#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    code: i64,
    #[serde(default)]
    #[allow(dead_code)]
    message: String,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct ViewData {
    bvid: String,
    cid: u64,
    title: String,
    #[serde(default)]
    duration: u64,
    /// Number of pages (分P count).
    #[serde(default)]
    videos: u32,
    owner: Owner,
    #[serde(default)]
    pages: Vec<Page>,
}

#[derive(Debug, Deserialize)]
struct Owner {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct Page {
    cid: u64,
    #[serde(default)]
    #[allow(dead_code)]
    page: u32,
    #[serde(default)]
    #[allow(dead_code)]
    part: String,
    #[serde(default)]
    duration: u64,
}

#[derive(Debug, Deserialize)]
struct PlayUrlData {
    #[serde(default)]
    dash: Option<Dash>,
}

#[derive(Debug, Deserialize)]
struct Dash {
    #[serde(default)]
    audio: Vec<AudioStream>,
}

#[derive(Debug, Deserialize)]
struct AudioStream {
    #[serde(default)]
    bandwidth: u64,
    #[serde(rename = "baseUrl", default)]
    base_url: Option<String>,
    #[serde(rename = "base_url", default)]
    base_url_snake: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_url() {
        let p = parse_url("https://www.bilibili.com/video/BV1HuMk61ECq").unwrap();
        assert_eq!(p.bvid, "BV1HuMk61ECq");
        assert_eq!(p.page, None);
    }

    #[test]
    fn parses_url_with_page() {
        let p = parse_url("https://www.bilibili.com/video/BV1HuMk61ECq?p=2").unwrap();
        assert_eq!(p.bvid, "BV1HuMk61ECq");
        assert_eq!(p.page, Some(2));
    }

    #[test]
    fn parses_bare_bvid_case_insensitive() {
        let p = parse_url("bv1Humk61ecq").unwrap();
        assert_eq!(p.bvid, "BV1Humk61ecq");
    }

    #[test]
    fn parses_av_id() {
        let p = parse_url("av1700000000").unwrap();
        assert_eq!(p.bvid, "av1700000000");
    }

    #[test]
    fn parses_full_av_url_with_page() {
        let p = parse_url("https://www.bilibili.com/video/AV1700000000?p=4").unwrap();
        assert_eq!(p.bvid, "av1700000000");
        assert_eq!(p.page, Some(4));
    }

    #[test]
    fn metadata_url_uses_aid_for_legacy_av_input() {
        assert_eq!(
            metadata_api_url("av1700000000"),
            "https://api.bilibili.com/x/web-interface/view?aid=1700000000"
        );
        assert_eq!(
            metadata_api_url("BV1GJ411x7h7"),
            "https://api.bilibili.com/x/web-interface/view?bvid=BV1GJ411x7h7"
        );
    }

    #[test]
    fn parses_b23_short_link() {
        let p = parse_url("https://b23.tv/abc123").unwrap();
        assert_eq!(p.bvid, "b23:abc123");
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            parse_url("hello world"),
            Err(ParseError::Unrecognized(_))
        ));
        assert!(matches!(parse_url(""), Err(ParseError::Empty)));
    }

    // ---- Response parsing tests (offline, with fixture JSON) ----

    #[test]
    fn parses_view_response() {
        let json = r#"{
            "code": 0, "message": "0",
            "data": {
                "bvid": "BV1GJ411x7h7", "aid": 80433022, "cid": 137649199,
                "title": "Never Gonna Give You Up", "duration": 213, "videos": 1,
                "owner": {"mid": 486906719, "name": "索尼音乐中国"},
                "pages": [{"cid": 137649199, "page": 1, "part": "song", "duration": 213}]
            }
        }"#;
        let v: ApiResponse<ViewData> = serde_json::from_str(json).unwrap();
        assert_eq!(v.code, 0);
        let data = v.data.unwrap();
        assert_eq!(data.title, "Never Gonna Give You Up");
        assert_eq!(data.owner.name, "索尼音乐中国");
        assert_eq!(data.videos, 1);
        assert_eq!(data.pages[0].cid, 137649199);
    }

    #[test]
    fn view_response_picks_correct_page_cid() {
        // Multi-page video: page 2 must take pages[1].cid.
        let json = r#"{
            "code": 0, "data": {
                "bvid": "BV1test", "cid": 100, "title": "t", "duration": 60, "videos": 2,
                "owner": {"name": "up"},
                "pages": [{"cid":100,"page":1},{"cid":200,"page":2}]
            }
        }"#;
        let v: ApiResponse<ViewData> = serde_json::from_str(json).unwrap();
        let data = v.data.unwrap();
        // page 1 -> data.cid (100)
        assert_eq!(data.pages.first().map(|p| p.cid), Some(100));
        // page 2 -> pages[1].cid (200)
        assert_eq!(data.pages.get(1).map(|p| p.cid), Some(200));
    }

    #[test]
    fn parses_playurl_picks_highest_bandwidth_audio() {
        let json = r#"{
            "code": 0, "data": {
                "dash": {
                    "duration": 213,
                    "audio": [
                        {"id":30216,"bandwidth":43962,"baseUrl":"https://a/low.m4s"},
                        {"id":30280,"bandwidth":203786,"baseUrl":"https://a/high.m4s"},
                        {"id":30232,"bandwidth":102931,"baseUrl":"https://a/mid.m4s"}
                    ]
                }
            }
        }"#;
        let v: ApiResponse<PlayUrlData> = serde_json::from_str(json).unwrap();
        let data = v.data.unwrap();
        let dash = data.dash.unwrap();
        let best = dash.audio.iter().max_by_key(|a| a.bandwidth).unwrap();
        assert_eq!(best.bandwidth, 203786);
        assert_eq!(best.base_url.as_deref(), Some("https://a/high.m4s"));
    }

    #[test]
    fn playurl_handles_snake_case_base_url() {
        // Some responses expose base_url (snake) instead of baseUrl.
        let json = r#"{"code":0,"data":{"dash":{"audio":[{"bandwidth":1,"base_url":"https://a/snake.m4s"}]}}}"#;
        let v: ApiResponse<PlayUrlData> = serde_json::from_str(json).unwrap();
        let a = v
            .data
            .unwrap()
            .dash
            .unwrap()
            .audio
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(a.base_url, None);
        assert_eq!(a.base_url_snake.as_deref(), Some("https://a/snake.m4s"));
    }

    #[test]
    fn playurl_missing_dash_is_not_accessible() {
        let json = r#"{"code":0,"data":{}}"#;
        let v: ApiResponse<PlayUrlData> = serde_json::from_str(json).unwrap();
        assert!(v.data.unwrap().dash.is_none());
    }

    // ---- Live network tests (ignored by default; run with --ignored) ----
    // These hit the real B站 API and host ffmpeg. They require network access.

    #[test]
    #[ignore]
    fn live_fetch_metadata() {
        let meta = fetch_metadata("BV1GJ411x7h7", Some(1)).expect("metadata fetch");
        assert_eq!(meta.bvid, "BV1GJ411x7h7");
        assert!(!meta.title.is_empty());
        assert!(meta.cid > 0);
        assert!(meta.duration_ms > 0);
    }

    #[test]
    #[ignore]
    fn live_fetch_playurl_audio() {
        let url = fetch_playurl_audio("BV1GJ411x7h7", 137649199).expect("playurl");
        assert!(url.starts_with("http"));
    }

    #[test]
    #[ignore]
    fn live_download_and_normalize() {
        let Some(test_root) = std::env::var_os("BIMYSCRIBE_LIVE_TEST_DIR") else {
            eprintln!("skipped: BIMYSCRIBE_LIVE_TEST_DIR is not set");
            return;
        };
        let url = fetch_playurl_audio("BV1GJ411x7h7", 137649199).expect("playurl");
        let dir = std::path::PathBuf::from(test_root).join("BiMyScribe-live-download");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("source.audio");
        let bytes = download_audio(&url, &dest, None, |_, _| {}).expect("download");
        assert!(bytes > 100_000, "downloaded too few bytes: {}", bytes);

        // Normalize via the host ffmpeg.
        let norm = dir.join("normalized.wav");
        let log = dir.join("ffmpeg.log");
        let spec = crate::process::SubprocessSpec::new(vec![
            "ffmpeg".into(),
            "-y".into(),
            "-i".into(),
            dest.to_string_lossy().into_owned(),
            "-ac".into(),
            "1".into(),
            "-ar".into(),
            "16000".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
            norm.to_string_lossy().into_owned(),
        ])
        .log(&log);
        let code = crate::process::run(spec).expect("ffmpeg spawn");
        assert_eq!(code, 0, "ffmpeg failed; see {}", log.display());
        assert!(norm.exists());
        assert!(std::fs::metadata(&norm).unwrap().len() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
