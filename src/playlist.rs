//! Named playlists persisted as one JSON file per playlist under
//! `$XDG_DATA_HOME/tuimu/playlists/` (falling back to `~/.local/share`).
//!
//! Tracks are stored with their source page URL; audio is re-extracted with
//! yt-dlp at play time, so saved playlists stay valid across sessions.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::model::{Quality, Source, Track};

const NAME_MAX_CHARS: usize = 60;

pub struct Playlist {
    pub tracks: Vec<Track>,
    /// Incremented on every save; the sync engine uses it for 3-way merges.
    pub revision: u64,
    /// Unix epoch seconds of the last save (0 for version 1 files).
    /// Read by the sync engine for conflict ordering.
    #[allow(dead_code)]
    pub updated_at: u64,
    /// Device name that wrote the file last.
    pub device: String,
}

/// A human-friendly default device name, used until config exists.
pub fn default_device_name() -> String {
    if env::var_os("TERMUX_VERSION").is_some() {
        return "phone".into();
    }
    env::var("USER")
        .ok()
        .filter(|user| !user.is_empty() && user != "root")
        .unwrap_or_else(|| "desktop".into())
}

/// Single owner of every playlist mutation.
///
/// The app routes all playlist operations through this store so sync hooks
/// (device name stamping, revision bumps, change detection) live in one
/// place. It also detects edits made by other processes — a sync client
/// such as Syncthing, or another tuimu on the same machine — by comparing
/// file metadata between scans.
pub struct PlaylistStore {
    dir: PathBuf,
    device: String,
    /// Last-seen `(mtime, size)` per playlist file; `None` until primed.
    baseline: Option<HashMap<String, (SystemTime, u64)>>,
}

impl PlaylistStore {
    pub fn open(dir: PathBuf, device: String) -> Self {
        Self {
            dir,
            device,
            baseline: None,
        }
    }

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Read by the sync engine when it pairs this device.
    #[allow(dead_code)]
    pub fn device(&self) -> &str {
        &self.device
    }

    pub fn save(&mut self, name: &str, tracks: &[Track]) -> Result<()> {
        let result = save_with_device(&self.dir, name, tracks, &self.device);
        self.refresh();
        result
    }

    pub fn load(&self, name: &str) -> Result<Option<Playlist>> {
        load(&self.dir, name)
    }

    pub fn list(&self) -> Vec<String> {
        list(&self.dir)
    }

    pub fn rename(&mut self, old: &str, new: &str) -> Result<bool> {
        let result = rename(&self.dir, old, new);
        self.refresh();
        result
    }

    pub fn delete(&mut self, name: &str) -> Result<bool> {
        let result = delete(&self.dir, name);
        self.refresh();
        result
    }

    pub fn add_track(&mut self, name: &str, track: &Track) -> Result<bool> {
        let result = add_track_with_device(&self.dir, name, track, &self.device);
        self.refresh();
        result
    }

    pub fn remove_track(&mut self, name: &str, index: usize) -> Result<bool> {
        let result = remove_track_with_device(&self.dir, name, index, &self.device);
        self.refresh();
        result
    }

    fn refresh(&mut self) {
        self.baseline = Some(scan_metadata(&self.dir));
    }

    /// True when playlist files changed since the last call without going
    /// through this store (prime call returns `false`).
    pub fn external_changes(&mut self) -> bool {
        let scan = scan_metadata(&self.dir);
        let changed = match &self.baseline {
            Some(baseline) => &scan != baseline,
            None => false,
        };
        self.baseline = Some(scan);
        changed
    }
}

fn scan_metadata(dir: &Path) -> HashMap<String, (SystemTime, u64)> {
    let mut map = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return map;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(meta) = entry.metadata() {
            map.insert(
                name.to_string(),
                (
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    meta.len(),
                ),
            );
        }
    }
    map
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

    Ok(Some(Playlist {
        tracks,
        // Version 1 files carry no metadata; they upgrade on next save.
        revision: json.get("revision").and_then(Value::as_u64).unwrap_or(0),
        updated_at: json.get("updated_at").and_then(Value::as_u64).unwrap_or(0),
        device: json
            .get("device")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }))
}

/// Persist a playlist atomically (temp file + rename), stamped with this
/// device's name. Test-only convenience; the app uses [`PlaylistStore`].
#[cfg(test)]
pub fn save(dir: &std::path::Path, name: &str, tracks: &[Track]) -> Result<()> {
    save_with_device(dir, name, tracks, &default_device_name())
}

pub fn save_with_device(
    dir: &std::path::Path,
    name: &str,
    tracks: &[Track],
    device: &str,
) -> Result<()> {
    save_at(dir, &sanitize_name(name), tracks, device)
}

fn save_at(dir: &std::path::Path, sanitized: &str, tracks: &[Track], device: &str) -> Result<()> {
    if sanitized.is_empty() {
        bail!("playlist name is empty");
    }
    fs::create_dir_all(dir).context("could not create the playlists directory")?;
    let path = dir.join(format!("{sanitized}.json"));

    // Bump the revision when the file already exists so concurrent
    // devices can tell generations apart.
    let revision = load_at(dir, sanitized)?
        .map(|playlist| playlist.revision + 1)
        .unwrap_or(1);

    let doc = json!({
        "name": sanitized,
        "version": 2,
        "revision": revision,
        "updated_at": crate::model::now_epoch(),
        "device": device,
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
    save_at(dir, &new, &playlist.tracks, &playlist.device)?;
    delete_at(dir, &old)?;
    Ok(true)
}

/// Append a track to an existing playlist. Returns `false` when the
/// playlist does not exist.
pub fn add_track_with_device(
    dir: &std::path::Path,
    name: &str,
    track: &Track,
    device: &str,
) -> Result<bool> {
    let sanitized = sanitize_name(name);
    let Some(mut playlist) = load_at(dir, &sanitized)? else {
        return Ok(false);
    };
    playlist.tracks.push(track.clone());
    save_at(dir, &sanitized, &playlist.tracks, device)?;
    Ok(true)
}

/// Remove the track at `index` from a playlist. Returns `false` when the
/// playlist or the index does not exist.
pub fn remove_track_with_device(
    dir: &std::path::Path,
    name: &str,
    index: usize,
    device: &str,
) -> Result<bool> {
    let sanitized = sanitize_name(name);
    let Some(mut playlist) = load_at(dir, &sanitized)? else {
        return Ok(false);
    };
    if index >= playlist.tracks.len() {
        return Ok(false);
    }
    playlist.tracks.remove(index);
    save_at(dir, &sanitized, &playlist.tracks, device)?;
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
        "id": track.id,
        "added_at": track.added_at,
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
        url: url.clone(),
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
        // v1 files have no id/added_at; synthesize them so merges can
        // dedupe on load.
        id: value
            .get("id")
            .and_then(Value::as_str)
            .and_then(non_empty)
            .map(str::to_string)
            .unwrap_or_else(|| crate::model::id_for_url(&url)),
        added_at: value.get("added_at").and_then(Value::as_u64).unwrap_or(0),
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
        Track::new(
            "Song".into(),
            "Artist".into(),
            Some(180.0),
            url.into(),
            Source::YouTube,
            Quality::High,
        )
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
        assert_eq!(
            playlist.tracks[0].id,
            crate::model::id_for_url("https://a/1")
        );
        assert!(playlist.tracks[0].added_at > 0);
    }

    #[test]
    fn version_two_metadata_roundtrips_and_revisions_bump() {
        let dir = test_dir("v2-meta");
        save_with_device(&dir, "mix", &[track("https://a/1")], "pixel").expect("save");
        let first = load(&dir, "mix").expect("load").expect("present");
        assert_eq!(first.revision, 1);
        assert_eq!(first.device, "pixel");
        assert!(first.updated_at > 0);

        save_with_device(&dir, "mix", &[track("https://a/1")], "pc").expect("save");
        let second = load(&dir, "mix").expect("load").expect("present");
        assert_eq!(second.revision, 2);
        assert_eq!(second.device, "pc");
    }

    #[test]
    fn version_one_files_load_and_upgrade() {
        let dir = test_dir("v1-upgrade");
        let v1 = serde_json::json!({
            "name": "legacy",
            "version": 1,
            "tracks": [{
                "title": "Old",
                "artist": "Hat",
                "url": "https://a/legacy",
                "source": "youtube",
                "quality": "high",
                "duration": 100.0,
            }],
        });
        fs::write(dir.join("legacy.json"), v1.to_string()).expect("write");

        let playlist = load(&dir, "legacy").expect("load").expect("present");
        assert_eq!(playlist.revision, 0);
        let legacy_id = crate::model::id_for_url("https://a/legacy");
        assert_eq!(playlist.tracks[0].id, legacy_id);
        assert_eq!(playlist.tracks[0].added_at, 0);

        // Saving upgrades the file to version 2 with a fresh revision.
        save_with_device(&dir, "legacy", &playlist.tracks, "pc").expect("save");
        let upgraded = load(&dir, "legacy").expect("load").expect("present");
        assert_eq!(upgraded.revision, 1);
        let doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("legacy.json")).expect("read"))
                .expect("json");
        assert_eq!(doc["version"], 2);
        assert_eq!(doc["tracks"][0]["id"], legacy_id);
    }

    #[test]
    fn track_ids_are_stable_across_url_variants() {
        let base = crate::model::id_for_url("https://www.youtube.com/watch?v=abc");
        assert_eq!(
            base,
            crate::model::id_for_url("HTTPS://WWW.YOUTUBE.COM/watch?v=abc")
        );
        assert_eq!(
            base,
            crate::model::id_for_url("https://www.youtube.com/watch?v=abc&utm_source=share&si=xyz")
        );
        assert_eq!(
            base,
            crate::model::id_for_url("https://www.youtube.com/watch?v=abc#t=30")
        );
        assert_ne!(
            base,
            crate::model::id_for_url("https://www.youtube.com/watch?v=other")
        );
    }

    #[test]
    fn store_stamps_device_and_detects_external_changes() {
        let dir = test_dir("store");
        let mut store = PlaylistStore::open(dir.clone(), "pixel".into());

        // Prime the baseline; own writes must not count as external.
        assert!(!store.external_changes());
        store.save("mix", &[track("https://a/1")]).expect("save");
        store.add_track("mix", &track("https://a/2")).expect("add");
        assert!(!store.external_changes());

        let saved = store.load("mix").expect("load").expect("present");
        assert_eq!(saved.device, "pixel");
        assert_eq!(saved.revision, 2);

        // A foreign write (another process, sync client) is detected.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(dir.join("mix.json"), "{\"version\":2,\"tracks\":[]}").expect("write");
        assert!(store.external_changes());
        assert!(!store.external_changes());

        // Foreign file created and deleted.
        fs::write(dir.join("foreign.json"), "{}").expect("write");
        assert!(store.external_changes());
        store.delete("foreign").expect("delete");
        assert!(!store.external_changes());
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

        assert!(add_track_with_device(&dir, "mix", &track("https://a/2"), "pc").expect("add"));
        assert_eq!(
            load(&dir, "mix")
                .expect("load")
                .expect("present")
                .tracks
                .len(),
            2
        );

        assert!(remove_track_with_device(&dir, "mix", 0, "pc").expect("remove"));
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
