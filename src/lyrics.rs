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
    let cleaned = clean_query(artist, title);
    let original = Query {
        artist: artist.trim().to_string(),
        title: title.trim().to_string(),
    };

    let mut queries = vec![cleaned];
    if original.artist != queries[0].artist || original.title != queries[0].title {
        queries.push(original);
    }

    let mut last_error = None;

    for query in &queries {
        if query.title.is_empty() {
            continue;
        }

        let mut durations = Vec::new();
        if let Some(duration) = duration.filter(|value| value.is_finite() && *value > 60.0) {
            durations.push(Some(duration));
        }
        durations.push(None);

        for duration in durations {
            match retry(|| fetch_exact(query, duration)) {
                Ok(Some(lyrics)) => return Ok(lyrics.with_source("LrcLib")),
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }

            match retry(|| fetch_search(query, duration)) {
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

fn clean_query(artist: &str, title: &str) -> Query {
    Query {
        artist: clean(artist),
        title: clean(title),
    }
}

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => ' ',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
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
    if let Some(results) = json.as_array() {
        if let Some(best) = results
            .iter()
            .filter_map(|item| {
                lyrics_from_object_with_source(item, "LrcLib")
                    .map(|lyrics| (score(item, query, duration), lyrics))
            })
            .max_by(|left, right| left.0.total_cmp(&right.0))
        {
            return Ok(Some(best.1));
        }
    }

    Ok(None)
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
    if let Some(title) = value.get("trackName").and_then(Value::as_str) {
        if title.to_lowercase() == query.title.to_lowercase() {
            score += 100.0;
        }
    }

    // Exact match on artist gets high score
    if let Some(artist) = value.get("artistName").and_then(Value::as_str) {
        if artist.to_lowercase() == query.artist.to_lowercase() {
            score += 50.0;
        }
    }

    // Partial match on title gets medium score
    if let Some(title) = value.get("trackName").and_then(Value::as_str) {
        if title.to_lowercase().contains(&query.title.to_lowercase()) {
            score += 20.0;
        }
    }

    // Partial match on artist gets low score
    if let Some(artist) = value.get("artistName").and_then(Value::as_str) {
        if artist.to_lowercase().contains(&query.artist.to_lowercase()) {
            score += 10.0;
        }
    }

    // Duration match gets additional score
    if let Some(duration) = duration {
        if let Some(track_duration) = value.get("duration").and_then(Value::as_f64) {
            let diff = (track_duration - duration).abs();
            if diff < 5.0 {
                score += 5.0;
            } else if diff < 10.0 {
                score += 2.0;
            }
        }
    }

    score
}

#[derive(Clone, Debug)]
struct Query {
    artist: String,
    title: String,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            artist: String::new(),
            title: String::new(),
        }
    }
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
}
