use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ytdlp::curl_bin;
use crate::psalmnote::get_psalmnote_lyrics;

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
    pub is_indonesian: bool,
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
    
    // First, try Indonesian lyrics (PsalmNote) for Indonesian artists
    if is_indonesian_artist(artist) {
        for query in &queries {
            if query.title.is_empty() {
                continue;
            }
            
            if let Ok(Some(psalmnote_lyrics)) = get_psalmnote_lyrics(&crate::model::Track {
                title: query.title.clone(),
                artist: query.artist.clone(),
                duration: None,
                url: String::new(),
                source: crate::model::Source::YouTube, // Default source
                quality: crate::model::Quality::Medium, // Default quality
            }) {
                let synced_lyrics = parse_indonesian_lyrics(&psalmnote_lyrics.to_lyrics_format());
                return Ok(Lyrics {
                    plain: Some(psalmnote_lyrics.lyrics),
                    synced: synced_lyrics,
                    instrumental: false,
                    is_indonesian: true,
                    source: Some("PsalmNote".to_string()),
                });
            }
        }
    }
    
    // Then try international lyrics services
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
                    Ok(Some(lyrics)) => return Ok(lyrics.with_source("LrcLib")),
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

            // Try with retries for search
            for attempt in 0..MAX_RETRIES {
                match fetch_search(query, duration) {
                    Ok(Some(lyrics)) => return Ok(lyrics.with_source("LrcLib")),
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
                                        lyrics_from_object_with_source(item, "LrcLib")
                                            .map(|lyrics| (score(item, query, None), lyrics))
                                    })
                                    .max_by(|left, right| left.0.total_cmp(&right.0))
                                {
                                    return Ok(best.1);
                                }
                            }
                        }
                        Err(_) => {
                            // If still fails, return a user-friendly message
                            anyhow::bail!("Lyrics service is currently busy. Please try again later.");
                        }
                    }
                }
            }
            Err(error)
        },
        None => Ok(Lyrics::default()),
    }
}

fn is_indonesian_artist(artist: &str) -> bool {
    let artist_lower = artist.to_lowercase();
    
    // Check for common Indonesian artist indicators
    let keywords = [
        "indonesia", "indonesian", "nusantara", "dangdut", "k-pop", "kpop",
        "j-pop", "jpop", "lagu indonesia", "penyanyi indonesia",
        "grup indonesia", "band indonesia", "singer indonesia",
    ];
    
    for keyword in keywords {
        if artist_lower.contains(keyword) {
            return true;
        }
    }
    
    // Check for Indonesian names (simplified check)
    let indonesian_names = [
        "rafael", "siti", "nissa", "ziva", "judika", "ariel",
        "noah", "slank", "sheila", "peterpan", "dewa", "kangen",
        "ungu", "kotak", "radja", "gigi", "jamrud", "padi",
        "geisha", "coboy", "junior", "vierra", "wani", "rita",
        "risa", "chrisye", "iwan", "kris", "boni", "once",
    ];
    
    for name in indonesian_names {
        if artist_lower.contains(name) {
            return true;
        }
    }
    
    false
}

fn parse_indonesian_lyrics(lyrics_text: &str) -> Option<Vec<LyricLine>> {
    let mut lines = Vec::new();
    let offset = 0.0;
    
    for (index, line) in lyrics_text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("Reference:") || line.starts_with("---") {
            continue;
        }
        
        // Simple timestamp detection (e.g., [00:12.34])
        if let Some((time_str, text)) = extract_timestamp(line) {
            if let Some(time) = parse_timestamp(time_str) {
                lines.push(LyricLine {
                    time: time + offset,
                    text: text.to_string(),
                });
            } else {
                // No timestamp found, use sequential timing
                lines.push(LyricLine {
                    time: index as f64 * 3.0 + offset, // 3 seconds per line
                    text: line.to_string(),
                });
            }
        } else {
            // No timestamp found, use sequential timing
            lines.push(LyricLine {
                time: index as f64 * 3.0 + offset, // 3 seconds per line
                text: line.to_string(),
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

fn extract_timestamp(line: &str) -> Option<(&str, &str)> {
    if let Some(start) = line.find('[') {
        if let Some(end) = line[start..].find(']') {
            let timestamp = &line[start + 1..start + end];
            let text = &line[start + end + 1..].trim();
            return Some((timestamp, text));
        }
    }
    None
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

    let json = get_json_with_timeout(&format!("{}/get", API_ROOT), &params, 10)?;
    match lyrics_from_object_with_source(&json, "LrcLib") {
        Some(lyrics) => Ok(Some(lyrics)),
        None => Ok(None),
    }
}

fn fetch_search(query: &Query, duration: Option<f64>) -> Result<Option<Lyrics>> {
    let mut params = Vec::new();
    if !query.title.is_empty() {
        params.push(("track_name", query.title.clone()));
    }
    if !query.artist.is_empty() {
        params.push(("artist_name", query.artist.clone()));
    }

    let json = get_json_with_timeout(&format!("{}/search", API_ROOT), &params, 10)?;
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

fn get_json_with_timeout(url: &str, params: &[(&str, String)], timeout: u64) -> Result<Value> {
    // Build query parameters string
    let query_string = params
        .iter()
        .map(|(key, value)| format!("{}={}", key, urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    
    // Build full URL with query parameters
    let full_url = if query_string.is_empty() {
        url.to_string()
    } else {
        format!("{}?{}", url, query_string)
    };

    let mut cmd = Command::new(curl_bin());
    cmd.args(["-sS", "--max-time", &timeout.to_string(), "-A", USER_AGENT]);
    cmd.arg(&full_url);

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to start curl")?;

    if !output.status.success() {
        bail!("curl failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let response = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&response)
        .with_context(|| format!("invalid JSON response: {}", response))?;

    Ok(json)
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
        .map(str::to_string);

    let synced = value
        .get("syncedLyrics")
        .and_then(Value::as_str)
        .and_then(|text| parse_lrc(text));

    Some(Lyrics {
        plain,
        synced,
        instrumental: false,
        is_indonesian: false,
        source: None,
    })
}

fn parse_lrc(text: &str) -> Option<Vec<LyricLine>> {
    let mut lines = Vec::new();
    let offset = 0.0;

    for line in text.lines() {
        if let Some((time, text)) = parse_lrc_line(line) {
            lines.push(LyricLine {
                time: time + offset,
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
    let parts: Vec<&str> = time_str.split('.').collect();
    let minutes_seconds = parts[0].split(':').collect::<Vec<_>>();
    
    if minutes_seconds.len() != 2 {
        return None;
    }
    
    let minutes = minutes_seconds[0].parse::<u64>().ok()?;
    let seconds = minutes_seconds[1].parse::<u64>().ok()?;
    let milliseconds = if parts.len() > 1 {
        parts[1].parse::<u64>().ok()?
    } else {
        0
    };

    if minutes > 99 || seconds > 59 || milliseconds > 999 {
        return None;
    }

    // For LRC format, "0:12.34" means 12.34 seconds, not 0 minutes and 12.34 seconds
    if minutes == 0 && seconds < 60 {
        Some(seconds as f64 + milliseconds as f64 / 1000.0)
    } else {
        Some(minutes as f64 * 60.0 + seconds as f64 + milliseconds as f64 / 1000.0)
    }
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
    use crate::model::{format_duration, Source};

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(65.4), "1:05");
        assert_eq!(format_duration(3661.0), "1:01:01");
        assert_eq!(format_duration(f64::NAN), "--:--");
    }

    #[test]
    fn detects_sources() {
        assert_eq!(
            Source::from_url("https://www.youtube.com/watch?v=abc"),
            Source::YouTube
        );
        assert_eq!(
            Source::from_url("https://soundcloud.com/artist/track"),
            Source::SoundCloud
        );
        assert_eq!(Source::from_url("https://example.com/audio"), Source::Web);
    }

    #[test]
    fn detects_indonesian_artists() {
        assert!(is_indonesian_artist("Indonesian Band"));
        assert!(is_indonesian_artist("Lagu Indonesia"));
        assert!(is_indonesian_artist("Siti Nurhaliza"));
        assert!(!is_indonesian_artist("English Band"));
        assert!(!is_indonesian_artist("Artist Name"));
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_timestamp("0:12.34"), Some(12.034));
        assert_eq!(parse_timestamp("1:23.45"), Some(83.045));
        assert_eq!(parse_timestamp("01:23.45"), Some(83.045));
        assert_eq!(parse_timestamp("61:23.45"), Some(3683.045));
        assert_eq!(parse_timestamp("invalid"), None);
        assert_eq!(parse_timestamp("01:invalid"), None);
        assert_eq!(parse_timestamp("01:-23"), None);
    }

    #[test]
    fn parses_lrc_line() {
        assert_eq!(
            parse_lrc_line("[00:12.34]Hello world"),
            Some((12.034, "Hello world"))
        );
        assert_eq!(
            parse_lrc_line("[01:23.45]Test line"),
            Some((83.045, "Test line"))
        );
        assert_eq!(parse_lrc_line("No timestamp"), None);
        assert_eq!(parse_lrc_line("[invalid]"), None);
    }

    #[test]
    fn parses_lrc() {
        let lrc = "[00:12.34]First line\n[00:15.67]Second line\n[00:20.00]Third line";
        let lyrics = parse_lrc(lrc).unwrap();
        assert_eq!(lyrics.len(), 3);
        assert_eq!(lyrics[0].time, 12.034);
        assert_eq!(lyrics[0].text, "First line");
        assert_eq!(lyrics[1].time, 15.067);
        assert_eq!(lyrics[1].text, "Second line");
        assert_eq!(lyrics[2].time, 20.00);
        assert_eq!(lyrics[2].text, "Third line");
    }

    #[test]
    fn parses_indonesian_lyrics() {
        let lyrics_text = "Reference: John 3:16\n---\nAmazing grace, how sweet the sound\nThat saved a wretch like me";
        let lyrics = parse_indonesian_lyrics(lyrics_text).unwrap();
        assert_eq!(lyrics.len(), 2);
        assert_eq!(lyrics[0].text, "Amazing grace, how sweet the sound");
        assert_eq!(lyrics[1].text, "That saved a wretch like me");
    }
}