use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use percent_encoding::percent_decode_str;
use regex::{Captures, Regex};
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, REFERER, USER_AGENT,
};
use reqwest::{Client, Method, Response};
use serde::Serialize;
use serde_json::Value;
use url::{Host, Url};

const DOCUMENT_HOSTS: &[&str] = &[
    "douyin.com",
    "www.douyin.com",
    "m.douyin.com",
    "v.douyin.com",
    "iesdouyin.com",
    "www.iesdouyin.com",
];
const PLAY_HOSTS: &[&str] = &["aweme.snssdk.com", "aweme-hl.snssdk.com"];
const MEDIA_HOST_SUFFIXES: &[&str] = &["zjcdn.com", "douyinvod.com", "bytecdntp.com", "bytecdn.cn"];
const IMAGE_HOST_SUFFIXES: &[&str] = &[
    "douyinpic.com",
    "douyincdn.com",
    "byteimg.com",
    "bytecdn.cn",
    "pstatp.com",
    "ibytedapm.com",
];
const JSON_ASSIGNMENTS: &[&str] = &[
    "window._ROUTER_DATA",
    "window._SSR_DATA",
    "window.__NEXT_DATA__",
    "window.__UNIVERSAL_DATA_FOR_REHYDRATION__",
];
pub const MOBILE_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Mobile Safari/537.36";
const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_REDIRECTS: usize = 5;
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_NODES: usize = 50_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResolveError {
    pub code: String,
    pub error: String,
    pub status: u16,
}

impl ResolveError {
    fn new(code: impl Into<String>, error: impl Into<String>, status: u16) -> Self {
        Self {
            code: code.into(),
            error: error.into(),
            status,
        }
    }

    fn bad_request(code: impl Into<String>, error: impl Into<String>) -> Self {
        Self::new(code, error, 400)
    }

    fn upstream(code: impl Into<String>, error: impl Into<String>) -> Self {
        Self::new(code, error, 502)
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.error)
    }
}

impl std::error::Error for ResolveError {}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ShareMediaCandidate {
    pub url: String,
    pub source_path: Option<String>,
    pub source_kind: Option<String>,
    pub directness: Option<String>,
    pub playback_variant: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub bitrate: Option<f64>,
    pub codec: Option<String>,
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ShareImage {
    pub url: String,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ShareResolveResponse {
    pub status: String,
    pub document_url: Option<String>,
    pub video_id: Option<String>,
    pub title: Option<String>,
    pub images: Vec<ShareImage>,
    pub media_url: Option<String>,
    pub playback_variant: Option<String>,
    pub candidates: Vec<ShareMediaCandidate>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Directness {
    PlayEndpoint,
    CdnMedia,
    Unknown,
}

impl Directness {
    fn as_str(self) -> &'static str {
        match self {
            Self::PlayEndpoint => "play-endpoint",
            Self::CdnMedia => "cdn-media",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SourceInfo {
    kind: &'static str,
    score: f64,
}

#[derive(Debug, Clone, Default)]
struct CandidateMetadata {
    bitrate: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
    codec: Option<String>,
    format: Option<String>,
}

#[derive(Debug, Clone)]
struct Candidate {
    url: String,
    source_path: String,
    source_kind: String,
    directness: Directness,
    playback_variant: Option<String>,
    width: Option<f64>,
    height: Option<f64>,
    bitrate: Option<f64>,
    codec: Option<String>,
    format: Option<String>,
    score: f64,
    evidence: Vec<String>,
    sources: Vec<String>,
    order: usize,
}

impl Candidate {
    fn public(&self) -> ShareMediaCandidate {
        ShareMediaCandidate {
            url: self.url.clone(),
            source_path: Some(self.source_path.clone()),
            source_kind: Some(self.source_kind.clone()),
            directness: Some(self.directness.as_str().to_string()),
            playback_variant: self.playback_variant.clone(),
            width: self.width,
            height: self.height,
            bitrate: self.bitrate,
            codec: self.codec.clone(),
            format: self.format.clone(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct CandidateSet {
    values: Vec<Candidate>,
    indexes: HashMap<String, usize>,
}

impl CandidateSet {
    fn merge(&mut self, candidate: Candidate) {
        if let Some(index) = self.indexes.get(&candidate.url).copied() {
            let existing = &mut self.values[index];
            existing.score = existing.score.max(candidate.score);
            existing.bitrate = max_option(existing.bitrate, candidate.bitrate);
            existing.width = max_option(existing.width, candidate.width);
            existing.height = max_option(existing.height, candidate.height);
            if existing.playback_variant.is_none() {
                existing.playback_variant = candidate.playback_variant;
            }
            extend_unique(&mut existing.evidence, candidate.evidence);
            extend_unique(&mut existing.sources, candidate.sources);
            return;
        }

        self.indexes
            .insert(candidate.url.clone(), self.values.len());
        self.values.push(candidate);
    }

    fn into_sorted(mut self) -> Vec<Candidate> {
        self.values.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| option_number(right.bitrate).total_cmp(&option_number(left.bitrate)))
                .then_with(|| {
                    let right_area = option_number(right.width) * option_number(right.height);
                    let left_area = option_number(left.width) * option_number(left.height);
                    right_area.total_cmp(&left_area)
                })
                .then_with(|| left.order.cmp(&right.order))
        });
        self.values
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtractionStats {
    scripts: usize,
    parsed_payloads: usize,
    visited_nodes: usize,
}

#[derive(Debug, Clone)]
struct ExtractionResult {
    candidates: Vec<Candidate>,
    stats: ExtractionStats,
    warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct CollectionState {
    nodes: usize,
    order: usize,
}

fn option_number(value: Option<f64>) -> f64 {
    value.unwrap_or(0.0)
}

fn max_option(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn extend_unique(target: &mut Vec<String>, values: Vec<String>) {
    let mut known: HashSet<String> = target.iter().cloned().collect();
    for value in values {
        if known.insert(value.clone()) {
            target.push(value);
        }
    }
}

fn source_info(key: &str) -> Option<SourceInfo> {
    match key {
        "playaddrh264" => Some(SourceInfo {
            kind: "play",
            score: 96.0,
        }),
        "playaddr" => Some(SourceInfo {
            kind: "play",
            score: 92.0,
        }),
        "playaddrbytevc1" => Some(SourceInfo {
            kind: "play",
            score: 88.0,
        }),
        "videourl" => Some(SourceInfo {
            kind: "video-url",
            score: 72.0,
        }),
        "contenturl" => Some(SourceInfo {
            kind: "video-url",
            score: 70.0,
        }),
        "downloadaddr" => Some(SourceInfo {
            kind: "download",
            score: 52.0,
        }),
        _ => None,
    }
}

fn normalized_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| !matches!(character, '-' | '_' | '.'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn host_matches(hostname: &str, suffix: &str) -> bool {
    hostname == suffix || hostname.ends_with(&format!(".{suffix}"))
}

fn named_hostname(url: &Url) -> Result<String, ResolveError> {
    match url.host() {
        Some(Host::Domain(hostname)) => Ok(hostname.to_ascii_lowercase()),
        Some(Host::Ipv4(_)) | Some(Host::Ipv6(_)) => Err(ResolveError::bad_request(
            "invalid_url",
            "The URL must use a named host.",
        )),
        None => Err(ResolveError::bad_request(
            "invalid_url",
            "The URL must include a host.",
        )),
    }
}

fn assert_basic_https_url(url: &Url, label: &str) -> Result<String, ResolveError> {
    if url.scheme() != "https" {
        return Err(ResolveError::bad_request(
            "invalid_url",
            format!("{label} must use HTTPS."),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
    {
        return Err(ResolveError::bad_request(
            "invalid_url",
            format!("{label} must not contain credentials or a custom port."),
        ));
    }
    named_hostname(url)
}

fn assert_document_url(value: &str) -> Result<Url, ResolveError> {
    let url = Url::parse(value)
        .map_err(|_| ResolveError::bad_request("invalid_url", "The share URL is not valid."))?;
    validate_document_url(url)
}

fn validate_document_url(url: Url) -> Result<Url, ResolveError> {
    let hostname = assert_basic_https_url(&url, "The share URL")?;
    if !DOCUMENT_HOSTS.contains(&hostname.as_str()) {
        return Err(ResolveError::bad_request(
            "unsupported_host",
            format!("Unsupported share host: {hostname}"),
        ));
    }
    Ok(url)
}

fn extract_share_url(input: &str) -> Result<Url, ResolveError> {
    let text = input.trim();
    if text.is_empty() {
        return Err(ResolveError::bad_request(
            "missing_input",
            "Paste a share link first.",
        ));
    }
    if text.len() > MAX_INPUT_BYTES {
        return Err(ResolveError::bad_request(
            "input_too_large",
            "The share text is too large.",
        ));
    }

    let pattern = Regex::new(r#"(?i)https?://[^\s<>"'`]+"#).expect("valid share URL regex");
    let matched = pattern.find(text).ok_or_else(|| {
        ResolveError::bad_request(
            "missing_url",
            "No HTTP or HTTPS URL was found in the share text.",
        )
    })?;
    let cleaned = matched.as_str().trim_end_matches(|character| {
        matches!(
            character,
            ')' | ','
                | '.'
                | ';'
                | '!'
                | '?'
                | '，'
                | '。'
                | '！'
                | '？'
                | '；'
                | '：'
                | '、'
                | '）'
                | '》'
                | '」'
        )
    });
    assert_document_url(cleaned)
}

fn extract_video_id(url: &Url) -> Option<String> {
    let pattern =
        Regex::new(r"/(?:(?:share)/)?(?:video|note)/(\d+)").expect("valid video ID regex");
    if let Some(captures) = pattern.captures(url.path()) {
        return captures.get(1).map(|capture| capture.as_str().to_string());
    }

    let modal_id = url
        .query_pairs()
        .find(|(key, _)| key == "modal_id")
        .map(|(_, value)| value.into_owned());
    modal_id.or_else(|| {
        url.query_pairs()
            .find(|(key, _)| key == "aweme_id")
            .map(|(_, value)| value.into_owned())
    })
}

fn canonical_video_document(video_id: &str) -> Result<Url, ResolveError> {
    if video_id.is_empty() || !video_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ResolveError::bad_request(
            "invalid_video_id",
            "The video ID is not valid.",
        ));
    }
    Url::parse(&format!(
        "https://www.iesdouyin.com/share/video/{video_id}/"
    ))
    .map_err(|_| ResolveError::bad_request("invalid_video_id", "The video ID is not valid."))
}

fn public_video_detail_url(document_url: &Url, video_id: &str) -> Result<Url, ResolveError> {
    if video_id.is_empty() || !video_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ResolveError::bad_request(
            "invalid_video_id",
            "The video ID is not valid.",
        ));
    }

    let mut url = document_url.clone();
    url.set_path("/aweme/v1/aweme/detail/");
    url.set_query(None);
    url.set_fragment(None);
    url.query_pairs_mut()
        .append_pair("aweme_id", video_id)
        .append_pair("aid", "6383")
        .append_pair("device_platform", "web")
        .append_pair("version_code", "280500")
        .append_pair("version_name", "28.5.0");
    Ok(url)
}

#[cfg(test)]
fn public_document_label(url: &Url) -> String {
    format!("{}{}", url.origin().ascii_serialization(), url.path())
}

fn media_classification(url: &Url) -> Directness {
    let Some(hostname) = url.host_str().map(str::to_ascii_lowercase) else {
        return Directness::Unknown;
    };
    let path = url.path();
    if PLAY_HOSTS.contains(&hostname.as_str())
        && Regex::new(r"^/aweme/v1/(?:play|playwm)/")
            .expect("valid play endpoint regex")
            .is_match(path)
    {
        return Directness::PlayEndpoint;
    }

    let lower_path = path.to_ascii_lowercase();
    let media_shape = lower_path.contains("/video/")
        || url
            .query_pairs()
            .any(|(key, value)| key == "mime_type" && value == "video_mp4")
        || [".mp4", ".m4v", ".mov"]
            .iter()
            .any(|extension| lower_path.ends_with(extension));
    if media_shape
        && MEDIA_HOST_SUFFIXES
            .iter()
            .any(|suffix| host_matches(&hostname, suffix))
    {
        Directness::CdnMedia
    } else {
        Directness::Unknown
    }
}

fn parse_media_url(value: &str, base: Option<&Url>) -> Result<Url, ResolveError> {
    let url = match base {
        Some(base) => base.join(value),
        None => Url::parse(value),
    }
    .map_err(|_| {
        ResolveError::upstream(
            "invalid_media_redirect",
            "The media redirect URL is not valid.",
        )
    })?;
    let hostname = assert_basic_https_url(&url, "The media URL")?;
    if media_classification(&url) == Directness::Unknown {
        return Err(ResolveError::upstream(
            "unsupported_media_host",
            format!("Unsupported media redirect host: {hostname}"),
        ));
    }
    Ok(url)
}

pub fn validate_download_url(value: &str) -> Result<Url, ResolveError> {
    parse_media_url(value, None)
}

pub fn validate_image_download_url(value: &str) -> Result<Url, ResolveError> {
    normalize_image_url(value).ok_or_else(|| {
        ResolveError::upstream(
            "unsupported_image_host",
            "The image URL is not a supported HTTPS image address.",
        )
    })
}

fn playback_variant_for_url(url: &Url) -> Option<String> {
    let hostname = url.host_str()?.to_ascii_lowercase();
    if PLAY_HOSTS.contains(&hostname.as_str()) && url.path() == "/aweme/v1/play/" {
        return Some("play".to_string());
    }
    if PLAY_HOSTS.contains(&hostname.as_str()) && url.path() == "/aweme/v1/playwm/" {
        return Some("playwm".to_string());
    }
    (media_classification(url) == Directness::CdnMedia).then(|| "direct".to_string())
}

fn derive_clean_play_url(value: &str) -> Result<Option<Url>, ResolveError> {
    let mut source = parse_media_url(value, None)?;
    let has_video_id = source.query_pairs().any(|(key, _)| key == "video_id");
    if source.path() != "/aweme/v1/playwm/" || !has_video_id {
        return Ok(None);
    }
    source.set_path("/aweme/v1/play/");
    Ok(Some(source))
}

fn parse_attributes(source: &str) -> HashMap<String, String> {
    let pattern = Regex::new(r#"([^\s=/>]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?"#)
        .expect("valid HTML attribute regex");
    let mut attributes = HashMap::new();
    for captures in pattern.captures_iter(source) {
        let Some(name) = captures.get(1) else {
            continue;
        };
        let value = captures
            .get(2)
            .or_else(|| captures.get(3))
            .or_else(|| captures.get(4))
            .map_or("", |capture| capture.as_str());
        attributes.insert(name.as_str().to_ascii_lowercase(), value.to_string());
    }
    attributes
}

fn decode_html_entities(value: &str) -> String {
    let pattern = Regex::new(r"&(?i:(amp|quot|apos|lt|gt|#\d+|#x[0-9a-f]+));")
        .expect("valid HTML entity regex");
    pattern
        .replace_all(value, |captures: &Captures<'_>| {
            let entity = captures.get(0).map_or("", |capture| capture.as_str());
            let key = captures
                .get(1)
                .map_or("", |capture| capture.as_str())
                .to_ascii_lowercase();
            match key.as_str() {
                "amp" => "&".to_string(),
                "quot" => "\"".to_string(),
                "apos" => "'".to_string(),
                "lt" => "<".to_string(),
                "gt" => ">".to_string(),
                _ => {
                    let parsed = if let Some(hex) = key.strip_prefix("#x") {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        key.strip_prefix('#').and_then(|digits| digits.parse().ok())
                    };
                    parsed
                        .and_then(char::from_u32)
                        .map(|character| character.to_string())
                        .unwrap_or_else(|| entity.to_string())
                }
            }
        })
        .into_owned()
}

fn decode_backslash_escapes(value: &str) -> String {
    let slash_decoded = value.replace("\\/", "/");
    let unicode_pattern = Regex::new(r"\\u([0-9a-fA-F]{4})").expect("valid unicode regex");
    let unicode_decoded = unicode_pattern.replace_all(&slash_decoded, |captures: &Captures<'_>| {
        let original = captures.get(0).map_or("", |capture| capture.as_str());
        captures
            .get(1)
            .and_then(|capture| u32::from_str_radix(capture.as_str(), 16).ok())
            .and_then(char::from_u32)
            .map(|character| character.to_string())
            .unwrap_or_else(|| original.to_string())
    });
    let hex_pattern = Regex::new(r"\\x([0-9a-fA-F]{2})").expect("valid hex regex");
    hex_pattern
        .replace_all(&unicode_decoded, |captures: &Captures<'_>| {
            let original = captures.get(0).map_or("", |capture| capture.as_str());
            captures
                .get(1)
                .and_then(|capture| u32::from_str_radix(capture.as_str(), 16).ok())
                .and_then(char::from_u32)
                .map(|character| character.to_string())
                .unwrap_or_else(|| original.to_string())
        })
        .into_owned()
}

fn normalize_candidate_url(value: &str) -> Option<Url> {
    let mut normalized = decode_backslash_escapes(&decode_html_entities(value.trim()));
    let lower = normalized.to_ascii_lowercase();
    if lower.starts_with("http%3a") || lower.starts_with("https%3a") {
        normalized = percent_decode_str(&normalized)
            .decode_utf8()
            .ok()?
            .into_owned();
    }
    if normalized.starts_with("//") {
        normalized.insert_str(0, "https:");
    }

    let mut url = Url::parse(&normalized).ok()?;
    if assert_basic_https_url(&url, "The media URL").is_err() {
        return None;
    }
    url.set_fragment(None);

    let path = url.path().to_ascii_lowercase();
    if [".avif", ".gif", ".jpg", ".jpeg", ".png", ".svg", ".webp"]
        .iter()
        .any(|extension| path.ends_with(extension))
        || ["avatar", "cover", "poster"]
            .iter()
            .any(|marker| path.contains(marker))
    {
        return None;
    }
    Some(url)
}

fn extract_address_urls(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::String(url) => output.push(url.clone()),
        Value::Array(values) => {
            for value in values {
                extract_address_urls(value, output);
            }
        }
        Value::Object(record) => {
            for key in ["url_list", "urlList", "urls"] {
                if let Some(Value::Array(values)) = record.get(key) {
                    for value in values {
                        extract_address_urls(value, output);
                    }
                    return;
                }
            }
            if let Some(Value::String(url)) = record.get("url") {
                output.push(url.clone());
            }
        }
        _ => {}
    }
}

fn finite_number(value: Option<&Value>) -> Option<f64> {
    let number = match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(number) => number.parse::<f64>().ok(),
        _ => None,
    }?;
    number.is_finite().then_some(number)
}

fn first_string(record: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| record.get(*key).and_then(Value::as_str).map(str::to_string))
}

fn metadata_from_record(record: &serde_json::Map<String, Value>) -> CandidateMetadata {
    CandidateMetadata {
        bitrate: finite_number(
            record
                .get("bit_rate")
                .or_else(|| record.get("bitrate"))
                .or_else(|| record.get("bitRate")),
        ),
        width: finite_number(record.get("width")),
        height: finite_number(record.get("height")),
        codec: first_string(record, &["codec_type", "codecType", "codec"]),
        format: first_string(record, &["format", "gear_name", "gearName"]),
    }
}

fn candidate_score(
    source: SourceInfo,
    url: &Url,
    directness: Directness,
    metadata: &CandidateMetadata,
) -> f64 {
    let mut score = source.score;
    let lower_path = url.path().to_ascii_lowercase();
    if directness == Directness::CdnMedia {
        score += 20.0;
    }
    if [".mp4", ".m4v", ".mov"]
        .iter()
        .any(|extension| lower_path.ends_with(extension))
    {
        score += 16.0;
    }
    if url
        .query_pairs()
        .any(|(key, value)| key == "mime_type" && value == "video_mp4")
    {
        score += 14.0;
    }
    if lower_path.contains("/video/") {
        score += 10.0;
    }
    if let Some(bitrate) = metadata.bitrate.filter(|value| *value > 0.0) {
        score += (bitrate + 1.0).log10().mul_add(2.0, 0.0).min(12.0);
    }
    if metadata.width.is_some_and(|value| value != 0.0)
        && metadata.height.is_some_and(|value| value != 0.0)
    {
        score += 6.0;
    }
    if lower_path.contains("/playwm/")
        || url
            .query_pairs()
            .any(|(key, value)| key == "watermark" && value == "1")
    {
        score -= 35.0;
    }
    if directness == Directness::Unknown {
        score -= 20.0;
    }
    (score * 100.0).round() / 100.0
}

fn balanced_json_after_assignment<'a>(script: &'a str, marker: &str) -> Option<&'a str> {
    let marker_index = script.find(marker)?;
    let equals_relative = script[marker_index + marker.len()..].find('=')?;
    let mut start = marker_index + marker.len() + equals_relative + 1;
    while let Some(character) = script[start..].chars().next() {
        if !character.is_whitespace() {
            break;
        }
        start += character.len_utf8();
    }
    let first = script.as_bytes().get(start).copied()?;
    if first != b'{' && first != b'[' {
        return None;
    }

    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (relative, byte) in script.as_bytes()[start..].iter().copied().enumerate() {
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
        if byte == b'"' {
            in_string = true;
            continue;
        }
        if byte == b'{' || byte == b'[' {
            stack.push(byte);
        } else if byte == b'}' || byte == b']' {
            let expected = if byte == b'}' { b'{' } else { b'[' };
            if stack.pop() != Some(expected) {
                return None;
            }
            if stack.is_empty() {
                return script.get(start..=start + relative);
            }
        }
    }
    None
}

fn parse_json_payload(value: &str, uri_encoded: bool) -> Option<Value> {
    let mut text = decode_html_entities(value.trim());
    let lower = text.to_ascii_lowercase();
    if uri_encoded
        || lower.starts_with("%7b")
        || lower.starts_with("%5b")
        || lower.starts_with("%22")
    {
        text = percent_decode_str(&text).decode_utf8().ok()?.into_owned();
    }

    let mut parsed: Value = serde_json::from_str(&text).ok()?;
    for _ in 0..2 {
        let Value::String(nested) = parsed else {
            return Some(parsed);
        };
        parsed = serde_json::from_str(&nested).ok()?;
    }
    Some(parsed)
}

fn collect_structured_payloads(html: &str) -> (Vec<(String, Value)>, usize) {
    let script_pattern =
        Regex::new(r"(?is)<script\b([^>]*)>(.*?)</script\s*>").expect("valid script regex");
    let mut payloads = Vec::new();
    let mut script_count = 0;

    for captures in script_pattern.captures_iter(html) {
        script_count += 1;
        let attributes = parse_attributes(captures.get(1).map_or("", |value| value.as_str()));
        let body = captures.get(2).map_or("", |value| value.as_str()).trim();
        if body.is_empty() {
            continue;
        }

        let mut parsed = None;
        let mut source = format!("script[{}]", script_count - 1);
        for marker in JSON_ASSIGNMENTS {
            let Some(json) = balanced_json_after_assignment(body, marker) else {
                continue;
            };
            parsed = parse_json_payload(json, false);
            source = (*marker).to_string();
            if parsed.is_some() {
                break;
            }
        }

        let id = attributes.get("id").map(|value| value.to_ascii_lowercase());
        let content_type = attributes
            .get("type")
            .map(|value| value.to_ascii_lowercase());
        if parsed.is_none() && matches!(id.as_deref(), Some("render_data" | "render-data")) {
            parsed = parse_json_payload(body, true);
            source = attributes.get("id").cloned().unwrap_or_default();
        }
        if parsed.is_none()
            && matches!(
                content_type.as_deref(),
                Some("application/json" | "application/ld+json")
            )
        {
            parsed = parse_json_payload(body, false);
            source = attributes
                .get("id")
                .cloned()
                .or(content_type)
                .unwrap_or_default();
        }
        if let Some(value) = parsed {
            payloads.push((source, value));
        }
    }
    (payloads, script_count)
}

fn find_video_roots<'a>(value: &'a Value, video_id: &str) -> (Vec<&'a Value>, bool) {
    fn visit<'a>(
        node: &'a Value,
        video_id: &str,
        depth: usize,
        nodes: &mut usize,
        roots: &mut Vec<&'a Value>,
        saw_identified_video: &mut bool,
    ) {
        if depth > MAX_JSON_DEPTH || *nodes > MAX_JSON_NODES {
            return;
        }
        *nodes += 1;
        match node {
            Value::Object(record) => {
                if let Some(aweme_id) = record.get("aweme_id").or_else(|| record.get("awemeId")) {
                    *saw_identified_video = true;
                    let matches = match aweme_id {
                        Value::String(value) => value == video_id,
                        Value::Number(value) => value.to_string() == video_id,
                        _ => false,
                    };
                    if matches && record.get("video").is_some_and(Value::is_object) {
                        roots.push(node);
                    }
                }
                for child in record.values() {
                    visit(
                        child,
                        video_id,
                        depth + 1,
                        nodes,
                        roots,
                        saw_identified_video,
                    );
                }
            }
            Value::Array(values) => {
                for child in values {
                    visit(
                        child,
                        video_id,
                        depth + 1,
                        nodes,
                        roots,
                        saw_identified_video,
                    );
                }
            }
            _ => {}
        }
    }

    let mut roots = Vec::new();
    let mut nodes = 0;
    let mut saw_identified_video = false;
    visit(
        value,
        video_id,
        0,
        &mut nodes,
        &mut roots,
        &mut saw_identified_video,
    );
    (roots, saw_identified_video)
}

fn collect_from_structured_value(
    value: &Value,
    source: &str,
    video_id: Option<&str>,
    candidates: &mut CandidateSet,
    state: &mut CollectionState,
) {
    fn visit(
        node: &Value,
        path: &str,
        source: &str,
        depth: usize,
        candidates: &mut CandidateSet,
        state: &mut CollectionState,
    ) {
        if depth > MAX_JSON_DEPTH || state.nodes > MAX_JSON_NODES {
            return;
        }
        state.nodes += 1;
        match node {
            Value::Array(values) => {
                for (index, child) in values.iter().enumerate() {
                    visit(
                        child,
                        &format!("{path}[{index}]"),
                        source,
                        depth + 1,
                        candidates,
                        state,
                    );
                }
            }
            Value::Object(record) => {
                let metadata = metadata_from_record(record);
                for (key, child) in record {
                    let child_path = format!("{path}.{key}");
                    if let Some(info) = source_info(&normalized_key(key)) {
                        let mut urls = Vec::new();
                        extract_address_urls(child, &mut urls);
                        for raw_url in urls {
                            let Some(url) = normalize_candidate_url(&raw_url) else {
                                continue;
                            };
                            let directness = media_classification(&url);
                            let candidate = Candidate {
                                url: url.to_string(),
                                source_path: child_path.clone(),
                                source_kind: info.kind.to_string(),
                                directness,
                                playback_variant: playback_variant_for_url(&url),
                                width: metadata.width,
                                height: metadata.height,
                                bitrate: metadata.bitrate,
                                codec: metadata.codec.clone(),
                                format: metadata.format.clone(),
                                score: candidate_score(info, &url, directness, &metadata),
                                evidence: vec![key.clone(), directness.as_str().to_string()],
                                sources: vec![source.to_string()],
                                order: state.order,
                            };
                            state.order += 1;
                            candidates.merge(candidate);
                        }
                    }
                    visit(child, &child_path, source, depth + 1, candidates, state);
                }
            }
            _ => {}
        }
    }

    match video_id {
        Some(video_id) => {
            let (roots, saw_identified_video) = find_video_roots(value, video_id);
            if roots.is_empty() && saw_identified_video {
                return;
            }
            if roots.is_empty() {
                visit(value, source, source, 0, candidates, state);
            } else {
                for (index, root) in roots.into_iter().enumerate() {
                    visit(
                        root,
                        &format!("{source}[{index}]"),
                        source,
                        0,
                        candidates,
                        state,
                    );
                }
            }
        }
        None => visit(value, source, source, 0, candidates, state),
    }
}

fn collect_html_media_tags(html: &str, candidates: &mut CandidateSet, state: &mut CollectionState) {
    let meta_pattern = Regex::new(r"(?is)<meta\b([^>]*)>").expect("valid meta regex");
    for captures in meta_pattern.captures_iter(html) {
        let attributes = parse_attributes(captures.get(1).map_or("", |value| value.as_str()));
        let property = attributes
            .get("property")
            .or_else(|| attributes.get("name"))
            .map_or_else(String::new, |value| value.to_ascii_lowercase());
        if !["og:video", "og:video:url", "og:video:secure_url"].contains(&property.as_str()) {
            continue;
        }
        let Some(url) = attributes
            .get("content")
            .and_then(|value| normalize_candidate_url(value))
        else {
            continue;
        };
        let info = SourceInfo {
            kind: "video-url",
            score: 68.0,
        };
        let directness = media_classification(&url);
        candidates.merge(Candidate {
            url: url.to_string(),
            source_path: format!("meta[{property}]"),
            source_kind: info.kind.to_string(),
            directness,
            playback_variant: playback_variant_for_url(&url),
            width: None,
            height: None,
            bitrate: None,
            codec: None,
            format: None,
            score: candidate_score(info, &url, directness, &CandidateMetadata::default()),
            evidence: vec![property, directness.as_str().to_string()],
            sources: vec!["html-meta".to_string()],
            order: state.order,
        });
        state.order += 1;
    }

    let media_pattern =
        Regex::new(r"(?is)<(?:video|source)\b([^>]*)>").expect("valid media element regex");
    for captures in media_pattern.captures_iter(html) {
        let attributes = parse_attributes(captures.get(1).map_or("", |value| value.as_str()));
        let Some(url) = attributes
            .get("src")
            .and_then(|value| normalize_candidate_url(value))
        else {
            continue;
        };
        let info = SourceInfo {
            kind: "video-url",
            score: 66.0,
        };
        let directness = media_classification(&url);
        candidates.merge(Candidate {
            url: url.to_string(),
            source_path: "html-media-src".to_string(),
            source_kind: info.kind.to_string(),
            directness,
            playback_variant: playback_variant_for_url(&url),
            width: None,
            height: None,
            bitrate: None,
            codec: None,
            format: None,
            score: candidate_score(info, &url, directness, &CandidateMetadata::default()),
            evidence: vec!["media src".to_string(), directness.as_str().to_string()],
            sources: vec!["html-media".to_string()],
            order: state.order,
        });
        state.order += 1;
    }
}

fn normalize_image_url(value: &str) -> Option<Url> {
    let normalized = decode_backslash_escapes(&decode_html_entities(value.trim()));
    let normalized = if normalized.starts_with("//") {
        format!("https:{normalized}")
    } else {
        normalized
    };
    let url = Url::parse(&normalized).ok()?;
    let hostname = assert_basic_https_url(&url, "The image URL").ok()?;
    IMAGE_HOST_SUFFIXES
        .iter()
        .any(|suffix| host_matches(&hostname, suffix))
        .then_some(url)
}

fn collect_title_and_images(
    value: &Value,
    title: &mut Option<String>,
    images: &mut Vec<ShareImage>,
) {
    fn visit(node: &Value, title: &mut Option<String>, images: &mut Vec<ShareImage>, depth: usize) {
        if depth > MAX_JSON_DEPTH || images.len() >= 50 {
            return;
        }
        match node {
            Value::Object(record) => {
                if title.is_none() {
                    for key in ["desc", "description", "title"] {
                        if let Some(value) = record
                            .get(key)
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                        {
                            *title = Some(value.to_string());
                            break;
                        }
                    }
                }
                for key in ["images", "image_list", "imageList", "image_list_v2"] {
                    if let Some(Value::Array(list)) = record.get(key) {
                        for item in list {
                            let mut urls = Vec::new();
                            extract_address_urls(item, &mut urls);
                            for raw in urls {
                                let Some(url) = normalize_image_url(&raw) else {
                                    continue;
                                };
                                if !images.iter().any(|image| image.url == url.as_str()) {
                                    images.push(ShareImage {
                                        url: url.to_string(),
                                        index: images.len(),
                                    });
                                }
                            }
                        }
                    }
                }
                for child in record.values() {
                    visit(child, title, images, depth + 1);
                }
            }
            Value::Array(values) => {
                for child in values {
                    visit(child, title, images, depth + 1);
                }
            }
            _ => {}
        }
    }
    visit(value, title, images, 0);
}

fn extract_video_candidates_from_value(
    value: &Value,
    source: &str,
    video_id: Option<&str>,
) -> ExtractionResult {
    let mut candidates = CandidateSet::default();
    let mut state = CollectionState::default();
    collect_from_structured_value(value, source, video_id, &mut candidates, &mut state);

    ExtractionResult {
        candidates: candidates.into_sorted(),
        stats: ExtractionStats {
            scripts: 0,
            parsed_payloads: 1,
            visited_nodes: state.nodes,
        },
        warnings: Vec::new(),
    }
}

fn extract_video_candidates_from_html(
    html: &str,
    video_id: Option<&str>,
) -> Result<ExtractionResult, ResolveError> {
    if html.len() > MAX_DOCUMENT_BYTES {
        return Err(ResolveError::upstream(
            "document_too_large",
            "The public video document is too large to inspect.",
        ));
    }

    let (payloads, script_count) = collect_structured_payloads(html);
    let parsed_payloads = payloads.len();
    let mut candidates = CandidateSet::default();
    let mut state = CollectionState::default();
    for (source, payload) in &payloads {
        collect_from_structured_value(payload, source, video_id, &mut candidates, &mut state);
    }
    collect_html_media_tags(html, &mut candidates, &mut state);

    Ok(ExtractionResult {
        candidates: candidates.into_sorted(),
        stats: ExtractionStats {
            scripts: script_count,
            parsed_payloads,
            visited_nodes: state.nodes,
        },
        warnings: if parsed_payloads == 0 {
            vec!["No supported public hydration payload was found in the document.".to_string()]
        } else {
            Vec::new()
        },
    })
}

fn build_http_client() -> Result<Client, ResolveError> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(REQUEST_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| {
            ResolveError::upstream(
                "client_initialization_failed",
                format!("The HTTP client could not be initialized: {error}"),
            )
        })
}

async fn send_request(
    client: &Client,
    method: Method,
    url: &Url,
    media: bool,
) -> Result<Response, ResolveError> {
    let request = client
        .request(method, url.clone())
        .header(USER_AGENT, MOBILE_USER_AGENT)
        .header(
            ACCEPT,
            if media {
                "video/*,*/*;q=0.5"
            } else {
                "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.7"
            },
        );
    let request = if media {
        request
    } else {
        request.header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.6")
    };
    request.send().await.map_err(|error| {
        ResolveError::upstream(
            "upstream_request_failed",
            format!("Public document request failed: {error}"),
        )
    })
}

fn response_location(response: &Response) -> Option<String> {
    response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

async fn read_text_limited(mut response: Response) -> Result<String, ResolveError> {
    let declared_length = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared_length.is_some_and(|length| length > MAX_DOCUMENT_BYTES as u64) {
        return Err(ResolveError::upstream(
            "document_too_large",
            "The public video document exceeds the size limit.",
        ));
    }

    let mut bytes = Vec::with_capacity(declared_length.unwrap_or(0) as usize);
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        ResolveError::upstream(
            "upstream_request_failed",
            format!("The public video document could not be read: {error}"),
        )
    })? {
        if bytes.len() + chunk.len() > MAX_DOCUMENT_BYTES {
            return Err(ResolveError::upstream(
                "document_too_large",
                "The public video document exceeds the size limit.",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn discover_video_document(
    input_url: Url,
    client: &Client,
) -> Result<(String, Url), ResolveError> {
    let mut current = input_url;
    for index in 0..=MAX_REDIRECTS {
        if let Some(video_id) = extract_video_id(&current) {
            return Ok((video_id.clone(), canonical_video_document(&video_id)?));
        }
        if index == MAX_REDIRECTS {
            return Err(ResolveError::upstream(
                "too_many_redirects",
                "The share link exceeded the redirect limit.",
            ));
        }

        let response = send_request(client, Method::GET, &current, false).await?;
        if !response.status().is_redirection() {
            return Err(ResolveError::new(
                "video_id_not_found",
                "The share link did not resolve to a public video document.",
                422,
            ));
        }
        let location = response_location(&response).ok_or_else(|| {
            ResolveError::upstream(
                "redirect_missing_location",
                "A share redirect did not include a destination.",
            )
        })?;
        current = validate_document_url(current.join(&location).map_err(|_| {
            ResolveError::bad_request("invalid_url", "The share URL is not valid.")
        })?)?;
    }
    Err(ResolveError::new(
        "video_id_not_found",
        "No video ID was found.",
        422,
    ))
}

async fn fetch_public_video_document(
    document_url: Url,
    client: &Client,
) -> Result<(Url, String), ResolveError> {
    let mut current = document_url;
    for _ in 0..3 {
        let response = send_request(client, Method::GET, &current, false).await?;
        if response.status().is_redirection() {
            let Some(location) = response_location(&response) else {
                break;
            };
            current = validate_document_url(current.join(&location).map_err(|_| {
                ResolveError::bad_request("invalid_url", "The share URL is not valid.")
            })?)?;
            continue;
        }
        if !response.status().is_success() {
            return Err(ResolveError::upstream(
                "document_fetch_failed",
                format!(
                    "The public video document returned HTTP {}.",
                    response.status().as_u16()
                ),
            ));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !content_type.is_empty()
            && !content_type.contains("text/html")
            && !content_type.contains("application/json")
        {
            return Err(ResolveError::upstream(
                "unexpected_document_type",
                format!("Unexpected public document type: {content_type}"),
            ));
        }
        return Ok((current, read_text_limited(response).await?));
    }
    Err(ResolveError::upstream(
        "document_redirect_failed",
        "The public video document redirect could not be resolved.",
    ))
}

async fn fetch_public_video_detail(
    document_url: &Url,
    video_id: &str,
    client: &Client,
) -> Result<Value, ResolveError> {
    let detail_url = public_video_detail_url(document_url, video_id)?;
    let response = client
        .get(detail_url)
        .header(USER_AGENT, MOBILE_USER_AGENT)
        .header(ACCEPT, "application/json,text/plain,*/*;q=0.5")
        .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.6")
        .header(REFERER, document_url.as_str())
        .header("Agw-Js-Conv", "str")
        .send()
        .await
        .map_err(|error| {
            ResolveError::upstream(
                "detail_request_failed",
                format!("Public video detail request failed: {error}"),
            )
        })?;
    if !response.status().is_success() {
        return Err(ResolveError::upstream(
            "detail_fetch_failed",
            format!(
                "The public video detail endpoint returned HTTP {}.",
                response.status().as_u16()
            ),
        ));
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.is_empty() && !content_type.contains("application/json") {
        return Err(ResolveError::upstream(
            "unexpected_detail_type",
            format!("Unexpected public video detail type: {content_type}"),
        ));
    }

    let body = read_text_limited(response).await?;
    if body.trim().is_empty() {
        return Err(ResolveError::upstream(
            "empty_detail_response",
            "The public video detail endpoint returned an empty response.",
        ));
    }
    let detail = serde_json::from_str::<Value>(&body).map_err(|error| {
        ResolveError::upstream(
            "invalid_detail_response",
            format!("The public video detail response was not valid JSON: {error}"),
        )
    })?;
    let status_code = detail.get("status_code").and_then(|value| match value {
        Value::Number(value) => value.as_i64(),
        Value::String(value) => value.parse::<i64>().ok(),
        _ => None,
    });
    if status_code.is_some_and(|code| code != 0) {
        return Err(ResolveError::upstream(
            "detail_unavailable",
            format!(
                "The public video detail endpoint returned status code {}.",
                status_code.unwrap_or_default()
            ),
        ));
    }
    Ok(detail)
}

async fn resolve_media_redirect(candidate_url: &str, client: &Client) -> Result<Url, ResolveError> {
    let mut current = parse_media_url(candidate_url, None)?;
    for _ in 0..4 {
        if media_classification(&current) == Directness::CdnMedia {
            return Ok(current);
        }
        let response = send_request(client, Method::HEAD, &current, true).await?;
        if !response.status().is_redirection() {
            return Err(ResolveError::upstream(
                "media_redirect_failed",
                format!(
                    "The public play endpoint returned HTTP {}.",
                    response.status().as_u16()
                ),
            ));
        }
        let location = response_location(&response).ok_or_else(|| {
            ResolveError::upstream(
                "media_redirect_missing_location",
                "The public play endpoint did not return a media location.",
            )
        })?;
        current = parse_media_url(&location, Some(&current))?;
    }
    Err(ResolveError::upstream(
        "media_redirect_limit",
        "The media redirect exceeded the redirect limit.",
    ))
}

async fn resolve_media_candidates(
    candidates: &[Candidate],
    client: &Client,
    warnings: &mut Vec<String>,
) -> Result<(Option<String>, Option<String>), ResolveError> {
    for candidate in candidates.iter().take(6) {
        if candidate.directness == Directness::CdnMedia {
            return Ok((
                Some(candidate.url.clone()),
                candidate
                    .playback_variant
                    .clone()
                    .or_else(|| Some("direct".to_string())),
            ));
        }
        if candidate.directness != Directness::PlayEndpoint {
            continue;
        }

        let clean_url = derive_clean_play_url(&candidate.url)?;
        let mut attempts = Vec::new();
        if let Some(clean) = &clean_url {
            attempts.push((clean.to_string(), "play".to_string()));
            attempts.push((candidate.url.clone(), "playwm".to_string()));
        } else {
            attempts.push((
                candidate.url.clone(),
                candidate
                    .playback_variant
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            ));
        }
        let mut clean_attempt_failed = false;
        for (url, variant) in attempts {
            match resolve_media_redirect(&url, client).await {
                Ok(resolved) => {
                    if clean_attempt_failed && variant == "playwm" {
                        push_warning(
                            warnings,
                            "The clean playback variant was unavailable; the original playback entry was used.",
                        );
                    }
                    return Ok((Some(resolved.to_string()), Some(variant)));
                }
                Err(error) if clean_url.is_some() && variant == "play" => {
                    clean_attempt_failed = true;
                    let _ = error;
                }
                Err(error) => push_warning(warnings, error.error),
            }
        }
    }
    Ok((None, None))
}

fn push_warning(warnings: &mut Vec<String>, warning: impl Into<String>) {
    let warning = warning.into();
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

async fn resolve_share_text_with_client(
    share_text: &str,
    client: &Client,
) -> Result<ShareResolveResponse, ResolveError> {
    let input_url = extract_share_url(share_text)?;
    let (video_id, canonical_url) = discover_video_document(input_url, client).await?;
    let (document_url, html) = fetch_public_video_document(canonical_url, client).await?;
    let mut extraction = extract_video_candidates_from_html(&html, Some(&video_id))?;
    let mut title = None;
    let mut images = Vec::new();
    for (_, payload) in collect_structured_payloads(&html).0 {
        collect_title_and_images(&payload, &mut title, &mut images);
    }
    let _stats = &extraction.stats;
    let mut warnings = extraction.warnings;
    let (mut media_url, mut media_variant) =
        resolve_media_candidates(&extraction.candidates, client, &mut warnings).await?;

    if media_url.is_none() {
        match fetch_public_video_detail(&document_url, &video_id, client).await {
            Ok(detail) => {
                let detail_extraction = extract_video_candidates_from_value(
                    &detail,
                    "public video detail",
                    Some(&video_id),
                );
                if detail_extraction.candidates.is_empty() {
                    push_warning(
                        &mut warnings,
                        "The public video detail response did not include a supported media address.",
                    );
                } else {
                    extraction = detail_extraction;
                    collect_title_and_images(&detail, &mut title, &mut images);
                    push_warning(
                        &mut warnings,
                        "The share page did not include media metadata; used the public video detail response.",
                    );
                    (media_url, media_variant) =
                        resolve_media_candidates(&extraction.candidates, client, &mut warnings)
                            .await?;
                }
            }
            Err(error) => push_warning(&mut warnings, error.error),
        }
    }

    if let Some(url) = &media_url {
        let variant = media_variant.clone();
        extraction
            .candidates
            .retain(|candidate| candidate.url != *url);
        extraction.candidates.insert(
            0,
            Candidate {
                url: url.clone(),
                source_path: "media redirect location".to_string(),
                source_kind: if variant.as_deref() == Some("play") {
                    "resolved-clean-cdn".to_string()
                } else {
                    "resolved-cdn".to_string()
                },
                directness: Directness::CdnMedia,
                playback_variant: variant,
                width: None,
                height: None,
                bitrate: None,
                codec: None,
                format: None,
                score: 130.0,
                evidence: vec!["public HTTP redirect".to_string(), "cdn-media".to_string()],
                sources: vec!["media-redirect".to_string()],
                order: 0,
            },
        );
        push_warning(
            &mut warnings,
            "Direct CDN URLs are temporary and may expire.",
        );
    }

    Ok(ShareResolveResponse {
        status: if media_url.is_some() {
            "resolved".to_string()
        } else {
            "document-only".to_string()
        },
        document_url: Some(document_url.to_string()),
        video_id: Some(video_id),
        title,
        images,
        media_url,
        playback_variant: media_variant,
        candidates: extraction
            .candidates
            .iter()
            .take(10)
            .map(Candidate::public)
            .collect(),
        warnings,
    })
}

#[derive(Clone)]
pub struct DouyinResolver {
    client: Client,
}

impl DouyinResolver {
    pub fn new() -> Result<Self, ResolveError> {
        Ok(Self {
            client: build_http_client()?,
        })
    }

    pub async fn resolve_share_text(
        &self,
        share_text: &str,
    ) -> Result<ShareResolveResponse, ResolveError> {
        resolve_share_text_with_client(share_text, &self.client).await
    }
}

pub async fn resolve_share_text(share_text: &str) -> Result<ShareResolveResponse, ResolveError> {
    DouyinResolver::new()?.resolve_share_text(share_text).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO_ID: &str = "7657414745401989745";
    const PLAYWM_URL: &str = "https://aweme.snssdk.com/aweme/v1/playwm/?line=0&line=1&video_id=opaque%2Btoken&ratio=1080p";
    const CLEAN_URL: &str =
        "https://aweme.snssdk.com/aweme/v1/play/?line=0&line=1&video_id=opaque%2Btoken&ratio=1080p";
    const CDN_URL: &str =
        "https://v3-dy-o.zjcdn.com/obj/tos/video/segment.mp4?mime_type=video_mp4&expires=123";
    const DETAIL_CDN_URL: &str = "https://v5-gz2-colda.douyinvod.com/obj/tos/video/segment.mp4?mime_type=video_mp4&expires=123";

    #[test]
    fn document_url_validation_uses_named_allowlisted_https_hosts() {
        assert_eq!(
            assert_document_url("https://v.douyin.com/short-code/")
                .expect("allowlisted URL")
                .as_str(),
            "https://v.douyin.com/short-code/"
        );

        for (url, code) in [
            (
                "https://v.douyin.com.attacker.example/short-code/",
                "unsupported_host",
            ),
            (
                "https://v.douyin.com@attacker.example/short-code/",
                "invalid_url",
            ),
            ("https://127.0.0.1/short-code/", "invalid_url"),
            ("https://v.douyin.com:444/short-code/", "invalid_url"),
            ("http://v.douyin.com/short-code/", "invalid_url"),
        ] {
            assert_eq!(assert_document_url(url).unwrap_err().code, code);
        }
    }

    #[test]
    fn share_text_extracts_the_first_url_and_trims_share_punctuation() {
        let url = extract_share_url("8.46 copy and open https://v.douyin.com/pL7ReZweqLE/\u{3002}")
            .expect("share URL");
        assert_eq!(url.as_str(), "https://v.douyin.com/pL7ReZweqLE/");
    }

    #[test]
    fn share_text_extracts_note_urls_from_image_posts() {
        let url = extract_share_url(
            "看看图文作品 https://www.iesdouyin.com/share/note/7682062654509331178/",
        )
        .expect("note URL should be extracted");
        assert_eq!(
            extract_video_id(&url).as_deref(),
            Some("7682062654509331178")
        );
    }

    #[test]
    fn clean_play_derivation_changes_only_the_endpoint_path() {
        let clean = derive_clean_play_url(PLAYWM_URL)
            .expect("valid playback URL")
            .expect("clean playback variant");
        assert_eq!(clean.path(), "/aweme/v1/play/");
        assert_eq!(clean.query(), Url::parse(PLAYWM_URL).unwrap().query());
        assert_eq!(clean.as_str(), CLEAN_URL);
    }

    #[test]
    fn public_video_detail_url_keeps_the_document_origin_and_uses_stable_parameters() {
        let document_url =
            Url::parse("https://www.iesdouyin.com/share/video/7657414745401989745/").unwrap();
        let detail_url = public_video_detail_url(&document_url, VIDEO_ID).unwrap();
        assert_eq!(
            detail_url.as_str(),
            "https://www.iesdouyin.com/aweme/v1/aweme/detail/?aweme_id=7657414745401989745&aid=6383&device_platform=web&version_code=280500&version_name=28.5.0"
        );
    }

    #[test]
    fn current_share_shell_has_no_embedded_media_candidate() {
        let html = format!(
            r#"<script>window._ROUTER_DATA = {{"loaderData":{{"video_(id)/page":{{"itemId":"{VIDEO_ID}"}}}}}};</script>"#
        );
        let result = extract_video_candidates_from_html(&html, Some(VIDEO_ID)).unwrap();
        assert_eq!(result.stats.parsed_payloads, 1);
        assert!(result.candidates.is_empty());
    }

    #[test]
    fn public_detail_payload_produces_direct_cdn_candidates() {
        let detail = serde_json::json!({
            "status_code": 0,
            "aweme_detail": {
                "aweme_id": VIDEO_ID,
                "video": {
                    "width": 1920,
                    "height": 1080,
                    "bit_rate": 6400000,
                    "play_addr_h264": {"url_list": [DETAIL_CDN_URL]},
                    "play_addr": {"url_list": [DETAIL_CDN_URL]}
                }
            }
        });
        let result =
            extract_video_candidates_from_value(&detail, "public video detail", Some(VIDEO_ID));
        let candidate = result.candidates.first().expect("direct CDN candidate");
        assert_eq!(candidate.url, DETAIL_CDN_URL);
        assert_eq!(candidate.directness, Directness::CdnMedia);
        assert_eq!(candidate.playback_variant.as_deref(), Some("direct"));
        assert_eq!(candidate.width, Some(1920.0));
        assert_eq!(candidate.height, Some(1080.0));
        assert_eq!(candidate.bitrate, Some(6_400_000.0));
    }

    #[test]
    fn router_data_and_html_tags_produce_ranked_deduplicated_candidates() {
        let encoded_cdn = CDN_URL.replace('&', "&amp;");
        let html = format!(
            r#"<html><head>
              <meta property="og:video" content="https://v3-dy-o.zjcdn.com/obj/tos/video/cover.jpg">
              <meta property="og:video" content="{encoded_cdn}">
            </head><body>
              <video src="https://v3-dy-o.zjcdn.com/obj/tos/video/poster.png"></video>
              <source src="{CDN_URL}">
              <script>window._ROUTER_DATA = {{"data":{{"aweme_id":"{VIDEO_ID}","video":{{"width":1920,"height":1080,"bit_rate":6400000,"play_addr":{{"url_list":["{PLAYWM_URL}","{PLAYWM_URL}"]}}}}}}}};</script>
            </body></html>"#
        );
        let result = extract_video_candidates_from_html(&html, Some(VIDEO_ID)).unwrap();
        assert_eq!(result.stats.parsed_payloads, 1);
        assert_eq!(
            result
                .candidates
                .iter()
                .filter(|candidate| candidate.url == PLAYWM_URL)
                .count(),
            1
        );
        let play = result
            .candidates
            .iter()
            .find(|candidate| candidate.url == PLAYWM_URL)
            .expect("play candidate");
        assert_eq!(play.width, Some(1920.0));
        assert_eq!(play.height, Some(1080.0));
        assert_eq!(play.bitrate, Some(6_400_000.0));
        assert_eq!(
            result
                .candidates
                .iter()
                .filter(|candidate| candidate.url == CDN_URL)
                .count(),
            1
        );
        assert!(!result
            .candidates
            .iter()
            .any(|candidate| candidate.url.contains("cover.jpg")
                || candidate.url.contains("poster.png")));
    }

    #[test]
    fn requested_video_never_uses_another_videos_structured_candidates() {
        let html = r#"<script>window._ROUTER_DATA={"detail":{"aweme_id":"9999999999999999999","video":{"play_addr":{"url_list":["https://aweme.snssdk.com/aweme/v1/playwm/?video_id=wrong-video"]}}}};</script>"#;
        let result = extract_video_candidates_from_html(html, Some(VIDEO_ID)).unwrap();
        assert!(result.candidates.is_empty());
    }

    #[test]
    fn play_is_ranked_before_playwm_without_claiming_visual_status() {
        let play = CLEAN_URL;
        let html = format!(
            r#"<script type="application/json">{{"detail":{{"aweme_id":"{VIDEO_ID}","video":{{"play_addr_h264":{{"url_list":["{play}"]}},"play_addr":{{"url_list":["{PLAYWM_URL}"]}}}}}}}}</script>"#
        );
        let result = extract_video_candidates_from_html(&html, Some(VIDEO_ID)).unwrap();
        let play_index = result
            .candidates
            .iter()
            .position(|candidate| candidate.url == play)
            .unwrap();
        let playwm_index = result
            .candidates
            .iter()
            .position(|candidate| candidate.url == PLAYWM_URL)
            .unwrap();
        assert!(play_index < playwm_index);
    }

    #[test]
    fn pure_parsers_reject_oversized_and_unsupported_inputs() {
        let oversized = "x".repeat(MAX_INPUT_BYTES + 1);
        let error = extract_share_url(&oversized).unwrap_err();
        assert_eq!(error.code, "input_too_large");
        assert_eq!(error.status, 400);

        let oversized_html = "x".repeat(MAX_DOCUMENT_BYTES + 1);
        let error =
            extract_video_candidates_from_html(&oversized_html, Some(VIDEO_ID)).unwrap_err();
        assert_eq!(error.code, "document_too_large");
        assert_eq!(error.status, 502);

        let error = parse_media_url(
            "https://v3-dy-o.zjcdn.com.invalid/obj/video/source.mp4",
            None,
        )
        .unwrap_err();
        assert_eq!(error.code, "unsupported_media_host");
    }

    #[test]
    fn public_detail_payload_produces_title_and_image_list() {
        let detail = serde_json::json!({
            "aweme_detail": {
                "desc": "重新穿上校服",
                "image_list": [
                    {"url_list": [
                        "https://p3-sign.douyinpic.com/tos-cn-i-0813/o0.webp",
                        "https://p3-sign.douyinpic.com/tos-cn-i-0813/o0.jpg"
                    ]},
                    {"url_list": ["https://p9-sign.douyinpic.com/tos-cn-i-0813/o1.webp"]}
                ]
            }
        });
        let mut title = None;
        let mut images = Vec::new();
        collect_title_and_images(&detail, &mut title, &mut images);
        assert_eq!(title.as_deref(), Some("重新穿上校服"));
        assert_eq!(images.len(), 3);
        assert_eq!(images[0].index, 0);
        assert!(images[2].url.ends_with("o1.webp"));
    }
    #[test]
    fn response_and_error_fields_are_camel_case_serializable() {
        let response = ShareResolveResponse {
            status: "resolved".to_string(),
            document_url: Some("https://www.iesdouyin.com/share/video/1/".to_string()),
            video_id: Some("1".to_string()),
            title: None,
            images: Vec::new(),
            media_url: None,
            playback_variant: Some("play".to_string()),
            candidates: Vec::new(),
            warnings: Vec::new(),
        };
        let json = serde_json::to_value(response).unwrap();
        assert!(json.get("documentUrl").is_some());
        assert!(json.get("videoId").is_some());
        assert!(json.get("playbackVariant").is_some());

        let error = ResolveError::new("invalid_url", "bad", 400);
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!({"code":"invalid_url","error":"bad","status":400})
        );
    }

    #[test]
    fn public_document_label_drops_query_and_fragment() {
        let url = Url::parse("https://v.douyin.com/a/?source=copy#fragment").unwrap();
        assert_eq!(public_document_label(&url), "https://v.douyin.com/a/");
    }
}
