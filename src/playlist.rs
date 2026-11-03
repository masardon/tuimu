//! Named playlists persisted as one JSON file per playlist under
//! `$XDG_DATA_HOME/tuimu/playlists/` (falling back to `~/.local/share`).
//!
//! Tracks are stored with their source page URL; audio is re-extracted with
//! yt-dlp at play time, so saved playlists stay valid across sessions.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::model::{Quality, Source, Track};

const NAME_MAX_CHARS: usize = 60;

pub struct Playlist {
    pub tracks: Vec<Track>,
}

/// Resolve the playlists directory for the current user, creating it if
/// missing.
pub fn playlists_dir() -> Result<PathBuf> {
    let dir = data_root().join("tuimu").join("playlists");
    fs::create_dir_all(&dir).context("could not create the playlists directory")?;
    Ok(dir)
}

fn data_root() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg);
    }
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(".local").join("share"),
        _ => PathBuf::from("."),
    }
}

/// Reduce a user-typed name to a safe file name fragment. Anything outside
/// letters, digits, spaces, `-`, and `_` is replaced; the result is trimmed
/// and capped, so path traversal and empty names collapse harmlessly.
pub fn sanitize_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_') {
                c
            } else {
                ' '
            }
        })
        .collect();
    let sanitized = sanitized.trim();
    sanitized.chars().take(NAME_MAX_CHARS).collect()
}

/// List saved playlist names, sorted alphabetically.
pub fn list(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| entry.path().file_stem()?.to_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

/// Load a playlist by name. `Ok(None)` when it does not exist.
pub fn load(dir: &std::path::Path, name: &str) -> Result<Option<Playlist>> {
    load_at(dir, &sanitize_name(name))
}

fn load_at(dir: &std::path::Path, sanitized: &str) -> Result<Option<Playlist>> {
    if sanitized.is_empty() {
        return Ok(None);
    }
    let path = dir.join(format!("{sanitized}.json"));
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("could not read the playlist file"),
    };

    let json: Value = serde_json::from_str(&text).context("playlist file is not valid JSON")?;
    let tracks = json
        .get("tracks")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(track_from_json).collect())
        .unwrap_or_default();

    Ok(Some(Playlist { tracks }))
}

/// Persist a playlist atomically (temp file + rename).
pub fn save(dir: &std::path::Path, name: &str, tracks: &[Track]) -> Result<()> {
    save_at(dir, &sanitize_name(name), tracks)
}

fn save_at(dir: &std::path::Path, sanitized: &str, tracks: &[Track]) -> Result<()> {
    if sanitized.is_empty() {
        bail!("playlist name is empty");
    }
    fs::create_dir_all(dir).context("could not create the playlists directory")?;
    let path = dir.join(format!("{sanitized}.json"));

    let doc = json!({
        "name": sanitized,
        "version": 1,
        "tracks": tracks.iter().map(track_to_json).collect::<Vec<_>>(),
    });
    let body = serde_json::to_string_pretty(&doc).context("could not encode the playlist")?;

    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, body).context("could not write the playlist file")?;
    fs::rename(&tmp, &path).context("could not finalize the playlist file")?;
    Ok(())
}

/// Delete a playlist. Returns `false` when it did not exist.
pub fn delete(dir: &std::path::Path, name: &str) -> Result<bool> {
    delete_at(dir, &sanitize_name(name))
}

fn delete_at(dir: &std::path::Path, sanitized: &str) -> Result<bool> {
    let path = dir.join(format!("{sanitized}.json"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("could not delete the playlist file"),
    }
}

/// Rename a playlist, preserving its tracks. Returns `false` when the old
/// name does not exist or both names sanitize to the same file.
pub fn rename(dir: &std::path::Path, old: &str, new: &str) -> Result<bool> {
    let old = sanitize_name(old);
    let new = sanitize_name(new);
    if old.is_empty() || new.is_empty() || old == new {
        return Ok(false);
    }
    let Some(playlist) = load_at(dir, &old)? else {
        return Ok(false);
    };
    save_at(dir, &new, &playlist.tracks)?;
    delete_at(dir, &old)?;
    Ok(true)
}

/// Append a track to an existing playlist. Returns `false` when the playlist
/// does not exist.
pub fn add_track(dir: &std::path::Path, name: &str, track: &Track) -> Result<bool> {
    let sanitized = sanitize_name(name);
    let Some(mut playlist) = load_at(dir, &sanitized)? else {
        return Ok(false);
    };
    playlist.tracks.push(track.clone());
    save_at(dir, &sanitized, &playlist.tracks)?;
    Ok(true)
}

/// Remove the track at `index` from a playlist. Returns `false` when the
/// playlist or the index does not exist.
pub fn remove_track(dir: &std::path::Path, name: &str, index: usize) -> Result<bool> {
    let sanitized = sanitize_name(name);
    let Some(mut playlist) = load_at(dir, &sanitized)? else {
        return Ok(false);
    };
    if index >= playlist.tracks.len() {
        return Ok(false);
    }
    playlist.tracks.remove(index);
    save_at(dir, &sanitized, &playlist.tracks)?;
    Ok(true)
}

fn track_to_json(track: &Track) -> Value {
    json!({
        "title": track.title,
        "artist": track.artist,
        "url": track.url,
        "source": source_slug(track.source),
        "quality": quality_slug(track.quality),
        "duration": track.duration,
    })
}

fn track_from_json(value: &Value) -> Option<Track> {
    let url = value
        .get("url")
        .and_then(Value::as_str)
        .and_then(non_empty)?
        .to_string();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .and_then(non_empty)
        .unwrap_or("Unknown title")
        .to_string();
    let artist = value
        .get("artist")
        .and_then(Value::as_str)
        .and_then(non_empty)
        .unwrap_or("Unknown artist")
        .to_string();

    Some(Track {
        title,
        artist,
        duration: value.get("duration").and_then(Value::as_f64),
        url,
        source: value
            .get("source")
            .and_then(Value::as_str)
            .and_then(source_from_slug)
            .unwrap_or(Source::Web),
        quality: value
            .get("quality")
            .and_then(Value::as_str)
            .and_then(quality_from_slug)
            .unwrap_or(Quality::Medium),
    })
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn source_slug(source: Source) -> &'static str {
    match source {
        Source::YouTube => "youtube",
        Source::SoundCloud => "soundcloud",
        Source::Bandcamp => "bandcamp",
        Source::Spotify => "spotify",
        Source::Web => "web",
    }
}

fn source_from_slug(slug: &str) -> Option<Source> {
    match slug {
        "youtube" => Some(Source::YouTube),
        "soundcloud" => Some(Source::SoundCloud),
        "bandcamp" => Some(Source::Bandcamp),
        "spotify" => Some(Source::Spotify),
        "web" => Some(Source::Web),
        _ => None,
    }
}

fn quality_slug(quality: Quality) -> &'static str {
    match quality {
        Quality::Low => "low",
        Quality::Medium => "medium",
        Quality::High => "high",
        Quality::Lossless => "lossless",
    }
}

fn quality_from_slug(slug: &str) -> Option<Quality> {
    match slug {
        "low" => Some(Quality::Low),
        "medium" => Some(Quality::Medium),
        "high" => Some(Quality::High),
        "lossless" => Some(Quality::Lossless),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tuimu-playlists-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn track(url: &str) -> Track {
        Track {
            title: "Song".into(),
            artist: "Artist".into(),
            duration: Some(180.0),
            url: url.into(),
            source: Source::YouTube,
            quality: Quality::High,
        }
    }

    #[test]
    fn saves_and_loads_playlists() {
        let dir = test_dir("roundtrip");
        save(&dir, "favorites", &[track("https://a/1")]).expect("save");
        let playlist = load(&dir, "favorites").expect("load").expect("present");
        assert_eq!(playlist.tracks.len(), 1);
        assert_eq!(playlist.tracks[0].url, "https://a/1");
        assert_eq!(playlist.tracks[0].source, Source::YouTube);
        assert_eq!(playlist.tracks[0].quality, Quality::High);
    }

    #[test]
    fn missing_playlist_loads_none() {
        let dir = test_dir("missing");
        assert!(load(&dir, "nope").expect("load").is_none());
    }

    #[test]
    fn lists_sorted_json_files_only() {
        let dir = test_dir("list");
        save(&dir, "b-mix", &[track("https://a/1")]).expect("save");
        save(&dir, "a-mix", &[track("https://a/2")]).expect("save");
        fs::write(dir.join("notes.txt"), "junk").expect("write");
        assert_eq!(list(&dir), vec!["a-mix", "b-mix"]);
    }

    #[test]
    fn deletes_and_reports_missing() {
        let dir = test_dir("delete");
        save(&dir, "temp", &[track("https://a/1")]).expect("save");
        assert!(delete(&dir, "temp").expect("delete"));
        assert!(!delete(&dir, "temp").expect("delete again"));
    }

    #[test]
    fn renames_preserve_tracks() {
        let dir = test_dir("rename");
        save(&dir, "old", &[track("https://a/1")]).expect("save");
        let renamed = rename(&dir, "old", "new").expect("rename");
        assert!(renamed);
        assert!(load(&dir, "old").expect("load").is_none());
        assert_eq!(
            load(&dir, "new")
                .expect("load")
                .expect("present")
                .tracks
                .len(),
            1
        );
    }

    #[test]
    fn adds_and_removes_tracks() {
        let dir = test_dir("mutate");
        save(&dir, "mix", &[track("https://a/1")]).expect("save");

        let mut playlist = load(&dir, "mix").expect("load").expect("present");
        playlist.tracks.push(track("https://a/2"));
        save(&dir, "mix", &playlist.tracks).expect("save");
        assert_eq!(
            load(&dir, "mix")
                .expect("load")
                .expect("present")
                .tracks
                .len(),
            2
        );

        playlist.tracks.remove(0);
        save(&dir, "mix", &playlist.tracks).expect("save");
        let playlist = load(&dir, "mix").expect("load").expect("present");
        assert_eq!(playlist.tracks[0].url, "https://a/2");
    }

    #[test]
    fn malformed_tracks_are_skipped() {
        let dir = test_dir("tolerant");
        let doc = json!({
            "name": "damaged",
            "tracks": [
                {"title": "ok", "artist": "a", "url": "https://a/1"},
                {"title": "no url", "artist": "a"},
                "not even an object",
                {"title": "unknown source", "artist": "a", "url": "https://a/2", "source": "napster", "quality": 7}
            ]
        });
        fs::write(
            dir.join("damaged.json"),
            serde_json::to_string(&doc).unwrap(),
        )
        .expect("write");

        let playlist = load(&dir, "damaged").expect("load").expect("present");
        assert_eq!(playlist.tracks.len(), 2);
        assert_eq!(playlist.tracks[1].source, Source::Web);
        assert_eq!(playlist.tracks[1].quality, Quality::Medium);
    }

    #[test]
    fn sanitizes_names_safely() {
        assert_eq!(sanitize_name("  Road Trip 24/7!! "), "Road Trip 24 7");
        assert_eq!(sanitize_name("../../etc/passwd"), "etc passwd");
        assert_eq!(sanitize_name("///"), "");
        let long = "x".repeat(200);
        assert_eq!(sanitize_name(&long).len(), NAME_MAX_CHARS);
    }
}
