use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ytdlp::curl_bin;

const API_ROOT: &str = "https://lrclib.net/api";
const USER_AGENT: &str = "tuimu/0.1.0 (terminal music player)";

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
        let synced = self.synced.as_ref()?;
        let index = synced.partition_point(|line| line.time <= position + 0.15);
        index.checked_sub(1)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Query {
    artist: String,
    title: String,
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
            match fetch_exact(query, duration) {
                Ok(Some(lyrics)) => return Ok(lyrics),
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }
        }

        match fetch_search(query, duration) {
            Ok(Some(lyrics)) => return Ok(lyrics),
            Ok(None) => {}
            Err(error) => last_error = Some(error),
        }
    }

    match last_error {
        Some(error) => Err(error),
        None => Ok(Lyrics::default()),
    }
}

fn fetch_exact(query: &Query, duration: Option<f64>) -> Result<Option<Lyrics>> {
    let mut params = vec![
        ("artist_name", query.artist.clone()),
        ("track_name", query.title.clone()),
    ];
    if let Some(duration) = duration {
        params.push(("duration", format!("{duration:.0}")));
    }

    let value = get_json(&format!("{API_ROOT}/get"), &params)?;
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

    let value = get_json(&format!("{API_ROOT}/search"), &params)?;
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

fn get_json(endpoint: &str, params: &[(&str, String)]) -> Result<Value> {
    let bin = curl_bin();
    let mut command = Command::new(&bin);
    command.args([
        "-sS",
        "--max-time",
        "15",
        "-A",
        USER_AGENT,
        "-H",
        "Accept: application/json",
        "--get",
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
        bail!("lyrics request failed: {}", stderr_excerpt(&output.stderr));
    }
    serde_json::from_slice(&output.stdout).context("lyrics service returned invalid JSON")
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
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string);
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
    })
}

fn score(item: &Value, query: &Query, duration: Option<f64>) -> f64 {
    let mut score = 0.0;
    if let Some(track) = item.get("trackName").and_then(Value::as_str)
        && normalize(track) == normalize(&query.title)
    {
        score += 6.0;
    }
    if let Some(artist) = item.get("artistName").and_then(Value::as_str)
        && normalize(artist) == normalize(&query.artist)
    {
        score += 5.0;
    }
    if let (Some(expected), Some(actual)) = (
        duration,
        item.get("duration").and_then(Value::as_f64),
    ) {
        let difference = (expected - actual).abs();
        if difference <= 2.0 {
            score += 4.0;
        } else if difference <= 5.0 {
            score += 2.0;
        } else if difference > 30.0 {
            score -= 3.0;
        }
    }
    if item
        .get("syncedLyrics")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
    {
        score += 2.0;
    }
    score
}

fn service_error(value: &Value) -> Option<String> {
    value
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("statusCode")
                .and_then(Value::as_u64)
                .map(|code| format!("lyrics service returned HTTP {code}"))
        })
}

fn is_not_found(value: &Value) -> bool {
    value.get("statusCode").and_then(Value::as_u64) == Some(404)
        || value
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("TrackNotFound"))
}

fn clean_query(artist: &str, title: &str) -> Query {
    let mut artist = artist.trim();
    if artist.eq_ignore_ascii_case("unknown artist") {
        artist = "";
    }
    let mut artist = artist.to_string();
    if artist.to_ascii_lowercase().ends_with(" - topic") {
        artist.truncate(artist.len() - " - topic".len());
        artist = artist.trim().to_string();
    }

    let mut title = title.trim().to_string();
    if !artist.is_empty() {
        let prefix = format!("{} - ", artist.to_ascii_lowercase());
        if title.to_ascii_lowercase().starts_with(&prefix) {
            title = title[prefix.len()..].trim().to_string();
        }
    }

    if artist.is_empty()
        && let Some((left, right)) = title.split_once(" - ")
    {
        let left = left.trim();
        let right = right.trim();
        if !left.is_empty() && !right.is_empty() {
            artist = left.to_string();
            title = right.to_string();
        }
    }

    let cleaned = strip_noise(&title);
    if !cleaned.is_empty() {
        title = cleaned;
    }

    Query { artist, title }
}

fn strip_noise(title: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for character in title.chars() {
        match character {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(character),
            _ => {}
        }
    }

    let lowered = out.to_ascii_lowercase();
    for marker in [" feat. ", " ft. ", " featuring "] {
        if let Some(position) = lowered.rfind(marker) {
            out.truncate(position);
            break;
        }
    }

    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches(|character: char| {
            character == '-' || character == '|' || character == ':' || character.is_whitespace()
        })
        .to_string()
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
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
        assert_eq!(lines[0].text, "Hello");
        assert_eq!(lines[0].time, 1.6);
        assert_eq!(lines[1].time, 3.1);
        assert_eq!(lines[2].text, "World");
        assert_eq!(lines[2].time, 5.35);
    }

    #[test]
    fn finds_current_synced_line() {
        let lyrics = Lyrics {
            synced: Some(vec![
                LyricLine {
                    time: 1.0,
                    text: "one".into(),
                },
                LyricLine {
                    time: 3.0,
                    text: "two".into(),
                },
            ]),
            ..Lyrics::default()
        };
        assert_eq!(lyrics.current_line(0.5), None);
        assert_eq!(lyrics.current_line(1.0), Some(0));
        assert_eq!(lyrics.current_line(4.0), Some(1));
    }

    #[test]
    fn cleans_noisy_youtube_titles() {
        assert_eq!(
            clean_query(
                "Daft Punk",
                "Daft Punk - Instant Crush (Official Video) ft. Julian Casablancas"
            ),
            Query {
                artist: "Daft Punk".into(),
                title: "Instant Crush".into(),
            }
        );
        assert_eq!(
            clean_query("Unknown artist", "Daft Punk - Get Lucky (Official Audio)"),
            Query {
                artist: "Daft Punk".into(),
                title: "Get Lucky".into(),
            }
        );
    }

    #[test]
    fn strips_channel_topic_suffix() {
        assert_eq!(
            clean_query("Daft Punk - Topic", "Get Lucky"),
            Query {
                artist: "Daft Punk".into(),
                title: "Get Lucky".into(),
            }
        );
    }
}
