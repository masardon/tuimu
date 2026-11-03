use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ytdlp::curl_bin;

const API_ROOT: &str = "https://lrclib.net/api";
const USER_AGENT: &str = "tuimu/0.1.0 (terminal music player)";
const MAX_RETRIES: usize = 2;
const RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Default)]
pub struct Lyrics {
    pub plain: Option<String>,
    pub synced: Option<Vec<LyricLine>>,
    pub instrumental: bool,
    pub source: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LyricLine {
    pub time: f64,
    pub text: String,
}

impl Lyrics {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        !self.instrumental
            && self
                .plain
                .as_deref()
                .is_none_or(|text| text.trim().is_empty())
            && self.synced.as_ref().is_none_or(|lines| lines.is_empty())
    }

    pub fn line_count(&self) -> usize {
        if let Some(synced) = &self.synced {
            synced.len()
        } else if let Some(plain) = &self.plain {
            plain.lines().count()
        } else {
            0
        }
    }

    pub fn with_source(mut self, source: &str) -> Self {
        self.source = Some(source.to_string());
        self
    }

    pub fn current_line(&self, position: f64) -> Option<usize> {
        self.synced
            .as_ref()?
            .iter()
            .rposition(|line| line.time <= position)
    }
}

impl Default for LyricLine {
    fn default() -> Self {
        Self {
            time: 0.0,
            text: String::new(),
        }
    }
}

pub fn fetch(artist: &str, title: &str, duration: Option<f64>) -> Result<Lyrics> {
    let queries = build_queries(artist, title);
    let mut last_error = None;

    for query in &queries {
        if query.title.is_empty() {
            continue;
        }

        // Structured attempts: exact match first, then fuzzy searches. Exact
        // matching with the real duration is authoritative when available.
        let known_duration = duration.filter(|value| value.is_finite() && *value > 60.0);
        if let Some(known) = known_duration {
            match retry(|| fetch_exact(query, Some(known))) {
                Ok(Some(lyrics)) => return Ok(lyrics.with_source("LrcLib")),
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }
        }

        match retry(|| fetch_exact(query, None)) {
            Ok(Some(lyrics)) => return Ok(lyrics.with_source("LrcLib")),
            Ok(None) => {}
            Err(error) => last_error = Some(error),
        }

        match retry(|| fetch_search(query, duration)) {
            Ok(Some(lyrics)) => return Ok(lyrics.with_source("LrcLib")),
            Ok(None) => {}
            Err(error) => last_error = Some(error),
        }

        // Channel-derived artist names are often wrong ("Warner Music
        // Indonesia"); searching by title alone reaches more local tracks.
        if !query.artist.is_empty() {
            let title_only = Query {
                artist: String::new(),
                title: query.title.clone(),
            };
            match retry(|| fetch_search(&title_only, duration)) {
                Ok(Some(lyrics)) => return Ok(lyrics.with_source("LrcLib")),
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }
        }
    }

    // Last resort: LrcLib's free-text search over "artist title".
    if let Some(query) = queries.iter().find(|query| !query.title.is_empty()) {
        let text = format!("{} {}", query.artist, query.title)
            .trim()
            .to_string();
        if !text.is_empty() {
            match retry(|| fetch_free_text(&text, duration)) {
                Ok(Some(lyrics)) => return Ok(lyrics.with_source("LrcLib")),
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }
        }
    }

    match last_error {
        Some(error) => Err(error),
        None => Ok(Lyrics::default()),
    }
}

/// Words that mark YouTube upload clutter rather than track names. Indonesian
/// uploads use "Lirik", "Lagu", and "Video Klip" where English uploads use
/// "Lyrics" and "Official Music Video".
const NOISE_WORDS: &[&str] = &[
    "official",
    "video",
    "videos",
    "lirik",
    "lagu",
    "lyrics",
    "lyric",
    "audio",
    "klip",
    "videoclip",
    "mv",
    "hd",
    "4k",
    "visualizer",
    "remastered",
    "remaster",
    "album",
];

/// Tokens stripped from the end of channel-derived artist names.
const CHANNEL_SUFFIXES: &[&str] = &["official", "vevo", "topic", "channel"];

fn build_queries(artist: &str, title: &str) -> Vec<Query> {
    let raw = Query {
        artist: artist.trim().to_string(),
        title: title.trim().to_string(),
    };
    let normalized = normalize_query(&raw);

    let mut queries = Vec::new();
    for query in [normalized, raw] {
        if !query.title.is_empty() && !queries.contains(&query) {
            queries.push(query);
        }
    }
    queries
}

fn normalize_query(query: &Query) -> Query {
    let (artist, title) = split_artist_title(&query.artist, &query.title);
    Query {
        artist: clean_artist(&artist),
        title: clean_title(&title),
    }
}

/// YouTube music uploads are titled "Artist - Title (...)", often with the
/// label channel as metadata artist. Trust the dash-separated prefix as the
/// artist whenever it is present.
fn split_artist_title(artist: &str, title: &str) -> (String, String) {
    for separator in [" - ", " – ", " — ", " -- "] {
        if let Some((prefix, suffix)) = title.split_once(separator)
            && !prefix.trim().is_empty()
            && !suffix.trim().is_empty()
            && prefix.trim().len() <= 60
        {
            return (prefix.trim().to_string(), suffix.trim().to_string());
        }
    }
    (artist.trim().to_string(), title.trim().to_string())
}

fn clean_title(title: &str) -> String {
    let sanitized: String = title
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => ' ',
            _ => c,
        })
        .collect();
    let text = strip_noise_groups(&sanitized);
    text.split_whitespace()
        .filter(|token| {
            let bare = token
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
            !NOISE_WORDS.contains(&bare.as_str())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Drop parenthesized/bracketed groups that contain upload clutter, e.g.
/// "(Official Music Video)", "[LIRIK]", or "(Full Album)".
fn strip_noise_groups(title: &str) -> String {
    let mut result = String::new();
    let mut group = String::new();
    let mut depth = 0usize;

    for ch in title.chars() {
        match ch {
            '(' | '[' | '{' => {
                depth += 1;
                if depth > 1 {
                    group.push(ch);
                }
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if !group_is_noise(&group) {
                        result.push('(');
                        result.push_str(&group);
                        result.push(')');
                    }
                    group.clear();
                } else {
                    group.push(ch);
                }
            }
            _ if depth > 0 => group.push(ch),
            _ => result.push(ch),
        }
    }

    // Unclosed trailing group, e.g. "Song (Official".
    if !group.is_empty() && !group_is_noise(&group) {
        result.push('(');
        result.push_str(&group);
    }
    result
}

fn group_is_noise(group: &str) -> bool {
    group.split_whitespace().any(|token| {
        let bare = token
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        NOISE_WORDS.contains(&bare.as_str())
    })
}

fn clean_artist(artist: &str) -> String {
    let mut tokens: Vec<&str> = artist.split_whitespace().collect();
    while let Some(last) = tokens.last() {
        if *last == "-" {
            tokens.pop();
            continue;
        }
        let bare = last
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if CHANNEL_SUFFIXES.contains(&bare.as_str()) {
            tokens.pop();
        } else if bare.len() > 4 && bare.ends_with("vevo") {
            let stripped = &last[..last.len() - 4];
            *tokens.last_mut().expect("token exists") = stripped;
        } else {
            break;
        }
    }
    if tokens.is_empty() {
        artist.trim().to_string()
    } else {
        tokens.join(" ")
    }
}

fn retry<T>(mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    let mut last_error = None;
    for attempt in 0..MAX_RETRIES {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < MAX_RETRIES {
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("lyrics request failed")))
}

fn fetch_exact(query: &Query, duration: Option<f64>) -> Result<Option<Lyrics>> {
    let mut params = Vec::new();
    if !query.title.is_empty() {
        params.push(("track_name", query.title.clone()));
    }
    if !query.artist.is_empty() {
        params.push(("artist_name", query.artist.clone()));
    }
    if let Some(duration) = duration {
        params.push(("duration", duration.round().to_string()));
    }

    let Some(json) = get_json_with_timeout(&format!("{}/get", API_ROOT), &params, 10)? else {
        return Ok(None);
    };
    Ok(lyrics_from_object_with_source(&json, "LrcLib"))
}

fn fetch_search(query: &Query, duration: Option<f64>) -> Result<Option<Lyrics>> {
    let mut params = Vec::new();
    if !query.title.is_empty() {
        params.push(("track_name", query.title.clone()));
    }
    if !query.artist.is_empty() {
        params.push(("artist_name", query.artist.clone()));
    }

    let Some(json) = get_json_with_timeout(&format!("{}/search", API_ROOT), &params, 10)? else {
        return Ok(None);
    };
    if let Some(results) = json.as_array()
        && let Some(best) = results
            .iter()
            .filter_map(|item| {
                lyrics_from_object_with_source(item, "LrcLib")
                    .map(|lyrics| (score(item, query, duration), lyrics))
            })
            .max_by(|left, right| left.0.total_cmp(&right.0))
    {
        return Ok(Some(best.1));
    }

    Ok(None)
}

/// Free-text search fallback: `q` searches across artist and track names,
/// which finds local-language tracks whose metadata differs from YouTube.
fn fetch_free_text(text: &str, duration: Option<f64>) -> Result<Option<Lyrics>> {
    let params = vec![("q", text.to_string())];
    let Some(json) = get_json_with_timeout(&format!("{}/search", API_ROOT), &params, 10)? else {
        return Ok(None);
    };
    let Some(results) = json.as_array() else {
        return Ok(None);
    };

    // LrcLib ranks by relevance; among candidates prefer the closest
    // duration so the synced lyrics line up with the audio.
    let mut best: Option<Lyrics> = None;
    let mut best_distance = f64::INFINITY;
    for item in results {
        let Some(lyrics) = lyrics_from_object_with_source(item, "LrcLib") else {
            continue;
        };
        let distance = match (duration, item.get("duration").and_then(Value::as_f64)) {
            (Some(wanted), Some(candidate)) if (wanted - candidate).abs() <= 7.0 => {
                (wanted - candidate).abs()
            }
            _ => f64::INFINITY,
        };
        if best.is_none() || distance < best_distance {
            best = Some(lyrics);
            best_distance = distance;
        }
    }

    Ok(best)
}

fn get_json_with_timeout(
    url: &str,
    params: &[(&str, String)],
    timeout: u64,
) -> Result<Option<Value>> {
    let query_string = params
        .iter()
        .map(|(key, value)| format!("{}={}", key, urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&");

    let full_url = if query_string.is_empty() {
        url.to_string()
    } else {
        format!("{}?{}", url, query_string)
    };

    let mut cmd = Command::new(curl_bin());
    cmd.args([
        "-sS",
        "--max-time",
        &timeout.to_string(),
        "-A",
        USER_AGENT,
        "-w",
        "\n%{http_code}",
    ]);
    cmd.arg(&full_url);

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to start curl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("lyrics request failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (body, status) = stdout
        .rsplit_once('\n')
        .context("lyrics service returned an empty response")?;
    let status = status
        .trim()
        .parse::<u16>()
        .context("lyrics service returned an invalid HTTP status")?;

    match status {
        200..=299 => {
            let json: Value = serde_json::from_str(body)
                .with_context(|| format!("invalid JSON response: {body}"))?;
            Ok(Some(json))
        }
        400 | 404 => Ok(None),
        429 | 500..=599 => bail!("lyrics service is unavailable (HTTP {status})"),
        _ => bail!("lyrics service returned HTTP {status}"),
    }
}

fn lyrics_from_object_with_source(value: &Value, source: &str) -> Option<Lyrics> {
    let mut lyrics = lyrics_from_object(value)?;
    lyrics.source = Some(source.to_string());
    Some(lyrics)
}

fn lyrics_from_object(value: &Value) -> Option<Lyrics> {
    if value.get("instrumental").and_then(Value::as_bool) == Some(true) {
        return Some(Lyrics {
            instrumental: true,
            ..Lyrics::default()
        });
    }

    let plain = value
        .get("plainLyrics")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.trim().is_empty());

    let synced = value
        .get("syncedLyrics")
        .and_then(Value::as_str)
        .and_then(parse_lrc);

    if plain.is_none() && synced.is_none() {
        return None;
    }

    Some(Lyrics {
        plain,
        synced,
        instrumental: false,
        source: None,
    })
}

fn parse_lrc(text: &str) -> Option<Vec<LyricLine>> {
    let mut lines = Vec::new();

    for line in text.lines() {
        if let Some((time, text)) = parse_lrc_line(line) {
            lines.push(LyricLine {
                time,
                text: text.to_string(),
            });
        }
    }

    if lines.is_empty() {
        None
    } else {
        lines.sort_by(|a, b| a.time.total_cmp(&b.time));
        Some(lines)
    }
}

fn parse_lrc_line(line: &str) -> Option<(f64, &str)> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    let start = line.find('[')?;
    let end = line[start + 1..].find(']')?;
    let time_str = &line[start + 1..start + 1 + end];
    let text = &line[start + 1 + end + 1..].trim();

    let time = parse_timestamp(time_str)?;
    Some((time, text))
}

fn parse_timestamp(time_str: &str) -> Option<f64> {
    let (minutes, rest) = time_str.split_once(':')?;
    let minutes = minutes.parse::<u64>().ok()?;

    let (seconds, fraction) = match rest.split_once('.') {
        Some((seconds, fraction)) => (seconds, Some(fraction)),
        None => (rest, None),
    };
    let seconds = seconds.parse::<u64>().ok()?;
    if seconds > 59 {
        return None;
    }

    let fraction = match fraction {
        Some(fraction) if !fraction.is_empty() => {
            let digits = fraction.len();
            let value = fraction.parse::<f64>().ok()?;
            value / 10f64.powi(digits as i32)
        }
        _ => 0.0,
    };

    Some(minutes as f64 * 60.0 + seconds as f64 + fraction)
}

fn score(value: &Value, query: &Query, duration: Option<f64>) -> f64 {
    let mut score = 0.0;

    // Exact match on title gets highest score
    if let Some(title) = value.get("trackName").and_then(Value::as_str)
        && title.to_lowercase() == query.title.to_lowercase()
    {
        score += 100.0;
    }

    // Exact match on artist gets high score
    if let Some(artist) = value.get("artistName").and_then(Value::as_str)
        && artist.to_lowercase() == query.artist.to_lowercase()
    {
        score += 50.0;
    }

    // Partial match on title gets medium score
    if let Some(title) = value.get("trackName").and_then(Value::as_str)
        && title.to_lowercase().contains(&query.title.to_lowercase())
    {
        score += 20.0;
    }

    // Partial match on artist gets low score
    if let Some(artist) = value.get("artistName").and_then(Value::as_str)
        && artist.to_lowercase().contains(&query.artist.to_lowercase())
    {
        score += 10.0;
    }

    // Duration match gets additional score
    if let Some(duration) = duration
        && let Some(track_duration) = value.get("duration").and_then(Value::as_f64)
    {
        let diff = (track_duration - duration).abs();
        if diff < 5.0 {
            score += 5.0;
        } else if diff < 10.0 {
            score += 2.0;
        }
    }

    score
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Query {
    artist: String,
    title: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_timestamp("0:12.34"), Some(12.34));
        assert_eq!(parse_timestamp("1:23.45"), Some(83.45));
        assert_eq!(parse_timestamp("01:23.45"), Some(83.45));
        assert_eq!(parse_timestamp("61:23.45"), Some(3683.45));
        assert_eq!(parse_timestamp("0:12.3"), Some(12.3));
        assert_eq!(parse_timestamp("0:12.345"), Some(12.345));
        assert_eq!(parse_timestamp("invalid"), None);
        assert_eq!(parse_timestamp("01:invalid"), None);
        assert_eq!(parse_timestamp("01:-23"), None);
        assert_eq!(parse_timestamp("01:60.00"), None);
    }

    #[test]
    fn parses_lrc_line() {
        assert_eq!(
            parse_lrc_line("[00:12.34]Hello world"),
            Some((12.34, "Hello world"))
        );
        assert_eq!(
            parse_lrc_line("[01:23.45]Test line"),
            Some((83.45, "Test line"))
        );
        assert_eq!(parse_lrc_line("No timestamp"), None);
        assert_eq!(parse_lrc_line("[invalid]"), None);
    }

    #[test]
    fn parses_lrc() {
        let lrc = "[00:12.34]First line\n[00:15.67]Second line\n[00:20.00]Third line";
        let lyrics = parse_lrc(lrc).unwrap();
        assert_eq!(lyrics.len(), 3);
        assert_eq!(lyrics[0].time, 12.34);
        assert_eq!(lyrics[0].text, "First line");
        assert_eq!(lyrics[1].time, 15.67);
        assert_eq!(lyrics[1].text, "Second line");
        assert_eq!(lyrics[2].time, 20.0);
        assert_eq!(lyrics[2].text, "Third line");
    }

    #[test]
    fn rejects_responses_without_lyrics() {
        let not_found = serde_json::json!({
            "code": 404,
            "name": "TrackNotFound",
            "message": "Failed to find requested track"
        });
        assert!(lyrics_from_object(&not_found).is_none());

        let empty = serde_json::json!({"plainLyrics": null, "syncedLyrics": null});
        assert!(lyrics_from_object(&empty).is_none());
    }

    #[test]
    fn parses_plain_lyrics() {
        let value = serde_json::json!({"plainLyrics": "one\ntwo"});
        let lyrics = lyrics_from_object(&value).unwrap();
        assert_eq!(lyrics.plain.as_deref(), Some("one\ntwo"));
        assert!(lyrics.synced.is_none());
        assert_eq!(lyrics.line_count(), 2);
    }

    #[test]
    fn tracks_current_line() {
        let synced = parse_lrc("[00:10.00]a\n[00:20.00]b\n[00:30.00]c").unwrap();
        let lyrics = Lyrics {
            synced: Some(synced),
            ..Lyrics::default()
        };
        assert_eq!(lyrics.current_line(0.0), None);
        assert_eq!(lyrics.current_line(9.9), None);
        assert_eq!(lyrics.current_line(10.0), Some(0));
        assert_eq!(lyrics.current_line(19.9), Some(0));
        assert_eq!(lyrics.current_line(20.0), Some(1));
        assert_eq!(lyrics.current_line(31.0), Some(2));
    }

    #[test]
    fn cleans_youtube_noise_from_titles() {
        assert_eq!(clean_title("Monokrom (Video Klip Official)"), "Monokrom");
        assert_eq!(clean_title("Dan... (Official Music Video)"), "Dan...");
        assert_eq!(
            clean_title("Hati-Hati di Jalan [LIRIK]"),
            "Hati-Hati di Jalan"
        );
        assert_eq!(
            clean_title("Separuh Aku (Official Music Video LIRIK)"),
            "Separuh Aku"
        );
        assert_eq!(
            clean_title("Kau Adalah (Isyana Sarasvati)"),
            "Kau Adalah (Isyana Sarasvati)"
        );
        assert_eq!(
            clean_title("Cinta Luar Biasa (Audio Official)"),
            "Cinta Luar Biasa"
        );
    }

    #[test]
    fn strips_channel_suffixes_from_artists() {
        assert_eq!(clean_artist("Sheila On 7 - Topic"), "Sheila On 7");
        assert_eq!(clean_artist("TulusVEVO"), "Tulus");
        assert_eq!(clean_artist("Tulus Official"), "Tulus");
        assert_eq!(clean_artist("NOAH Official Channel"), "NOAH");
        assert_eq!(clean_artist("Official"), "Official");
    }

    #[test]
    fn splits_artist_from_youtube_titles() {
        let (artist, title) = split_artist_title(
            "Warner Music Indonesia",
            "Tulus - Monokrom (Video Klip Official)",
        );
        assert_eq!(artist, "Tulus");
        assert_eq!(title, "Monokrom (Video Klip Official)");

        let (artist, title) = split_artist_title("Sheila On 7", "Dan...");
        assert_eq!(artist, "Sheila On 7");
        assert_eq!(title, "Dan...");

        let (artist, title) = split_artist_title("x", "Tulus – Monokrom");
        assert_eq!(artist, "Tulus");
        assert_eq!(title, "Monokrom");
    }

    #[test]
    fn normalizes_local_music_queries() {
        let query = normalize_query(&Query {
            artist: "Tulus Official".into(),
            title: "Tulus - Monokrom (Video Klip Official)".into(),
        });
        assert_eq!(query.artist, "Tulus");
        assert_eq!(query.title, "Monokrom");

        let query = normalize_query(&Query {
            artist: "Sheila On 7 - Topic".into(),
            title: "Sheila On 7 - Dan... (Official Music Video)".into(),
        });
        assert_eq!(query.artist, "Sheila On 7");
        assert_eq!(query.title, "Dan...");
    }

    #[test]
    fn builds_deduped_queries() {
        let queries = build_queries("Tulus", "Monokrom");
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].artist, "Tulus");

        let queries = build_queries("Tulus Official", "Tulus - Monokrom (Official Music Video)");
        assert_eq!(queries.len(), 2);
    }
}
