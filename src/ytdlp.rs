use std::env;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::model::{SearchSource, Source, Track};

#[derive(Debug)]
pub struct Lookup {
    pub tracks: Vec<Track>,
    pub note: Option<String>,
}

pub fn ytdlp_bin() -> String {
    env::var("TUIMU_YTDLP").unwrap_or_else(|_| "yt-dlp".into())
}

pub fn curl_bin() -> String {
    env::var("TUIMU_CURL").unwrap_or_else(|_| "curl".into())
}

pub fn check_dependencies() -> Result<()> {
    let output = Command::new(ytdlp_bin())
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| {
            format!(
                "`{}` was not found. Install yt-dlp first (https://github.com/yt-dlp/yt-dlp).",
                ytdlp_bin()
            )
        })?;

    if !output.status.success() {
        bail!(
            "`{} --version` failed: {}",
            ytdlp_bin(),
            stderr_excerpt(&output.stderr)
        );
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

pub fn extract_stream(url: &str) -> Result<StreamInfo> {
    let source = Source::from_url(url);
    let android = source == Source::YouTube;

    match extract_stream_with(url, source, android) {
        Ok(info) => Ok(info),
        Err(error) if android => {
            // The Android client is far more reliable for direct stream URLs on
            // YouTube, but fall back to the default clients for videos it
            // cannot handle.
            extract_stream_with(url, source, false)
                .with_context(|| format!("Android client failed first: {error:#}"))
        }
        Err(error) => Err(error),
    }
}

fn extract_stream_with(url: &str, source: Source, android: bool) -> Result<StreamInfo> {
    let mut command = Command::new(ytdlp_bin());
    command.args([
        "--dump-single-json",
        "--skip-download",
        "--no-playlist",
        "--no-warnings",
        "--socket-timeout",
        "20",
    ]);
    match source {
        Source::YouTube => {
            if android {
                // The default youtube client frequently hands out signed URLs
                // that the CDN rejects with HTTP 403. The Android client's
                // URLs are much more consistent.
                command.args(["--extractor-args", "youtube:player_client=android"]);
            }
            command.args(["--add-header", "Referer:https://www.youtube.com/"]);
        }
        Source::SoundCloud => {
            command.args(["--add-header", "Referer:https://soundcloud.com/"]);
        }
        _ => {}
    }
    let output = command
        .args(["-f", "bestaudio/best", "--", url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to start {}", ytdlp_bin()))?;

    if !output.status.success() && output.stdout.is_empty() {
        bail!(
            "could not extract audio: {}",
            stderr_excerpt(&output.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&output.stdout)
        .context("yt-dlp returned invalid JSON while extracting audio")?;
    parse_stream_info(&value)
}

fn parse_stream_info(value: &Value) -> Result<StreamInfo> {
    let url = value
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
        .context("yt-dlp did not return a playable stream URL")?
        .to_string();

    let headers = value
        .get("http_headers")
        .and_then(Value::as_object)
        .or_else(|| {
            value
                .get("requested_downloads")
                .and_then(Value::as_array)
                .and_then(|downloads| downloads.first())
                .and_then(|download| download.get("http_headers"))
                .and_then(Value::as_object)
        })
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(StreamInfo { url, headers })
}

pub fn lookup(query: &str, source: SearchSource, limit: usize) -> Result<Lookup> {
    let query = query.trim();

    if is_spotify_url(query) {
        return resolve_spotify(query);
    }

    if let Some(url) = normalize_url(query) {
        let tracks = resolve_url(&url)?;
        return Ok(Lookup { tracks, note: None });
    }

    let tracks = search(query, source, limit)?;
    Ok(Lookup { tracks, note: None })
}

pub fn search(query: &str, source: SearchSource, limit: usize) -> Result<Vec<Track>> {
    let target = format!("{}{}:{}", source.search_prefix(), limit, query);
    let output = Command::new(ytdlp_bin())
        .args([
            "--flat-playlist",
            "--dump-json",
            "--no-warnings",
            "--ignore-errors",
            "--socket-timeout",
            "20",
            &target,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to start {}", ytdlp_bin()))?;

    let tracks = parse_json_lines(&output.stdout, Some(source));
    if !tracks.is_empty() {
        return Ok(tracks);
    }

    if !output.status.success() {
        bail!(
            "{} search failed: {}",
            source,
            stderr_excerpt(&output.stderr)
        );
    }
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        let stderr = stderr_excerpt(&output.stderr);
        if !stderr.is_empty() {
            bail!("{} search returned nothing: {}", source, stderr);
        }
    }
    Ok(Vec::new())
}

fn resolve_url(url: &str) -> Result<Vec<Track>> {
    let output = Command::new(ytdlp_bin())
        .args([
            "--dump-single-json",
            "--flat-playlist",
            "--skip-download",
            "--no-warnings",
            "--socket-timeout",
            "20",
            url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to start {}", ytdlp_bin()))?;

    if !output.status.success() && output.stdout.is_empty() {
        bail!("could not resolve URL: {}", stderr_excerpt(&output.stderr));
    }

    let value: Value = serde_json::from_slice(&output.stdout)
        .context("yt-dlp returned invalid JSON for this URL")?;

    if let Some(entries) = value.get("entries").and_then(Value::as_array) {
        let tracks: Vec<Track> = entries
            .iter()
            .filter_map(|entry| track_from_json(entry, None))
            .collect();
        if tracks.is_empty() {
            bail!("the playlist did not contain any playable entries");
        }
        return Ok(tracks);
    }

    let track = track_from_json(&value, None).context("yt-dlp could not find a playable track")?;
    Ok(vec![track])
}

fn resolve_spotify(url: &str) -> Result<Lookup> {
    if !url.to_ascii_lowercase().contains("/track/") {
        bail!(
            "Spotify albums and playlists cannot be read without an account. Paste an individual Spotify track link instead."
        );
    }

    let output = Command::new(curl_bin())
        .args([
            "-fsSL",
            "--max-time",
            "15",
            "-G",
            "--data-urlencode",
            &format!("url={url}"),
            "https://open.spotify.com/oembed",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| {
            format!(
                "Spotify matching needs `{}` (or set TUIMU_CURL). Install curl or paste a direct YouTube/SoundCloud URL.",
                curl_bin()
            )
        })?;

    if !output.status.success() {
        bail!(
            "Spotify metadata lookup failed: {}",
            stderr_excerpt(&output.stderr)
        );
    }

    let value: Value =
        serde_json::from_slice(&output.stdout).context("Spotify returned invalid metadata")?;
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .context("Spotify did not return a track title")?;

    // There is no unauthenticated Spotify playback API. Match the track to
    // freely available audio and be explicit about where it came from.
    let tracks = search(title, SearchSource::YouTube, 8)?;
    Ok(Lookup {
        tracks,
        note: Some(format!(
            "Spotify track \"{title}\" matched to YouTube audio (Spotify streams require login)."
        )),
    })
}

fn parse_json_lines(bytes: &[u8], fallback: Option<SearchSource>) -> Vec<Track> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let line = trim_ascii(line);
            if line.is_empty() {
                return None;
            }
            let value: Value = serde_json::from_slice(line).ok()?;
            track_from_json(&value, fallback)
        })
        .collect()
}

fn track_from_json(value: &Value, fallback: Option<SearchSource>) -> Option<Track> {
    let title = value
        .get("track")
        .and_then(Value::as_str)
        .or_else(|| value.get("title").and_then(Value::as_str))
        .map(str::trim)
        .filter(|title| !title.is_empty())?
        .to_string();

    let artist = value
        .get("artist")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("artists")
                .and_then(Value::as_array)
                .and_then(|artists| artists.first())
                .and_then(Value::as_str)
        })
        .or_else(|| value.get("uploader").and_then(Value::as_str))
        .or_else(|| value.get("channel").and_then(Value::as_str))
        .or_else(|| value.get("creator").and_then(Value::as_str))
        .map(str::trim)
        .filter(|artist| !artist.is_empty())
        .unwrap_or("Unknown artist")
        .to_string();

    let mut url = value
        .get("webpage_url")
        .and_then(Value::as_str)
        .or_else(|| value.get("original_url").and_then(Value::as_str))
        .or_else(|| value.get("url").and_then(Value::as_str))
        .map(str::to_string);

    if url
        .as_deref()
        .is_none_or(|url| !(url.starts_with("http://") || url.starts_with("https://")))
    {
        let id = value.get("id").and_then(Value::as_str)?;
        let ie_key = value.get("ie_key").and_then(Value::as_str).unwrap_or("");
        if ie_key == "Youtube" || matches!(fallback, Some(SearchSource::YouTube)) {
            url = Some(format!("https://www.youtube.com/watch?v={id}"));
        }
    }

    let url = url?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }

    let duration = value.get("duration").and_then(Value::as_f64);
    let mut source = Source::from_url(&url);
    if source == Source::Web {
        source = match fallback {
            Some(SearchSource::YouTube) => Source::YouTube,
            Some(SearchSource::SoundCloud) => Source::SoundCloud,
            None => Source::Web,
        };
    }

    let quality = Track::detect_quality(&title, source);
    
    Some(Track {
        title,
        artist,
        duration,
        url,
        source,
        quality,
    })
}

fn normalize_url(query: &str) -> Option<String> {
    if query.starts_with("http://") || query.starts_with("https://") {
        Some(query.to_string())
    } else if query.starts_with("www.") {
        Some(format!("https://{query}"))
    } else {
        None
    }
}

fn is_spotify_url(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    lower.starts_with("http") && lower.contains("spotify.com")
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn stderr_excerpt(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.is_empty() {
        String::new()
    } else if line.chars().count() > 240 {
        let mut short: String = line.chars().take(239).collect();
        short.push('…');
        short
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_search_entries() {
        let json = br#"{"_type":"url","ie_key":"Youtube","id":"abc","url":"https://www.youtube.com/watch?v=abc","title":"A song","uploader":"An artist","duration":123}"#;
        let tracks = parse_json_lines(json, Some(SearchSource::YouTube));
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "A song");
        assert_eq!(tracks[0].artist, "An artist");
        assert_eq!(tracks[0].duration, Some(123.0));
        assert_eq!(tracks[0].source, Source::YouTube);
    }

    #[test]
    fn builds_youtube_url_from_id() {
        let value = serde_json::json!({"id":"abc","ie_key":"Youtube","title":"A song"});
        let track = track_from_json(&value, None).unwrap();
        assert_eq!(track.url, "https://www.youtube.com/watch?v=abc");
    }

    #[test]
    fn parses_stream_info() {
        let value = serde_json::json!({
            "url": "https://example.com/audio.webm",
            "http_headers": {"User-Agent": "test agent", "Referer": "https://example.com/"}
        });
        let info = parse_stream_info(&value).unwrap();
        assert_eq!(info.url, "https://example.com/audio.webm");
        assert_eq!(info.headers.len(), 2);
    }

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_url("www.example.com"),
            Some("https://www.example.com".into())
        );
        assert_eq!(normalize_url("not a url"), None);
    }
}
