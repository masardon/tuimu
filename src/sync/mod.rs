//! Playlist sync engine.
//!
//! The engine is transport-agnostic: a [`RemoteStore`] moves bytes (Dropbox
//! in production, an in-memory map in tests), while all decisions — who
//! uploads, who downloads, when to merge, when to keep both copies — are
//! made by [`plan`], a pure function over three inputs:
//!
//! * *base*:   what this device last synced (persisted in `.sync/state.json`)
//! * *local*:  what is on disk right now
//! * *remote*: what the store lists right now
//!
//! Conflict policy: playlist-level, last-writer-merged. When both sides
//! changed a playlist, the track lists are 3-way merged by track id; if the
//! same track was changed differently on both sides, both playlists are kept
//! (`name (conflict-<device>)`), so no edit is ever lost. Deletions are
//! propagated through tombstones on the store and win over edits.

pub mod auth;
pub mod dropbox;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::model::Track;
use crate::playlist;

/// Marker file at the store root; a folder without it is not ours and is
/// refused (prevents leaking playlists into foreign folders and syncing
/// foreign files into ours).
pub const MARKER_PATH: &str = ".tuimu-sync";
/// Store subdirectory holding the playlists.
pub const PLAYLISTS_PREFIX: &str = "playlists/";
/// Tombstones live here, garbage-collected after 30 days.
pub const TOMBSTONES_PREFIX: &str = ".tombstones/";
const TOMBSTONE_TTL_SECS: u64 = 30 * 24 * 3600;

#[derive(Debug)]
pub enum StoreError {
    /// The remote file changed since the revision we based our write on.
    Conflict,
    /// Any other transport failure (network, auth, quota…).
    Other(anyhow::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict => write!(f, "remote file changed concurrently"),
            Self::Other(error) => write!(f, "{error:#}"),
        }
    }
}
impl std::error::Error for StoreError {}

impl From<anyhow::Error> for StoreError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

/// A file on the store, identified by path and content revision.
#[derive(Clone, Debug)]
pub struct RemoteFile {
    pub path: String,
    pub rev: String,
}

/// Byte transport for the sync engine. Implementations must treat `rev`
/// as an opaque content revision for optimistic concurrency.
pub trait RemoteStore {
    fn list(&mut self) -> Result<Vec<RemoteFile>, StoreError>;
    /// Raw bytes at `path`; `None` when the file does not exist remotely.
    fn download(&mut self, path: &str) -> Result<Option<Vec<u8>>, StoreError>;
    /// Create (`base_rev = None`) or update a file. `Err(StoreError::Conflict)`
    /// when the remote content moved past `base_rev`.
    fn upload(
        &mut self,
        path: &str,
        bytes: &[u8],
        base_rev: Option<&str>,
    ) -> Result<String, StoreError>;
    fn delete(&mut self, path: &str) -> Result<(), StoreError>;
}

/// Per-playlist record of the last synchronized state.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct BaseEntry {
    pub rev: Option<String>,
    /// Playlist JSON as last synced (merge base for 3-way merges).
    pub content: String,
}

/// Persisted sync state, stored as JSON under `<playlists>/.sync/state.json`.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SyncState {
    pub base: HashMap<String, BaseEntry>,
}

impl SyncState {
    fn load(path: &std::path::Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("could not create the sync state directory")?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(
            &tmp,
            serde_json::to_string(self).context("could not encode sync state")?,
        )
        .context("could not write the sync state")?;
        fs::rename(&tmp, path).context("could not finalize the sync state")?;
        Ok(())
    }
}

/// What one sync cycle did; drives the status line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CycleReport {
    pub uploaded: Vec<String>,
    pub downloaded: Vec<String>,
    pub deleted: Vec<String>,
    pub conflicts: Vec<String>,
}

impl CycleReport {
    pub fn is_empty(&self) -> bool {
        self.uploaded.is_empty()
            && self.downloaded.is_empty()
            && self.deleted.is_empty()
            && self.conflicts.is_empty()
    }

    fn touched(&self) -> HashSet<String> {
        self.uploaded
            .iter()
            .chain(&self.downloaded)
            .chain(&self.deleted)
            .chain(&self.conflicts)
            .cloned()
            .collect()
    }
}

/// One planned action per playlist; produced by [`plan`], executed by
/// [`Engine::cycle`].
#[derive(Clone, Debug, PartialEq)]
enum Action {
    /// Push local content, re-stamped with a fresh revision.
    Upload {
        name: String,
        base_rev: Option<String>,
    },
    /// Pull remote content over the local file.
    Download {
        name: String,
    },
    /// Remote gone, local untouched since base: delete locally.
    DeleteLocal {
        name: String,
    },
    /// Local gone, remote untouched since base: delete remotely + tombstone.
    DeleteRemote {
        name: String,
    },
    /// Both sides changed: merge tracks if possible, else keep both copies
    /// (remote wins the name, local survives as a conflict copy).
    MergeOrKeepBoth {
        name: String,
    },
    Nothing,
}

/// Pure decision table over the base (last synced), local contents, and the
/// remote listing (`name -> remote revision`). Never touches the disk or the
/// store; the engine executes what this returns.
fn plan(
    base: &HashMap<String, BaseEntry>,
    local: &HashMap<String, String>,
    remote: &HashMap<String, String>,
) -> HashMap<String, Action> {
    let mut names: Vec<&String> = base
        .keys()
        .chain(local.keys())
        .chain(remote.keys())
        .collect();
    names.sort();
    names.dedup();

    let mut actions = HashMap::new();
    for name in names {
        let base = base.get(name);
        let local = local.get(name);
        let remote = remote.get(name);
        let action = match (base, local, remote) {
            // Never synced anywhere.
            (None, Some(_), None) => Action::Upload {
                name: name.clone(),
                base_rev: None,
            },
            (None, None, Some(_)) => Action::Download { name: name.clone() },
            // Both sides have it but we have no base (fresh device):
            // merge against an empty base, keeping both sides' tracks.
            (None, Some(_), Some(_)) => Action::MergeOrKeepBoth { name: name.clone() },
            (None, None, None) => Action::Nothing,

            (Some(base), local, remote) => {
                let local_changed = match local {
                    Some(content) => *content != base.content,
                    None => true,
                };
                let remote_changed = match remote {
                    None => true,
                    Some(rev) => base.rev.as_deref().is_none_or(|base_rev| *rev != base_rev),
                };
                match (local, remote) {
                    (Some(_), Some(rev)) => match (local_changed, remote_changed) {
                        (false, false) => Action::Nothing,
                        (true, false) => Action::Upload {
                            name: name.clone(),
                            base_rev: Some(rev.clone()),
                        },
                        (false, true) => Action::Download { name: name.clone() },
                        (true, true) => Action::MergeOrKeepBoth { name: name.clone() },
                    },
                    // Deleted locally, remote untouched (or changed): deletion
                    // wins over concurrent edits.
                    (None, Some(_)) => Action::DeleteRemote { name: name.clone() },
                    // Deleted remotely, local untouched: propagate delete.
                    (Some(_), None) if !local_changed => Action::DeleteLocal { name: name.clone() },
                    // Deleted remotely, local changed: local survives, recreate.
                    (Some(_), None) => Action::Upload {
                        name: name.clone(),
                        base_rev: None,
                    },
                    // In base but nowhere else: nothing to do.
                    (None, None) => Action::Nothing,
                }
            }
        };
        actions.insert((*name).clone(), action);
    }
    actions
}

/// 3-way merge of track lists by id. Returns `None` when the same track was
/// edited differently on both sides (the caller keeps both copies).
///
/// Policy: base order is preserved, edits from both sides apply (ours win on
/// ties, borrowing a missing title from theirs), deletions win over edits,
/// and additions from both sides are appended ordered by `added_at`.
fn merge_tracks(base: &[Track], ours: &[Track], theirs: &[Track]) -> Option<Vec<Track>> {
    fn index(tracks: &[Track]) -> HashMap<&str, &Track> {
        tracks.iter().map(|t| (t.id.as_str(), t)).collect()
    }
    let (base_i, ours_i, theirs_i) = (index(base), index(ours), index(theirs));

    // Real conflict: the same existing track edited differently on both
    // sides. Deletions (a side missing the id) never conflict here —
    // deletion wins; add-add of the same id dedupes silently.
    for (id, ours_track) in &ours_i {
        if let (Some(base_track), Some(theirs_track)) = (base_i.get(*id), theirs_i.get(*id))
            && ours_track.title != base_track.title
            && theirs_track.title != base_track.title
            && ours_track.title != theirs_track.title
        {
            return None;
        }
    }

    let mut merged: Vec<Track> = Vec::with_capacity(base.len());
    for track in base {
        match (
            ours_i.get(track.id.as_str()),
            theirs_i.get(track.id.as_str()),
        ) {
            // Deleted on a side: deletion wins.
            (None, _) | (_, None) => {}
            (Some(ours), Some(theirs)) => {
                let mut chosen = (*ours).clone();
                if chosen.title.trim().is_empty() {
                    chosen.title = theirs.title.clone();
                }
                merged.push(chosen);
            }
        }
    }
    let mut additions: Vec<Track> = Vec::new();
    for source in [&theirs_i, &ours_i] {
        for (id, track) in source {
            if !base_i.contains_key(*id) && !merged.iter().any(|m| m.id == *id) {
                additions.push((*track).clone());
            }
        }
    }
    additions.sort_by_key(|t| t.added_at);
    additions.dedup_by(|a, b| a.id == b.id);
    merged.append(&mut additions);
    Some(merged)
}

/// The engine: owns the local playlists directory, the persisted sync
/// state, and a [`RemoteStore`].
pub struct Engine<S: RemoteStore> {
    store: S,
    dir: PathBuf,
    device: String,
    state_path: PathBuf,
    state: SyncState,
}

impl<S: RemoteStore> Engine<S> {
    pub fn new(store: S, dir: PathBuf, device: String) -> Self {
        let state_path = dir.join(".sync").join("state.json");
        let state = SyncState::load(&state_path);
        Self {
            store,
            dir,
            device,
            state_path,
            state,
        }
    }

    /// Run one full sync cycle. A transport error aborts the cycle before
    /// the state is persisted, so a later retry sees the same world.
    pub fn cycle(&mut self) -> Result<CycleReport> {
        let remote_files = self
            .store
            .list()
            .map_err(|error| anyhow::anyhow!("could not list remote files: {error}"))?;
        self.ensure_root(&remote_files)?;

        let mut remote: HashMap<String, String> = HashMap::new();
        let mut tombstones: Vec<String> = Vec::new();
        for file in &remote_files {
            if let Some(name) = file.path.strip_prefix(PLAYLISTS_PREFIX) {
                remote.insert(playlist_name_of(name), file.rev.clone());
            } else if let Some(name) = file.path.strip_prefix(TOMBSTONES_PREFIX) {
                tombstones.push(playlist_name_of(name));
            }
        }

        let local: HashMap<String, String> = playlist::list(&self.dir)
            .into_iter()
            .filter_map(|name| {
                playlist::read_raw(&self.dir, &name)
                    .ok()
                    .flatten()
                    .map(|content| (name, content))
            })
            .collect();

        let mut report = CycleReport::default();
        for action in plan(&self.state.base, &local, &remote).into_values() {
            match action {
                Action::Nothing => {}
                Action::Upload { name, base_rev } => {
                    let content = local
                        .get(&name)
                        .with_context(|| format!("planned upload of vanished playlist {name}"))?;
                    let revision = playlist::parse_raw(content)?.revision + 1;
                    let stamped =
                        playlist::reencode_with_revision(content, revision, &self.device)?;
                    let new_rev = self
                        .store
                        .upload(
                            &format!("{PLAYLISTS_PREFIX}{name}.json"),
                            stamped.as_bytes(),
                            base_rev.as_deref(),
                        )
                        .map_err(|error| anyhow::anyhow!("could not upload {name}: {error}"))?;
                    // Keep our file in lockstep with what we pushed.
                    playlist::write_raw(&self.dir, &name, &stamped)?;
                    self.state.base.insert(
                        name.clone(),
                        BaseEntry {
                            rev: Some(new_rev),
                            content: stamped,
                        },
                    );
                    report.uploaded.push(name);
                }
                Action::Download { name } => {
                    let content = self
                        .download_playlist(&name)?
                        .with_context(|| format!("remote copy of {name} vanished during sync"))?;
                    playlist::write_raw(&self.dir, &name, &content)?;
                    self.state.base.insert(
                        name.clone(),
                        BaseEntry {
                            rev: remote.get(&name).cloned(),
                            content: content.clone(),
                        },
                    );
                    report.downloaded.push(name);
                }
                Action::DeleteLocal { name } => {
                    playlist::delete(&self.dir, &name)?;
                    self.state.base.remove(&name);
                    report.deleted.push(name);
                }
                Action::DeleteRemote { name } => {
                    self.store
                        .delete(&format!("{PLAYLISTS_PREFIX}{name}.json"))
                        .map_err(|error| {
                            anyhow::anyhow!("could not delete {name} remotely: {error}")
                        })?;
                    self.write_tombstone(&name)?;
                    self.state.base.remove(&name);
                    report.deleted.push(name);
                }
                Action::MergeOrKeepBoth { name } => {
                    self.merge_or_keep_both(&name, &local, &remote, &mut report)?;
                }
            }
        }

        self.apply_tombstones(&tombstones, &report.touched())?;
        self.gc_tombstones(&remote_files)?;
        self.state.save(&self.state_path)?;
        Ok(report)
    }

    fn merge_or_keep_both(
        &mut self,
        name: &str,
        local: &HashMap<String, String>,
        remote: &HashMap<String, String>,
        report: &mut CycleReport,
    ) -> Result<()> {
        let base_entry = self.state.base.get(name).cloned().unwrap_or_default();
        let base_tracks = playlist::parse_raw(&base_entry.content)
            .map(|p| p.tracks)
            .unwrap_or_default();
        let ours = local
            .get(name)
            .with_context(|| format!("planned merge of vanished playlist {name}"))?;
        let ours_tracks = playlist::parse_raw(ours)?.tracks;
        let theirs_raw = self
            .download_playlist(name)?
            .with_context(|| format!("remote copy of {name} vanished during sync"))?;
        let theirs_tracks = playlist::parse_raw(&theirs_raw)?.tracks;

        match merge_tracks(&base_tracks, &ours_tracks, &theirs_tracks) {
            Some(merged) if merged != base_tracks => {
                let revision = playlist::parse_raw(&theirs_raw)?
                    .revision
                    .max(playlist::parse_raw(ours)?.revision)
                    + 1;
                let content = encode_merged(&merged, name, revision, &self.device);
                let new_rev = self
                    .store
                    .upload(
                        &format!("{PLAYLISTS_PREFIX}{name}.json"),
                        content.as_bytes(),
                        remote.get(name).map(String::as_str),
                    )
                    .map_err(|error| anyhow::anyhow!("could not upload merged {name}: {error}"))?;
                playlist::write_raw(&self.dir, name, &content)?;
                self.state.base.insert(
                    name.to_string(),
                    BaseEntry {
                        rev: Some(new_rev),
                        content,
                    },
                );
                report.uploaded.push(name.to_string());
            }
            Some(_) => {
                // Both sides ended up at the same content; adopt the remote
                // revision as the base and stop churning.
                let content = theirs_raw;
                playlist::write_raw(&self.dir, name, &content)?;
                self.state.base.insert(
                    name.to_string(),
                    BaseEntry {
                        rev: remote.get(name).cloned(),
                        content,
                    },
                );
            }
            None => {
                // Keep both: the remote keeps the name; our divergent copy
                // moves aside so nothing is lost.
                let conflict_name =
                    playlist::sanitize_name(&format!("{name} (conflict-{})", self.device));
                playlist::write_raw(&self.dir, &conflict_name, ours)?;
                playlist::write_raw(&self.dir, name, &theirs_raw)?;
                self.state.base.insert(
                    name.to_string(),
                    BaseEntry {
                        rev: remote.get(name).cloned(),
                        content: theirs_raw,
                    },
                );
                report.conflicts.push(name.to_string());
            }
        }
        Ok(())
    }

    fn download_playlist(&mut self, name: &str) -> Result<Option<String>> {
        self.store
            .download(&format!("{PLAYLISTS_PREFIX}{name}.json"))
            .map_err(|error| anyhow::anyhow!("could not download {name}: {error}"))
            .and_then(|bytes| {
                bytes
                    .map(|b| {
                        String::from_utf8(b)
                            .map_err(|_| anyhow::anyhow!("{name} is not valid UTF-8"))
                    })
                    .transpose()
            })
    }

    /// Refuse to touch a store root that is not ours: the marker must be
    /// the only non-playlist content, or the root must be empty.
    fn ensure_root(&mut self, remote_files: &[RemoteFile]) -> Result<()> {
        let known = remote_files.iter().all(|file| {
            file.path == MARKER_PATH
                || file.path.starts_with(PLAYLISTS_PREFIX)
                || file.path.starts_with(TOMBSTONES_PREFIX)
        });
        if !known {
            bail!("the remote sync folder contains foreign files; refusing to sync");
        }
        if remote_files.iter().any(|file| file.path == MARKER_PATH) {
            return Ok(());
        }
        if remote_files.is_empty() {
            let marker = format!(
                "tuimu sync root\nversion 1\ncreated {}\n",
                crate::model::now_epoch()
            );
            self.store
                .upload(MARKER_PATH, marker.as_bytes(), None)
                .map_err(|error| anyhow::anyhow!("could not create the sync marker: {error}"))?;
            return Ok(());
        }
        bail!("the remote sync folder is not a TuiMu sync root")
    }

    fn write_tombstone(&mut self, name: &str) -> Result<()> {
        let doc = serde_json::json!({
            "name": name,
            "deleted_at": crate::model::now_epoch(),
            "device": self.device,
        });
        self.store
            .upload(
                &format!("{TOMBSTONES_PREFIX}{name}.json"),
                doc.to_string().as_bytes(),
                None,
            )
            .map_err(|error| {
                anyhow::anyhow!("could not write the tombstone for {name}: {error}")
            })?;
        Ok(())
    }

    /// Apply remote tombstones: delete local copies of playlists another
    /// device deleted. Playlists this cycle already wrote (recreated after a
    /// delete, or freshly downloaded) are skipped, and applied tombstones
    /// are removed from the store so they cannot bite twice.
    fn apply_tombstones(&mut self, tombstones: &[String], touched: &HashSet<String>) -> Result<()> {
        for name in tombstones {
            if touched.contains(name) {
                continue;
            }
            let existed_locally = self.state.base.remove(name).is_some()
                || playlist::read_raw(&self.dir, name)?.is_some();
            if existed_locally {
                playlist::delete(&self.dir, name)?;
            }
            self.store
                .delete(&format!("{TOMBSTONES_PREFIX}{name}.json"))
                .map_err(|error| {
                    anyhow::anyhow!("could not clear the tombstone for {name}: {error}")
                })?;
        }
        Ok(())
    }

    fn gc_tombstones(&mut self, remote_files: &[RemoteFile]) -> Result<()> {
        let now = crate::model::now_epoch();
        for file in remote_files {
            if !file.path.starts_with(TOMBSTONES_PREFIX) {
                continue;
            }
            let Ok(Some(bytes)) = self.store.download(&file.path) else {
                continue;
            };
            if let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && let Some(deleted_at) = doc.get("deleted_at").and_then(|value| value.as_u64())
                && now.saturating_sub(deleted_at) > TOMBSTONE_TTL_SECS
            {
                let _ = self.store.delete(&file.path);
            }
        }
        Ok(())
    }
}

fn playlist_name_of(path: &str) -> String {
    path.trim_end_matches(".json").to_string()
}

fn encode_merged(tracks: &[Track], name: &str, revision: u64, device: &str) -> String {
    let doc = serde_json::json!({
        "name": name,
        "version": 2,
        "revision": revision,
        "updated_at": crate::model::now_epoch(),
        "device": device,
        "tracks": tracks.iter().map(playlist::track_to_json).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&doc).expect("playlist JSON always encodes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Quality, Source, Track};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    // ---- test doubles -----------------------------------------------------

    /// In-memory store shared between device engines, so two `Engine`s see
    /// one "cloud". Revs are monotonic strings.
    #[derive(Clone, Default)]
    struct FakeStore {
        files: Arc<Mutex<Files>>,
        rev_counter: Arc<AtomicU64>,
        failing: Arc<AtomicBool>,
    }

    type Files = HashMap<String, (String, Vec<u8>)>;

    impl FakeStore {
        fn new() -> Self {
            Self::default()
        }

        fn set_failing(&self, failing: bool) {
            self.failing.store(failing, Ordering::SeqCst);
        }

        fn paths(&self) -> Vec<String> {
            self.files.lock().unwrap().keys().cloned().collect()
        }
    }

    impl RemoteStore for FakeStore {
        fn list(&mut self) -> Result<Vec<RemoteFile>, StoreError> {
            if self.failing.load(Ordering::SeqCst) {
                return Err(StoreError::Other(anyhow::anyhow!("simulated outage")));
            }
            let files = self.files.lock().unwrap();
            Ok(files
                .iter()
                .map(|(path, (rev, _))| RemoteFile {
                    path: path.clone(),
                    rev: rev.clone(),
                })
                .collect())
        }

        fn download(&mut self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
            Ok(self
                .files
                .lock()
                .unwrap()
                .get(path)
                .map(|(_, bytes)| bytes.clone()))
        }

        fn upload(
            &mut self,
            path: &str,
            bytes: &[u8],
            base_rev: Option<&str>,
        ) -> Result<String, StoreError> {
            let mut files = self.files.lock().unwrap();
            let current = files.get(path);
            match (current, base_rev) {
                (Some((rev, _)), Some(base)) if rev != base => {
                    return Err(StoreError::Conflict);
                }
                (Some(_), None) => return Err(StoreError::Conflict),
                _ => {}
            }
            let rev = self.rev_counter.fetch_add(1, Ordering::SeqCst).to_string();
            files.insert(path.to_string(), (rev.clone(), bytes.to_vec()));
            Ok(rev)
        }

        fn delete(&mut self, path: &str) -> Result<(), StoreError> {
            self.files.lock().unwrap().remove(path);
            Ok(())
        }
    }

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tuimu-sync-{tag}-{}-{}",
            std::process::id(),
            crate::model::now_epoch()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn track(url: &str, title: &str) -> Track {
        Track::new(
            title.into(),
            "Artist".into(),
            Some(180.0),
            url.into(),
            Source::YouTube,
            Quality::High,
        )
    }

    fn save_local(dir: &std::path::Path, name: &str, tracks: &[Track]) {
        playlist::save_with_device(dir, name, tracks, "local").expect("save");
    }

    fn device(store: &FakeStore, dir: PathBuf, name: &str) -> Engine<FakeStore> {
        Engine::new(store.clone(), dir, name.to_string())
    }

    // ---- pure plan --------------------------------------------------------

    #[test]
    fn plan_decides_each_combination() {
        let mut base = HashMap::new();
        base.insert(
            "same".into(),
            BaseEntry {
                rev: Some("r1".into()),
                content: "c1".into(),
            },
        );
        base.insert(
            "local-edited".into(),
            BaseEntry {
                rev: Some("r2".into()),
                content: "c2".into(),
            },
        );
        base.insert(
            "remote-edited".into(),
            BaseEntry {
                rev: Some("r3".into()),
                content: "c3".into(),
            },
        );
        base.insert(
            "deleted-locally".into(),
            BaseEntry {
                rev: Some("r4".into()),
                content: "c4".into(),
            },
        );
        base.insert(
            "deleted-remotely".into(),
            BaseEntry {
                rev: Some("r5".into()),
                content: "c5".into(),
            },
        );
        base.insert(
            "recreated".into(),
            BaseEntry {
                rev: Some("r6".into()),
                content: "c6".into(),
            },
        );

        let mut local = HashMap::new();
        local.insert("fresh-local".into(), "x".into());
        local.insert("same".into(), "c1".into());
        local.insert("local-edited".into(), "c2-changed".into());
        local.insert("remote-edited".into(), "c3".into());
        local.insert("both-changed".into(), "ours".into());
        local.insert("deleted-remotely".into(), "c5".into());
        local.insert("recreated".into(), "c6-new".into());

        let mut remote = HashMap::new();
        remote.insert("fresh-remote".into(), "rX".into());
        remote.insert("same".into(), "r1".into());
        remote.insert("local-edited".into(), "r2".into());
        remote.insert("remote-edited".into(), "r9".into());
        remote.insert("both-changed".into(), "rY".into());
        remote.insert("deleted-locally".into(), "r4".into());
        // deleted-remotely: absent; recreated: absent

        let actions = plan(&base, &local, &remote);
        let get = |name: &str| actions.get(name).expect("action");

        assert_eq!(
            *get("fresh-local"),
            Action::Upload {
                name: "fresh-local".into(),
                base_rev: None
            }
        );
        assert_eq!(
            *get("fresh-remote"),
            Action::Download {
                name: "fresh-remote".into()
            }
        );
        assert_eq!(*get("same"), Action::Nothing);
        assert_eq!(
            *get("local-edited"),
            Action::Upload {
                name: "local-edited".into(),
                base_rev: Some("r2".into())
            }
        );
        assert_eq!(
            *get("remote-edited"),
            Action::Download {
                name: "remote-edited".into()
            }
        );
        assert_eq!(
            *get("both-changed"),
            Action::MergeOrKeepBoth {
                name: "both-changed".into()
            }
        );
        assert_eq!(
            *get("deleted-locally"),
            Action::DeleteRemote {
                name: "deleted-locally".into()
            }
        );
        assert_eq!(
            *get("deleted-remotely"),
            Action::DeleteLocal {
                name: "deleted-remotely".into()
            }
        );
        assert_eq!(
            *get("recreated"),
            Action::Upload {
                name: "recreated".into(),
                base_rev: None
            }
        );
    }

    // ---- pure merge -------------------------------------------------------

    #[test]
    fn merge_unions_additions_and_keeps_order() {
        let base = [track("https://a/1", "One"), track("https://a/2", "Two")];
        let ours = base.to_vec();
        let theirs = [
            base[0].clone(),
            base[1].clone(),
            track("https://a/3", "Three"),
        ];
        let merged = merge_tracks(&base, &ours, &theirs).expect("merge");
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[2].url, "https://a/3");
    }

    #[test]
    fn merge_deletion_wins() {
        let base = [track("https://a/1", "One"), track("https://a/2", "Two")];
        let ours = vec![base[0].clone()]; // removed track 2
        let theirs = base.to_vec(); // untouched
        let merged = merge_tracks(&base, &ours, &theirs).expect("merge");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].url, "https://a/1");
    }

    #[test]
    fn merge_conflicting_edits_returns_none() {
        let base = [track("https://a/1", "One")];
        let ours = [track("https://a/1", "One (remix)")];
        let theirs = [track("https://a/1", "One (live)")];
        assert!(merge_tracks(&base, &ours, &theirs).is_none());
    }

    #[test]
    fn merge_identical_edits_do_not_conflict() {
        let base = [track("https://a/1", "One")];
        let ours = [track("https://a/1", "One (remix)")];
        let theirs = [track("https://a/1", "One (remix)")];
        let merged = merge_tracks(&base, &ours, &theirs).expect("merge");
        assert_eq!(merged[0].title, "One (remix)");
    }

    // ---- engine end-to-end -----------------------------------------------

    #[test]
    fn fresh_upload_then_download_on_second_device() {
        let store = FakeStore::new();
        let dir_a = dir("up-a");
        let dir_b = dir("up-b");
        save_local(&dir_a, "mix", &[track("https://a/1", "One")]);

        let report = device(&store, dir_a.clone(), "pixel")
            .cycle()
            .expect("cycle a");
        assert_eq!(report.uploaded, vec!["mix".to_string()]);

        let report = device(&store, dir_b.clone(), "pc")
            .cycle()
            .expect("cycle b");
        assert_eq!(report.downloaded, vec!["mix".to_string()]);
        let b = playlist::load(&dir_b, "mix")
            .expect("load")
            .expect("present");
        assert_eq!(b.tracks.len(), 1);
        assert_eq!(b.tracks[0].url, "https://a/1");
    }

    #[test]
    fn edits_flow_between_devices() {
        let store = FakeStore::new();
        let dir_a = dir("flow-a");
        let dir_b = dir("flow-b");
        save_local(&dir_a, "mix", &[track("https://a/1", "One")]);
        device(&store, dir_a.clone(), "pixel").cycle().expect("a1");
        device(&store, dir_b.clone(), "pc").cycle().expect("b1");

        // B adds a track and syncs; A downloads it.
        playlist::add_track_with_device(&dir_b, "mix", &track("https://a/2", "Two"), "pc")
            .expect("add");
        device(&store, dir_b.clone(), "pc").cycle().expect("b2");
        let report = device(&store, dir_a.clone(), "pixel").cycle().expect("a2");
        assert_eq!(report.downloaded, vec!["mix".to_string()]);
        let a = playlist::load(&dir_a, "mix")
            .expect("load")
            .expect("present");
        assert_eq!(a.tracks.len(), 2);
    }

    #[test]
    fn concurrent_additions_merge() {
        let store = FakeStore::new();
        let dir_a = dir("merge-a");
        let dir_b = dir("merge-b");
        save_local(&dir_a, "mix", &[track("https://a/1", "One")]);
        device(&store, dir_a.clone(), "pixel").cycle().expect("a1");
        device(&store, dir_b.clone(), "pc").cycle().expect("b1");

        // Both devices add different tracks while offline, then sync.
        playlist::add_track_with_device(&dir_a, "mix", &track("https://a/9", "Nine"), "pixel")
            .expect("add a");
        playlist::add_track_with_device(&dir_b, "mix", &track("https://a/5", "Five"), "pc")
            .expect("add b");
        device(&store, dir_a.clone(), "pixel").cycle().expect("a2");
        let report = device(&store, dir_b.clone(), "pc").cycle().expect("b2");
        assert!(report.conflicts.is_empty());
        let b = playlist::load(&dir_b, "mix")
            .expect("load")
            .expect("present");
        assert_eq!(b.tracks.len(), 3);
        device(&store, dir_a.clone(), "pixel").cycle().expect("a3");
        let a = playlist::load(&dir_a, "mix")
            .expect("load")
            .expect("present");
        assert_eq!(a.tracks.len(), 3);
    }

    #[test]
    fn conflicting_track_edits_keep_both_playlists() {
        let store = FakeStore::new();
        let dir_a = dir("conflict-a");
        let dir_b = dir("conflict-b");
        save_local(&dir_a, "mix", &[track("https://a/1", "One")]);
        device(&store, dir_a.clone(), "pixel").cycle().expect("a1");
        device(&store, dir_b.clone(), "pc").cycle().expect("b1");

        // Both edit the same track differently.
        save_local(&dir_a, "mix", &[track("https://a/1", "One (pixel edit)")]);
        save_local(&dir_b, "mix", &[track("https://a/1", "One (pc edit)")]);

        device(&store, dir_a.clone(), "pixel").cycle().expect("a2");
        let report = device(&store, dir_b.clone(), "pc").cycle().expect("b2");
        assert_eq!(report.conflicts, vec!["mix".to_string()]);

        // B kept both: the remote copy under the original name and its own
        // divergent edit as a conflict copy.
        let remote_copy = playlist::load(&dir_b, "mix")
            .expect("load")
            .expect("present");
        assert_eq!(remote_copy.tracks[0].title, "One (pixel edit)");
        let conflict = playlist::load(&dir_b, "mix (conflict-pc)")
            .expect("load")
            .expect("present");
        assert_eq!(conflict.tracks[0].title, "One (pc edit)");
    }

    #[test]
    fn deletions_propagate_through_tombstones() {
        let store = FakeStore::new();
        let dir_a = dir("del-a");
        let dir_b = dir("del-b");
        save_local(&dir_a, "mix", &[track("https://a/1", "One")]);
        device(&store, dir_a.clone(), "pixel").cycle().expect("a1");
        device(&store, dir_b.clone(), "pc").cycle().expect("b1");

        // A deletes; the tombstone makes B delete its copy too.
        playlist::delete(&dir_a, "mix").expect("delete");
        let report = device(&store, dir_a.clone(), "pixel").cycle().expect("a2");
        assert_eq!(report.deleted, vec!["mix".to_string()]);
        assert!(
            store
                .paths()
                .iter()
                .any(|path| path.starts_with(TOMBSTONES_PREFIX))
        );

        let report = device(&store, dir_b.clone(), "pc").cycle().expect("b2");
        assert!(playlist::load(&dir_b, "mix").expect("load").is_none());
        assert_eq!(report.downloaded, Vec::<String>::new());
    }

    #[test]
    fn recreated_playlist_survives_stale_tombstone() {
        let store = FakeStore::new();
        let dir_a = dir("tomb-a");
        let dir_b = dir("tomb-b");
        save_local(&dir_a, "mix", &[track("https://a/1", "One")]);
        device(&store, dir_a.clone(), "pixel").cycle().expect("a1");
        device(&store, dir_b.clone(), "pc").cycle().expect("b1");
        playlist::delete(&dir_a, "mix").expect("delete");
        device(&store, dir_a.clone(), "pixel").cycle().expect("a2");

        // B recreates the same name before learning about the delete; the
        // fresh upload must not be undone by the stale tombstone.
        save_local(&dir_b, "mix", &[track("https://a/7", "Seven")]);
        let report = device(&store, dir_b.clone(), "pc").cycle().expect("b2");
        assert_eq!(report.uploaded, vec!["mix".to_string()]);
        assert!(playlist::load(&dir_b, "mix").expect("load").is_some());
    }

    #[test]
    fn foreign_store_root_is_refused() {
        let store = FakeStore::new();
        {
            let mut guard = store.files.lock().unwrap();
            guard.insert(
                "README.md".into(),
                ("0".into(), b"someone else's folder".to_vec()),
            );
        }
        let result = device(&store, dir("foreign"), "pixel").cycle();
        assert!(result.is_err());
        // Nothing leaked into the store.
        assert_eq!(store.paths(), vec!["README.md".to_string()]);
    }

    #[test]
    fn transport_failure_leaves_state_untouched() {
        let store = FakeStore::new();
        let dir_a = dir("fail-a");
        save_local(&dir_a, "mix", &[track("https://a/1", "One")]);
        device(&store, dir_a.clone(), "pixel").cycle().expect("a1");

        // Outage: the cycle fails, local data intact, no state written for
        // the failing part; after recovery the cycle still works.
        store.set_failing(true);
        let dir_b = dir("fail-b");
        let mut engine = device(&store, dir_b.clone(), "pc");
        assert!(engine.cycle().is_err());
        store.set_failing(false);
        let report = engine.cycle().expect("recovered");
        assert_eq!(report.downloaded, vec!["mix".to_string()]);
    }
}
