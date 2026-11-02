use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ytdlp::curl_bin;

const API_ROOT: &str = "https://lrclib.net/api";
const USER_AGENT: &str = "tuimu/0.1.0 (terminal music player)";
const MAX_RETRIES: usize = 3;
const RETRY_DELAY: u64 = 2; // seconds
const FALLBACK_TIMEOUT: u64 = 5; // seconds before trying fallback

#[derive(Clone, Debug, Default)]
pub struct Lyrics {
    pub plain: Option<String>,
    pub synced: Option<Vec<LyricLine>>,
    pub instrumental: bool,
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

    pub fn current_line(&self, position: f64) -> Option<usize> {
        if let Some(synced) = &self.synced {
            synced
                .iter()
                .position(|line| line.time >= position)
                .map(|index| index.saturating_sub(1))
                .filter(|&index| index < synced.len())
        } else {
            None
        }
    }
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

fn clean_query(artist: &str, title: &str) -> Query {
    fn clean(value: &str) -> String {
        value
            .trim()
            .chars()
            .map(|c| match c {
                '<' | '>' | ':' | '"' | '\'' | '\\' | '/' | '|' | '?' | '*' => ' ',
                _ => c,
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    Query {
        artist: clean(artist),
        title: clean(title),
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
            // Try with retries
            for attempt in 0..MAX_RETRIES {
                match fetch_exact(query, duration) {
                    Ok(Some(lyrics)) => return Ok(lyrics),
                    Ok(None) => break,
                    Err(error) => {
                        if attempt == MAX_RETRIES - 1 {
                            last_error = Some(error);
                        } else {
                            // Wait before retrying
                            std::thread::sleep(std::time::Duration::from_secs(RETRY_DELAY));
                        }
                    }
                }
            }
        }

        // Try with retries for search
        for attempt in 0..MAX_RETRIES {
            match fetch_search(query, duration) {
                Ok(Some(lyrics)) => return Ok(lyrics),
                Ok(None) => break,
                Err(error) => {
                    if attempt == MAX_RETRIES - 1 {
                        last_error = Some(error);
                    } else {
                        // Wait before retrying
                        std::thread::sleep(std::time::Duration::from_secs(RETRY_DELAY));
                    }
                }
            }
        }
    }

    match last_error {
        Some(error) => {
            // If we get a "server busy" error, try with a longer timeout
            let error_msg = error.to_string();
            if error_msg.contains("server is busy") || error_msg.contains("503") {
                // Try one more time with extended timeout
                std::thread::sleep(std::time::Duration::from_secs(FALLBACK_TIMEOUT));
                
                for query in &queries {
                    if query.title.is_empty() {
                        continue;
                    }
                    
                    let mut params = Vec::new();
                    if !query.title.is_empty() {
                        params.push(("track_name", query.title.clone()));
                    }
                    if !query.artist.is_empty() {
                        params.push(("artist_name", query.artist.clone()));
                    }

                    match get_json_with_timeout(&format!("{}/search", API_ROOT), &params, 20) {
                        Ok(json) => {
                            if let Some(results) = json.as_array() {
                                if let Some(best) = results
                                    .iter()
                                    .filter_map(|item| {
                                        lyrics_from_object(item).map(|lyrics| (score(item, query, None), lyrics))
                                    })
                                    .max_by(|left, right| left.0.total_cmp(&right.0))
                                {
                                    return Ok(best.1);
                                }
                            }
                        }
                        Err(_) => {
                            // If still fails, return a user-friendly message
                            anyhow::bail!("Lyrics service is currently busy. Please try again later.")
                        }
                    }
                }
            }
            Err(error)
        },
        None => Ok(Lyrics::default()),
    }
}

fn get_json_with_timeout(endpoint: &str, params: &[(&str, String)], timeout: u64) -> Result<Value> {
    let bin = curl_bin();
    let mut command = Command::new(&bin);
    command.args([
        "-sS",
        "--max-time",
        &timeout.to_string(),
        "-A",
        USER_AGENT,
        "-H",
        "Accept: application/json",
        "--get",
        "--connect-timeout",
        "5",
    ]);
    for (key, value) in params {
        command.arg("--data-urlencode").arg(format!("{key}={value}"));
    }
    let output = command
        .arg(endpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to start `{bin}` for lyrics"))?;

    if !output.status.success() {
        let error_msg = stderr_excerpt(&output.stderr);
        if error_msg.contains("503") || error_msg.contains("server is busy") {
            bail!("Lyrics service is temporarily busy. Please try again later.");
        } else if error_msg.contains("timeout") {
            bail!("Lyrics request timed out. Please try again.");
        } else {
            bail!("lyrics request failed: {}", error_msg);
        }
    }
    
    let json = serde_json::from_slice(&output.stdout)
        .context("lyrics service returned invalid JSON")?;
    
    // Check for service errors in the response
    if let Some(error) = service_error(&json) {
        if error.contains("503") || error.contains("server is busy") {
            bail!("Lyrics service is temporarily busy. Please try again later.");
        } else {
            bail!("{}", error);
        }
    }
    
    Ok(json)
}



fn fetch_exact(query: &Query, duration: Option<f64>) -> Result<Option<Lyrics>> {
    let mut params = vec![
        ("artist_name", query.artist.clone()),
        ("track_name", query.title.clone()),
    ];
    if let Some(duration) = duration {
        params.push(("duration", format!("{duration:.0}")));
    }

    let value = get_json_with_timeout(&format!("{API_ROOT}/get"), &params, 10)?;
    if let Some(error) = service_error(&value) {
        if is_not_found(&value) {
            return Ok(None);
        }
        bail!("{error}");
    }
    Ok(lyrics_from_object(&value))
}

fn fetch_search(query: &Query, duration: Option<f64>) -> Result<Option<Lyrics>> {
    let mut params = Vec::new();
    if !query.title.is_empty() {
        params.push(("track_name", query.title.clone()));
    }
    if !query.artist.is_empty() {
        params.push(("artist_name", query.artist.clone()));
    }

    let value = get_json_with_timeout(&format!("{API_ROOT}/search"), &params, 10)?;
    if let Some(error) = service_error(&value) {
        bail!("{error}");
    }

    let Some(results) = value.as_array() else {
        return Ok(None);
    };
    let best = results
        .iter()
        .filter_map(|item| {
            lyrics_from_object(item).map(|lyrics| (score(item, query, duration), lyrics))
        })
        .max_by(|left, right| left.0.total_cmp(&right.0));
    Ok(best.map(|(_, lyrics)| lyrics))
}

fn score(value: &Value, query: &Query, duration: Option<f64>) -> f64 {
    let mut score = 0.0;

    // Prefer exact duration matches
    if let Some(duration) = duration {
        if let Some(track_duration) = value
            .get("duration")
            .and_then(Value::as_f64)
        {
            let diff = (duration - track_duration).abs();
            if diff < 5.0 {
                score += 1.0 - (diff / 5.0);
            }
        }
    }

    // Prefer instrumental matches
    if value.get("instrumental").and_then(Value::as_bool) == Some(true) {
        if query.title.to_lowercase().contains("instrumental") {
            score += 1.0;
        } else {
            score -= 1.0;
        }
    }

    // Prefer synced lyrics
    if value.get("synced").and_then(Value::as_bool) == Some(true) {
        score += 1.0;
    }

    // Prefer exact title matches
    if let Some(title) = value.get("track_name").and_then(Value::as_str) {
        if query.title.eq_ignore_ascii_case(title) {
            score += 1.0;
        }
    }

    // Prefer exact artist matches
    if let Some(artist) = value.get("artist_name").and_then(Value::as_str) {
        if query.artist.eq_ignore_ascii_case(artist) {
            score += 1.0;
        }
    }

    score
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
        .map(str::to_string);

    let synced = value
        .get("syncedLyrics")
        .and_then(Value::as_str)
        .and_then(|text| parse_lrc(text));

    Some(Lyrics {
        plain,
        synced,
        instrumental: false,
    })
}

fn service_error(value: &Value) -> Option<String> {
    value
        .get("message")
        .and_then(Value::as_str)
        .map(|msg| {
            // Clean up common error messages
            let msg = msg.trim();
            if msg.contains("server is busy") || msg.contains("503") {
                "Lyrics service is temporarily busy. Please try again later.".to_string()
            } else {
                msg.to_string()
            }
        })
        .or_else(|| {
            value
                .get("statusCode")
                .and_then(Value::as_u64)
                .map(|code| match code {
                    503 => "Lyrics service is temporarily busy. Please try again later.".to_string(),
                    429 => "Too many requests. Please wait before trying again.".to_string(),
                    404 => "Lyrics not found for this track.".to_string(),
                    _ => format!("Lyrics service returned HTTP {}", code),
                })
        })
}

fn is_not_found(value: &Value) -> bool {
    value
        .get("statusCode")
        .and_then(Value::as_u64)
        .map(|code| code == 404)
        .unwrap_or(false)
}

fn stderr_excerpt(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        "unknown error".into()
    } else {
        text.chars().take(200).collect()
    }
}

pub fn parse_lrc(input: &str) -> Option<Vec<LyricLine>> {
    let mut offset = 0.0;
    let mut lines = Vec::new();

    for raw in input.lines() {
        let mut rest = raw.trim();
        if rest.is_empty() {
            continue;
        }

        if let Some(after_open) = rest.strip_prefix('[')
            && let Some((tag, after)) = after_open.split_once(']')
            && !tag.as_bytes().first().is_some_and(u8::is_ascii_digit)
        {
            if let Some((name, value)) = tag.split_once(':')
                && name.eq_ignore_ascii_case("offset")
                && let Ok(parsed) = value.trim().parse::<f64>()
            {
                offset = parsed / 1000.0;
            }
            rest = after.trim_start();
        }

        let mut times = Vec::new();
        let mut remaining = rest;
        while let Some(after_open) = remaining.strip_prefix('[') {
            let Some((tag, after)) = after_open.split_once(']') else {
                break;
            };
            let Some(time) = parse_timestamp(tag) else {
                break;
            };
            times.push(time);
            remaining = after.trim_start();
        }

        if times.is_empty() {
            continue;
        }
        let text = strip_word_timestamps(remaining).trim().to_string();
        for time in times {
            lines.push(LyricLine {
                time: time + offset,
                text: text.clone(),
            });
        }
    }

    if lines.is_empty() {
        return None;
    }
    lines.sort_by(|left, right| left.time.total_cmp(&right.time));
    Some(lines)
}

fn parse_timestamp(value: &str) -> Option<f64> {
    let (minutes, seconds) = value.split_once(':')?;
    let minutes: f64 = minutes.trim().parse().ok()?;
    let seconds: f64 = seconds.trim().replace(':', ".").parse().ok()?;
    if !minutes.is_finite() || !seconds.is_finite() || minutes < 0.0 || seconds < 0.0 {
        return None;
    }
    Some(minutes * 60.0 + seconds)
}

fn strip_word_timestamps(value: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for character in value.chars() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(character),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_synced_lyrics_and_offset() {
        let lines = parse_lrc(
            "[ar:Daft Punk]\n[offset:100]\n[00:01.50][00:03.00]Hello\n[00:05.25]World\n",
        )
        .unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].time, 1.6);
        assert_eq!(lines[0].text, "Hello");
        assert_eq!(lines[1].time, 3.1);
        assert_eq!(lines[1].text, "Hello");
        assert_eq!(lines[2].time, 5.35);
        assert_eq!(lines[2].text, "World");
    }

    #[test]
    fn parses_synced_lyrics_with_word_timestamps() {
        let lines = parse_lrc(
            "[00:01.50]He<00:02.00>l<00:02.50>lo\n[00:05.25]Wo<00:06.00>rld\n",
        )
        .unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].time, 1.5);
        assert_eq!(lines[0].text, "Hello");
        assert_eq!(lines[1].time, 5.25);
        assert_eq!(lines[1].text, "World");
    }

    #[test]
    fn ignores_unsynced_lyrics() {
        assert!(parse_lrc("This is not a synced lyric").is_none());
    }

    #[test]
    fn ignores_metadata() {
        assert!(parse_lrc("[ar:Artist]\n[ti:Title]\n[al:Album]\n[offset:100]").is_none());
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_timestamp("01:23"), Some(83.0));
        assert_eq!(parse_timestamp("1:23.45"), Some(83.45));
        assert_eq!(parse_timestamp("01:23.45"), Some(83.45));
        assert_eq!(parse_timestamp("61:23.45"), Some(3683.45));
        assert_eq!(parse_timestamp("invalid"), None);
        assert_eq!(parse_timestamp("01:invalid"), None);
        assert_eq!(parse_timestamp("01:-23"), None);
    }
}