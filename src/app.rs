use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::config;
use crate::input::TextInput;
use crate::lyrics::{self, Lyrics};
use crate::model::{QualityFilter, RepeatMode, SearchSource, Track};
use crate::mpv::{Mpv, MpvMessage, Player};
use crate::playlist::{self, Playlist, PlaylistStore};
use crate::visualizer::{BANDS, Visualizer, VisualizerStatus, smooth};
use crate::ytdlp::{self, Lookup, StreamInfo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Search,
    Results,
    Queue,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Search => Self::Results,
            Self::Results => Self::Queue,
            Self::Queue => Self::Search,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Search => Self::Queue,
            Self::Results => Self::Search,
            Self::Queue => Self::Results,
        }
    }
}

#[derive(Debug, Default)]
pub struct PlayerState {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    pub loading: bool,
}

struct SearchMessage {
    seq: u64,
    result: Result<Lookup, String>,
}

struct PlayMessage {
    seq: u64,
    track_url: String,
    result: Result<StreamInfo, String>,
}

struct LyricsMessage {
    track_url: String,
    result: Result<Lyrics, String>,
}

#[derive(Debug, Default)]
pub struct LyricsState {
    pub visible: bool,
    pub track_url: Option<String>,
    pub title: String,
    pub artist: String,
    pub lines: Option<Lyrics>,
    pub loading: bool,
    pub error: Option<String>,
    pub instrumental: bool,
    pub source: Option<String>,
}

/// What the lyrics panel should display for the current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LyricsStatus {
    /// No track has been played yet.
    Idle,
    /// A fetch is in flight.
    Loading,
    /// The fetch failed or produced no lyrics.
    Unavailable,
    /// The track is marked as instrumental.
    Instrumental,
    /// Lyrics are ready to render.
    Ready,
}

impl LyricsState {
    pub fn status(&self) -> LyricsStatus {
        if self.loading {
            LyricsStatus::Loading
        } else if self.instrumental {
            LyricsStatus::Instrumental
        } else if let Some(lines) = &self.lines {
            if lines.line_count() == 0 {
                LyricsStatus::Unavailable
            } else {
                LyricsStatus::Ready
            }
        } else if self.error.is_some() {
            LyricsStatus::Unavailable
        } else {
            LyricsStatus::Idle
        }
    }
}

/// State of the playlist popups. `None` means no popup is open and keys go
/// to the regular views.
#[derive(Default)]
pub enum PlaylistUi {
    #[default]
    None,
    /// Text prompt used for both "save queue as playlist" and renaming.
    NamePrompt {
        action: NameAction,
        input: TextInput,
    },
    /// Browser for saved playlists (and, one level down, their tracks).
    Picker {
        mode: PickerMode,
        level: PickerLevel,
        selected: usize,
        names: Vec<String>,
    },
}

#[derive(Clone, Debug)]
pub enum NameAction {
    SaveQueue,
    Rename { old: String },
}

#[derive(Clone, Debug)]
pub enum PickerMode {
    Open,
    /// Add a specific track to whichever playlist is chosen.
    AddTo {
        track: Track,
    },
}

#[derive(Clone, Debug)]
pub enum PickerLevel {
    Playlists,
    /// Tracks of `playlist`, cached so the picker stays in sync with edits.
    Tracks {
        playlist: String,
        tracks: Vec<Track>,
    },
}

/// How long an `Unavailable` visualizer waits before retrying capture.
const VIZ_RETRY: Duration = Duration::from_secs(5);
/// How often playlist files are scanned for changes made by other
/// processes.
const PLAYLIST_SCAN_INTERVAL: Duration = Duration::from_secs(2);

pub struct App {
    pub should_quit: bool,
    pub focus: Focus,
    pub input: TextInput,
    pub source: SearchSource,
    pub all_results: Vec<Track>,
    pub results: Vec<Track>,
    pub result_selected: usize,
    pub queue: Vec<Track>,
    pub queue_selected: usize,
    pub current: Option<usize>,
    pub player: PlayerState,
    pub repeat: RepeatMode,
    pub status: String,
    pub status_is_error: bool,
    pub help: bool,
    pub loading: bool,
    pub tick: u64,
    pub lyrics: LyricsState,
    pub lyrics_enabled: bool,
    pub playlist_ui: PlaylistUi,
    playlists: PlaylistStore,
    /// When playlist files were last scanned for external changes.
    playlist_scan_at: Instant,
    pub viz_enabled: bool,
    pub viz_visible: bool,
    viz: Visualizer,
    viz_bars: Vec<f32>,
    /// When an `Unavailable` visualizer may be retried next.
    viz_retry_at: Instant,
    pub quality_filter: QualityFilter,
    prev_volume: f64,
    search_seq: u64,
    search_tx: Sender<SearchMessage>,
    search_rx: Receiver<SearchMessage>,
    play_seq: u64,
    play_tx: Sender<PlayMessage>,
    play_rx: Receiver<PlayMessage>,
    lyrics_tx: Sender<LyricsMessage>,
    lyrics_rx: Receiver<LyricsMessage>,
    lyrics_pending: HashSet<String>,
    lyrics_cache: HashMap<String, Result<Lyrics, String>>,
    mpv: Box<dyn Player>,
    mpv_closed: bool,
}

impl App {
    pub fn new(mpv: Mpv, lyrics_enabled: bool, viz_enabled: bool) -> Self {
        Self::with_player(Box::new(mpv), lyrics_enabled, viz_enabled)
    }

    fn with_player(mpv: Box<dyn Player>, lyrics_enabled: bool, viz_enabled: bool) -> Self {
        let (search_tx, search_rx) = mpsc::channel();
        let (play_tx, play_rx) = mpsc::channel();
        let (lyrics_tx, lyrics_rx) = mpsc::channel();
        Self {
            should_quit: false,
            focus: Focus::Search,
            input: TextInput::default(),
            source: SearchSource::YouTube,
            all_results: Vec::new(),
            results: Vec::new(),
            result_selected: 0,
            queue: Vec::new(),
            queue_selected: 0,
            current: None,
            player: PlayerState {
                volume: 70.0,
                paused: true,
                ..PlayerState::default()
            },
            repeat: RepeatMode::Off,
            status: "Type a search and press Enter. ? help · @ source.".into(),
            status_is_error: false,
            help: false,
            loading: false,
            tick: 0,
            lyrics: LyricsState {
                visible: lyrics_enabled,
                ..LyricsState::default()
            },
            lyrics_enabled,
            playlist_ui: PlaylistUi::None,
            playlists: PlaylistStore::open(
                playlist::playlists_dir().unwrap_or_else(|_| PathBuf::from(".")),
                config::Config::load().device,
            ),
            playlist_scan_at: Instant::now(),
            viz_enabled,
            viz_visible: viz_enabled,
            viz: Visualizer::default(),
            viz_bars: vec![0.0; BANDS],
            viz_retry_at: Instant::now(),
            quality_filter: QualityFilter::All,
            prev_volume: 70.0,
            search_seq: 0,
            search_tx,
            search_rx,
            play_seq: 0,
            play_tx,
            play_rx,
            lyrics_tx,
            lyrics_rx,
            lyrics_pending: HashSet::new(),
            lyrics_cache: HashMap::new(),
            mpv,
            mpv_closed: false,
        }
    }

    pub fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|index| self.queue.get(index))
    }

    pub fn current_track_url(&self) -> Option<&str> {
        self.current_track().map(|track| track.url.as_str())
    }

    pub fn poll_background(&mut self) {
        // Watch for playlist files changed by other processes (a sync client
        // such as Syncthing, or another tuimu on the same machine) and fold
        // them into the open picker.
        if self.playlist_scan_at.elapsed() >= PLAYLIST_SCAN_INTERVAL {
            self.playlist_scan_at = Instant::now();
            if self.playlists.external_changes() {
                self.refresh_playlist_picker();
                self.set_status("Playlists changed on disk.", false);
            }
        }

        while let Ok(message) = self.search_rx.try_recv() {
            if message.seq == self.search_seq {
                self.handle_search_result(message.result);
            }
        }

        while let Ok(message) = self.play_rx.try_recv() {
            if message.seq == self.play_seq {
                self.handle_play_result(message);
            }
        }

        while let Ok(message) = self.lyrics_rx.try_recv() {
            self.lyrics_pending.remove(&message.track_url);
            let is_current = self.lyrics.track_url.as_deref() == Some(message.track_url.as_str())
                || self.current_track_url() == Some(message.track_url.as_str());
            self.lyrics_cache
                .insert(message.track_url.clone(), message.result.clone());
            if is_current {
                self.apply_lyrics_result(&message.result);
            }
        }

        if self.mpv_closed {
            return;
        }

        loop {
            match self.mpv.try_recv() {
                Ok(MpvMessage::Event(event)) => self.handle_mpv_event(event),
                Ok(MpvMessage::Closed(reason)) => {
                    self.mpv_closed = true;
                    self.player.loading = false;
                    self.player.paused = true;
                    self.set_status(format!("Playback engine stopped: {reason}"), true);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.mpv_closed = true;
                    break;
                }
            }
        }
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.tick_visualizer();
    }

    /// Current capture state of the visualizer.
    pub fn viz_status(&self) -> VisualizerStatus {
        self.viz.status()
    }

    /// Latest smoothed band levels (0..1), always `BANDS` long.
    pub fn viz_bars(&self) -> &[f32] {
        &self.viz_bars
    }

    /// `v`: show or hide the visualizer panel.
    pub fn toggle_visualizer(&mut self) {
        if !self.viz_enabled {
            self.set_status("Visualizer is disabled by --no-visualizer.", true);
            return;
        }
        self.viz_visible = !self.viz_visible;
    }

    fn tick_visualizer(&mut self) {
        // Capture runs only while the panel is shown. A failed start is
        // retried every few seconds, so installing PulseAudio mid-session
        // (or its daemon coming up later) heals on its own.
        if self.viz_enabled && self.viz_visible {
            match self.viz.status() {
                VisualizerStatus::Stopped => self.viz.start(),
                VisualizerStatus::Unavailable if self.viz_retry_at.elapsed() >= VIZ_RETRY => {
                    self.viz_retry_at = Instant::now();
                    self.viz.stop();
                    self.viz.start();
                }
                _ => {}
            }
        } else if self.viz.status() != VisualizerStatus::Stopped {
            self.viz.stop();
        }

        match self.viz.poll() {
            Some(bands) => self.viz_bars = smooth(&self.viz_bars, &bands),
            // No frame this tick: keep decaying so bars fall instead of
            // freezing (covers pauses and silent monitors).
            None => {
                if self.viz.status() == VisualizerStatus::Running {
                    let silent = vec![0.0; BANDS];
                    self.viz_bars = smooth(&self.viz_bars, &silent);
                }
            }
        }
    }

    /// Reload picker caches after playlist files changed underneath us.
    fn refresh_playlist_picker(&mut self) {
        if let PlaylistUi::Picker { names, level, .. } = &mut self.playlist_ui {
            *names = self.playlists.list();
            if let PickerLevel::Tracks { playlist, tracks } = level
                && let Some(loaded) = self.playlists.load(playlist).unwrap_or(None)
            {
                *tracks = loaded.tracks;
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        let text = text.replace(['\r', '\n'], " ");
        if let PlaylistUi::NamePrompt { input, .. } = &mut self.playlist_ui {
            input.insert_str(text.trim());
            return;
        }
        if self.focus == Focus::Search && !self.help {
            self.input.insert_str(text.trim());
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Quitting is always available, even while editing or inside a popup.
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
            self.should_quit = true;
            return;
        }

        if self.help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::F(1)
            ) {
                self.help = false;
            }
            return;
        }

        // Playlist popups swallow all keys except the global quit above.
        if !matches!(self.playlist_ui, PlaylistUi::None) {
            self.handle_playlist_key(key);
            return;
        }

        // Printable keys are reserved for the search input while it is focused,
        // so shortcuts such as `q`, `@`, and `#` must be typed there normally.
        match self.focus {
            Focus::Search => self.handle_search_key(key, ctrl),
            Focus::Results => self.handle_results_key(key),
            Focus::Queue => self.handle_queue_key(key),
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent, ctrl: bool) {
        match key.code {
            KeyCode::Enter => self.start_search(),
            KeyCode::Esc => self.focus = Focus::Results,
            KeyCode::F(1) => self.help = true,
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.previous(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Char(ch) if ctrl => match ch.to_ascii_lowercase() {
                'u' => self.input.clear(),
                'w' => self.input.delete_word(),
                'a' => self.input.home(),
                'e' => self.input.end(),
                _ => {}
            },
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.input.insert(ch);
            }
            _ => {}
        }
    }

    fn handle_results_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc | KeyCode::Char('/') | KeyCode::Char('i') => {
                self.focus = Focus::Search;
            }
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.previous(),
            KeyCode::Down | KeyCode::Char('j') => self.move_result(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_result(-1),
            KeyCode::Char('g') => self.select_result(0),
            KeyCode::Char('G') => {
                if !self.results.is_empty() {
                    self.select_result(self.results.len() - 1);
                }
            }
            KeyCode::Enter => self.play_results_selected(),
            KeyCode::Char('a') => self.add_selected_result(),
            KeyCode::Char('A') => self.add_all_results(),
            KeyCode::Char(' ') => self.toggle_play_pause(),
            KeyCode::Char('n') => self.next_track(),
            KeyCode::Char('p') => self.previous_track(),
            KeyCode::Char('r') => self.cycle_repeat(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(5.0),
            KeyCode::Char('-') => self.change_volume(-5.0),
            KeyCode::Left => self.seek(-5.0),
            KeyCode::Right => self.seek(5.0),
            KeyCode::F(1) | KeyCode::Char('?') => self.help = true,
            KeyCode::F(3) | KeyCode::Char('#') => self.toggle_lyrics(),
            KeyCode::Char('@') => self.cycle_source(),
            KeyCode::Char('f') => self.cycle_quality_filter(),
            KeyCode::Char('Q') => self.sort_by_quality(),
            KeyCode::Char('T') => self.sort_by_duration(),
            KeyCode::Char('W') => self.toggle_high_quality_only(),
            KeyCode::Char('S') => self.open_save_playlist_prompt(),
            KeyCode::Char('s') => self.open_add_to_playlist(),
            KeyCode::Char('o') => self.open_playlist_picker(),
            KeyCode::Char('v') => self.toggle_visualizer(),
            _ => {}
        }
    }

    fn handle_queue_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc | KeyCode::Char('/') | KeyCode::Char('i') => {
                self.focus = Focus::Search;
            }
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.previous(),
            KeyCode::Down | KeyCode::Char('j') => self.move_queue(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_queue(-1),
            KeyCode::Char('g') => self.select_queue(0),
            KeyCode::Char('G') => {
                if !self.queue.is_empty() {
                    self.select_queue(self.queue.len() - 1);
                }
            }
            KeyCode::Enter => self.play_queue_index(self.queue_selected),
            KeyCode::Char('d') | KeyCode::Delete => self.remove_selected_queue_item(),
            KeyCode::Char('J') => self.reorder_queue(1),
            KeyCode::Char('K') => self.reorder_queue(-1),
            KeyCode::Char(' ') => self.toggle_play_pause(),
            KeyCode::Char('n') => self.next_track(),
            KeyCode::Char('p') => self.previous_track(),
            KeyCode::Char('r') => self.cycle_repeat(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(5.0),
            KeyCode::Char('-') => self.change_volume(-5.0),
            KeyCode::Left => self.seek(-5.0),
            KeyCode::Right => self.seek(5.0),
            KeyCode::F(1) | KeyCode::Char('?') => self.help = true,
            KeyCode::F(3) | KeyCode::Char('#') => self.toggle_lyrics(),
            KeyCode::Char('@') => self.cycle_source(),
            KeyCode::Char('f') => self.cycle_quality_filter(),
            KeyCode::Char('Q') => self.sort_by_quality(),
            KeyCode::Char('T') => self.sort_by_duration(),
            KeyCode::Char('W') => self.toggle_high_quality_only(),
            KeyCode::Char('S') => self.open_save_playlist_prompt(),
            KeyCode::Char('s') => self.open_add_queue_item_to_playlist(),
            KeyCode::Char('o') => self.open_playlist_picker(),
            KeyCode::Char('v') => self.toggle_visualizer(),
            _ => {}
        }
    }

    fn start_search(&mut self) {
        let query = self.input.value();
        let query = query.trim().to_string();
        if query.is_empty() {
            self.focus = Focus::Search;
            self.set_status("Enter something to search for.", true);
            return;
        }

        self.search_seq = self.search_seq.wrapping_add(1);
        let seq = self.search_seq;
        self.loading = true;
        self.all_results.clear();
        self.results.clear();
        self.result_selected = 0;
        // Keep focus on Search to allow typing while results load
        self.focus = Focus::Search;
        self.set_status(
            format!("Searching {} for \"{}\"…", self.source.label(), query),
            false,
        );

        let tx = self.search_tx.clone();
        let source = self.source;
        let spawn_result = thread::Builder::new().name("lookup".into()).spawn(move || {
            let result = ytdlp::lookup(&query, source, 15).map_err(|error| format!("{error:#}"));
            let _ = tx.send(SearchMessage { seq, result });
        });

        if let Err(error) = spawn_result {
            self.loading = false;
            self.set_status(format!("Could not start search: {error}"), true);
        }
    }

    fn handle_search_result(&mut self, result: Result<Lookup, String>) {
        self.loading = false;
        match result {
            Ok(lookup) => {
                let Lookup { tracks, note } = lookup;
                self.all_results = tracks;
                self.result_selected = 0;
                self.apply_quality_filter();

                let total = self.all_results.len();
                let shown = self.results.len();
                let mut message = if total == 0 {
                    "No results.".to_string()
                } else if shown == 0 {
                    format!(
                        "No results match the {} quality filter ({total} found). Press f to change the filter.",
                        self.quality_filter.label()
                    )
                } else if shown == total {
                    if shown == 1 {
                        "1 result. Press Enter to play it.".to_string()
                    } else {
                        format!("{shown} results. Press Enter to play the selection.")
                    }
                } else {
                    format!(
                        "Showing {shown} of {total} results (quality filter: {}). Press Enter to play the selection.",
                        self.quality_filter.label()
                    )
                };
                if let Some(note) = note {
                    message = format!("{note} {message}");
                }
                self.set_status(message, false);
            }
            Err(error) => {
                self.all_results.clear();
                self.results.clear();
                self.result_selected = 0;
                self.set_status(error, true);
            }
        }
    }

    fn move_result(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let current = self.result_selected.min(self.results.len() - 1);
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            (current + delta as usize).min(self.results.len() - 1)
        };
        self.result_selected = next;
    }

    fn select_result(&mut self, index: usize) {
        if !self.results.is_empty() {
            self.result_selected = index.min(self.results.len() - 1);
        }
    }

    fn move_queue(&mut self, delta: isize) {
        if self.queue.is_empty() {
            return;
        }
        let current = self.queue_selected.min(self.queue.len() - 1);
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            (current + delta as usize).min(self.queue.len() - 1)
        };
        self.queue_selected = next;
    }

    fn select_queue(&mut self, index: usize) {
        if !self.queue.is_empty() {
            self.queue_selected = index.min(self.queue.len() - 1);
        }
    }

    fn play_results_selected(&mut self) {
        if self.results.is_empty() {
            self.set_status("No search result selected.", true);
            return;
        }
        self.queue = self.results.clone();
        self.queue_selected = self.result_selected.min(self.queue.len() - 1);
        self.play_queue_index(self.queue_selected);
    }

    pub fn toggle_lyrics(&mut self) {
        if !self.lyrics_enabled {
            self.set_status("Lyrics are disabled by --no-lyrics.", true);
            return;
        }

        self.lyrics.visible = !self.lyrics.visible;
        if self.lyrics.visible {
            // Hydrate from cache or restart a fetch for the current track so
            // the panel is never blank when it is shown again.
            self.request_lyrics_for_current();
        }
    }

    fn request_lyrics_for_current(&mut self) {
        let Some(track) = self.current_track().cloned() else {
            return;
        };

        let track_url = track.url.clone();
        self.lyrics.track_url = Some(track_url.clone());
        self.lyrics.title = track.title.clone();
        self.lyrics.artist = track.artist.clone();
        self.lyrics.error = None;
        self.lyrics.instrumental = false;
        self.lyrics.source = None;

        if let Some(cached) = self.lyrics_cache.get(&track_url).cloned() {
            self.apply_lyrics_result(&cached);
            return;
        }

        if self.lyrics_pending.contains(&track_url) {
            self.lyrics.loading = true;
            return;
        }

        self.lyrics.loading = true;
        self.lyrics.lines = None;
        self.lyrics_pending.insert(track_url.clone());

        let tx = self.lyrics_tx.clone();
        let artist = track.artist.clone();
        let title = track.title.clone();
        let duration = track.duration;
        let spawn_result = thread::Builder::new().name("lyrics".into()).spawn(move || {
            let result = lyrics::fetch(&artist, &title, duration)
                .map_err(|error| format!("Could not fetch lyrics: {error:#}"));
            let _ = tx.send(LyricsMessage { track_url, result });
        });

        if let Err(error) = spawn_result {
            self.lyrics_pending.remove(&track.url);
            self.lyrics.loading = false;
            self.lyrics.error = Some(format!("Could not start lyrics fetch: {error}"));
        }
    }

    fn apply_lyrics_result(&mut self, result: &Result<Lyrics, String>) {
        match result {
            Ok(lyrics) => {
                self.lyrics.lines = Some(lyrics.clone());
                self.lyrics.loading = false;
                self.lyrics.error = None;
                self.lyrics.instrumental = lyrics.instrumental;
                self.lyrics.source = lyrics.source.clone();
            }
            Err(error) => {
                self.lyrics.lines = None;
                self.lyrics.loading = false;
                self.lyrics.error = Some(error.clone());
            }
        }
    }

    fn play_queue_index(&mut self, index: usize) {
        let Some(track) = self.queue.get(index).cloned() else {
            return;
        };

        self.current = Some(index);
        self.queue_selected = index;
        self.player.position = 0.0;
        self.player.duration = track.duration.unwrap_or(0.0);
        self.player.paused = false;

        let lyrics_visible = self.lyrics.visible;
        self.lyrics = LyricsState::default();
        self.lyrics.visible = lyrics_visible;
        // Lyrics follow playback automatically; fetch in the background even
        // while the panel is hidden so the cache is warm when it is shown.
        if self.lyrics_enabled {
            self.request_lyrics_for_current();
        }

        if self.mpv_closed {
            self.player.loading = false;
            self.set_status("The playback engine is not running.", true);
            return;
        }

        self.play_seq = self.play_seq.wrapping_add(1);
        let seq = self.play_seq;
        self.player.loading = true;
        self.set_status(
            format!("Resolving audio: {} — {}", track.title, track.artist),
            false,
        );

        // Stop whatever is currently playing while the new stream is resolved.
        let _ = self.mpv.stop();

        let tx = self.play_tx.clone();
        let track_url = track.url.clone();
        let spawn_result = thread::Builder::new()
            .name("extract".into())
            .spawn(move || {
                let result =
                    ytdlp::extract_stream(&track_url).map_err(|error| format!("{error:#}"));
                let _ = tx.send(PlayMessage {
                    seq,
                    track_url,
                    result,
                });
            });

        if let Err(error) = spawn_result {
            self.player.paused = true;
            self.reset_player();
            self.set_status(format!("Could not start audio extraction: {error}"), true);
        }
    }

    fn handle_play_result(&mut self, message: PlayMessage) {
        let matches_current = self
            .current_track()
            .is_some_and(|track| track.url == message.track_url);
        if !matches_current {
            return;
        }

        match message.result {
            Ok(stream) => {
                if self.mpv_closed {
                    self.player.loading = false;
                    self.player.paused = true;
                    self.set_status("The playback engine is not running.", true);
                    return;
                }
                if let Err(error) = self.mpv.load(&stream.url, &stream.headers) {
                    self.player.paused = true;
                    self.reset_player();
                    self.set_status(format!("Could not load track: {error:#}"), true);
                } else {
                    // Relaunching mpv for a different source resets its volume.
                    let _ = self.mpv.set_volume(self.player.volume);
                }
            }
            Err(error) => {
                self.player.paused = true;
                self.reset_player();
                self.set_status(format!("Could not resolve audio: {error}"), true);
            }
        }
    }

    fn add_selected_result(&mut self) {
        let Some(track) = self.results.get(self.result_selected).cloned() else {
            return;
        };
        self.queue.push(track.clone());
        self.set_status(format!("Queued: {}", track.title), false);
    }

    fn add_all_results(&mut self) {
        if self.results.is_empty() {
            return;
        }
        let count = self.results.len();
        self.queue.extend(self.results.iter().cloned());
        self.set_status(format!("Queued {count} result(s)."), false);
    }

    fn remove_selected_queue_item(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let index = self.queue_selected.min(self.queue.len() - 1);
        let was_current = self.current == Some(index);
        let removed = self.queue.remove(index);

        if was_current {
            self.play_seq = self.play_seq.wrapping_add(1);
            if !self.mpv_closed {
                let _ = self.mpv.stop();
            }
            self.reset_player();
            self.lyrics = LyricsState::default();
        } else if let Some(current) = self.current
            && index < current
        {
            self.current = Some(current - 1);
        }

        if !self.queue.is_empty() {
            self.queue_selected = index.min(self.queue.len() - 1);
        } else {
            self.queue_selected = 0;
        }
        self.set_status(format!("Removed: {}", removed.title), false);
    }

    fn reorder_queue(&mut self, delta: isize) {
        if self.queue.len() < 2 || self.queue.is_empty() {
            return;
        }
        let from = self.queue_selected.min(self.queue.len() - 1);
        let to = if delta < 0 {
            from.saturating_sub(delta.unsigned_abs())
        } else {
            (from + delta as usize).min(self.queue.len() - 1)
        };
        if from == to {
            return;
        }
        self.queue.swap(from, to);
        if self.current == Some(from) {
            self.current = Some(to);
        } else if self.current == Some(to) {
            self.current = Some(from);
        }
        self.queue_selected = to;
    }

    fn toggle_play_pause(&mut self) {
        if self.current.is_none() {
            match self.focus {
                Focus::Queue if !self.queue.is_empty() => {
                    self.play_queue_index(self.queue_selected);
                }
                Focus::Results if !self.results.is_empty() => self.play_results_selected(),
                _ if !self.results.is_empty() => self.play_results_selected(),
                _ if !self.queue.is_empty() => self.play_queue_index(self.queue_selected),
                _ => self.set_status("Nothing to play yet.", true),
            }
            return;
        }

        if self.mpv_closed {
            self.set_status("The playback engine is not running.", true);
            return;
        }
        match self.mpv.toggle_pause() {
            Ok(()) => self.player.paused = !self.player.paused,
            Err(error) => self.set_status(format!("Could not toggle playback: {error:#}"), true),
        }
    }

    fn next_track(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let index = self
            .current
            .unwrap_or(self.queue_selected)
            .min(self.queue.len() - 1);
        if index + 1 < self.queue.len() {
            self.play_queue_index(index + 1);
        } else if self.repeat == RepeatMode::All {
            self.play_queue_index(0);
        } else {
            self.set_status("End of queue.", false);
        }
    }

    fn previous_track(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.current.is_some() && self.player.position > 3.0 {
            self.seek(-self.player.position);
            return;
        }

        let index = self
            .current
            .unwrap_or(self.queue_selected)
            .min(self.queue.len() - 1);
        if index > 0 {
            self.play_queue_index(index - 1);
        } else if self.repeat == RepeatMode::All {
            self.play_queue_index(self.queue.len() - 1);
        } else if self.current.is_some() {
            self.seek(-self.player.position);
        }
    }

    fn seek(&mut self, delta: f64) {
        if self.current.is_none() || self.mpv_closed {
            return;
        }
        let target = (self.player.position + delta).max(0.0);
        let target = if self.player.duration > 0.0 {
            target.min(self.player.duration)
        } else {
            target
        };
        if let Err(error) = self.mpv.seek_relative(delta) {
            self.set_status(format!("Could not seek: {error:#}"), true);
            return;
        }
        self.player.position = target;
    }

    fn change_volume(&mut self, delta: f64) {
        let volume = (self.player.volume + delta).clamp(0.0, 100.0);
        let volume = (volume / 5.0).round() * 5.0;
        if self.mpv_closed {
            return;
        }
        match self.mpv.set_volume(volume) {
            Ok(()) => {
                self.player.volume = volume;
                self.prev_volume = volume.max(1.0);
            }
            Err(error) => self.set_status(format!("Could not set volume: {error:#}"), true),
        }
    }

    fn toggle_mute(&mut self) {
        if self.mpv_closed {
            return;
        }
        let volume = if self.player.volume > 0.0 {
            self.prev_volume = self.player.volume;
            0.0
        } else if self.prev_volume > 0.0 {
            self.prev_volume
        } else {
            70.0
        };
        match self.mpv.set_volume(volume) {
            Ok(()) => self.player.volume = volume,
            Err(error) => self.set_status(format!("Could not mute: {error:#}"), true),
        }
    }

    fn cycle_source(&mut self) {
        self.source = self.source.next();
        self.set_status(format!("Search source: {}", self.source.label()), false);
    }

    fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.next();
        self.set_status(format!("Repeat: {}", self.repeat.label()), false);
    }

    fn handle_mpv_event(&mut self, event: Value) {
        match event.get("event").and_then(Value::as_str) {
            Some("property-change") => {
                let name = event.get("name").and_then(Value::as_str).unwrap_or("");
                let value = event.get("data").unwrap_or(&Value::Null);
                match name {
                    "time-pos" => {
                        // Ignore stale events from the previous file while the
                        // next track is still being resolved.
                        if !self.player.loading
                            && let Some(position) = value.as_f64()
                        {
                            self.player.position = position;
                        }
                    }
                    "duration" => {
                        if !self.player.loading
                            && let Some(duration) = value.as_f64()
                        {
                            self.player.duration = duration;
                        }
                    }
                    "pause" => {
                        if let Some(paused) = value.as_bool() {
                            self.player.paused = paused;
                        }
                    }
                    "volume" => {
                        if let Some(volume) = value.as_f64() {
                            self.player.volume = volume;
                        }
                    }
                    _ => {}
                }
            }
            Some("start-file") => {
                self.player.loading = true;
            }
            Some("file-loaded") => {
                self.player.loading = false;
                self.player.paused = false;
                if let Some(track) = self.current_track() {
                    self.set_status(
                        format!("Playing: {} — {}", track.title, track.artist),
                        false,
                    );
                }
            }
            Some("end-file") => {
                let reason = event.get("reason").and_then(Value::as_str).unwrap_or("");
                match reason {
                    "eof" => self.handle_eof(),
                    "error" => {
                        self.player.loading = false;
                        self.player.paused = true;
                        self.current = None;
                        self.player.position = 0.0;
                        let detail = event
                            .get("file_error")
                            .and_then(Value::as_str)
                            .filter(|detail| !detail.is_empty());
                        let message = match detail {
                            Some(detail) => format!(
                                "Playback failed: {detail}. Check that yt-dlp is up to date and the track is available."
                            ),
                            None => "Playback failed. Check that yt-dlp is up to date and the track is available.".to_string(),
                        };
                        self.set_status(message, true);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn handle_eof(&mut self) {
        let Some(index) = self.current else {
            return;
        };
        if self.repeat == RepeatMode::One {
            self.play_queue_index(index);
            return;
        }
        if index + 1 < self.queue.len() {
            self.play_queue_index(index + 1);
        } else if self.repeat == RepeatMode::All && !self.queue.is_empty() {
            self.play_queue_index(0);
        } else {
            self.reset_player();
            self.set_status("Queue finished.", false);
        }
    }

    fn reset_player(&mut self) {
        self.current = None;
        self.player.position = 0.0;
        self.player.duration = 0.0;
        self.player.loading = false;
        self.player.paused = true;
        self.lyrics = LyricsState::default();
    }

    pub fn set_status(&mut self, message: impl Into<String>, is_error: bool) {
        self.status = message.into();
        self.status_is_error = is_error;
    }

    pub fn apply_quality_filter(&mut self) {
        self.rebuild_filtered_results();
        if !self.all_results.is_empty() {
            self.set_status(
                format!(
                    "Quality filter: {} ({} of {} results)",
                    self.quality_filter.label(),
                    self.results.len(),
                    self.all_results.len()
                ),
                false,
            );
        }
    }

    fn rebuild_filtered_results(&mut self) {
        self.results = self
            .all_results
            .iter()
            .filter(|track| self.quality_filter.matches(track.quality))
            .cloned()
            .collect();
        if self.results.is_empty() {
            self.result_selected = 0;
        } else {
            self.result_selected = self.result_selected.min(self.results.len() - 1);
        }
    }

    pub fn cycle_quality_filter(&mut self) {
        self.quality_filter = match self.quality_filter {
            QualityFilter::All => QualityFilter::Lossless,
            QualityFilter::Lossless => QualityFilter::High,
            QualityFilter::High => QualityFilter::Medium,
            QualityFilter::Medium => QualityFilter::Low,
            QualityFilter::Low => QualityFilter::All,
        };
        self.apply_quality_filter();
    }

    pub fn sort_by_quality(&mut self) {
        self.all_results
            .sort_by_key(|track| std::cmp::Reverse(track.quality_score()));
        self.rebuild_filtered_results();
        self.set_status(
            "Results sorted by quality (highest first)".to_string(),
            false,
        );
    }

    pub fn sort_by_duration(&mut self) {
        self.all_results.sort_by(|a, b| {
            a.duration
                .unwrap_or(0.0)
                .total_cmp(&b.duration.unwrap_or(0.0))
        });
        self.rebuild_filtered_results();
        self.set_status("Results sorted by duration".to_string(), false);
    }

    pub fn set_quality_preference(&mut self, quality: QualityFilter) {
        self.quality_filter = quality;
        self.apply_quality_filter();
    }

    pub fn toggle_high_quality_only(&mut self) {
        if self.quality_filter == QualityFilter::High
            || self.quality_filter == QualityFilter::Lossless
        {
            self.set_quality_preference(QualityFilter::All);
        } else {
            self.set_quality_preference(QualityFilter::High);
        }
    }

    // ----- Playlists ------------------------------------------------------

    /// `S`: prompt for a name and save the current queue as a playlist.
    pub fn open_save_playlist_prompt(&mut self) {
        if self.queue.is_empty() {
            self.set_status("Queue is empty — nothing to save.", true);
            return;
        }
        self.playlist_ui = PlaylistUi::NamePrompt {
            action: NameAction::SaveQueue,
            input: TextInput::default(),
        };
    }

    /// `o`: browse saved playlists.
    pub fn open_playlist_picker(&mut self) {
        let names = self.playlists.list();
        self.playlist_ui = PlaylistUi::Picker {
            mode: PickerMode::Open,
            level: PickerLevel::Playlists,
            selected: 0,
            names,
        };
    }

    /// `s` (results view): pick a playlist to add the selected result to.
    pub fn open_add_to_playlist(&mut self) {
        let Some(track) = self.results.get(self.result_selected).cloned() else {
            self.set_status("No track selected.", true);
            return;
        };
        self.open_add_to_playlist_with(track);
    }

    /// `s` (queue view): pick a playlist to add the selected queue item to.
    pub fn open_add_queue_item_to_playlist(&mut self) {
        let Some(track) = self.queue.get(self.queue_selected).cloned() else {
            self.set_status("Queue is empty.", true);
            return;
        };
        self.open_add_to_playlist_with(track);
    }

    fn open_add_to_playlist_with(&mut self, track: Track) {
        let names = self.playlists.list();
        self.playlist_ui = PlaylistUi::Picker {
            mode: PickerMode::AddTo { track },
            level: PickerLevel::Playlists,
            selected: 0,
            names,
        };
    }

    /// Queue `tracks` and start playing the first one (used by the
    /// `--playlist` startup flag).
    pub fn start_with_tracks(&mut self, tracks: Vec<Track>) {
        if tracks.is_empty() {
            return;
        }
        self.queue = tracks;
        self.queue_selected = 0;
        self.play_queue_index(0);
    }

    fn handle_playlist_key(&mut self, key: KeyEvent) {
        if matches!(self.playlist_ui, PlaylistUi::NamePrompt { .. }) {
            self.handle_playlist_name_key(key);
        } else if matches!(self.playlist_ui, PlaylistUi::Picker { .. }) {
            self.handle_playlist_picker_key(key);
        }
    }

    fn handle_playlist_name_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => {
                let (action, name) = match &self.playlist_ui {
                    PlaylistUi::NamePrompt { action, input } => (action.clone(), input.value()),
                    _ => return,
                };
                self.confirm_playlist_name(&action, &name);
            }
            KeyCode::Esc => self.playlist_ui = PlaylistUi::None,
            KeyCode::Backspace => {
                if let PlaylistUi::NamePrompt { input, .. } = &mut self.playlist_ui {
                    input.backspace();
                }
            }
            KeyCode::Delete => {
                if let PlaylistUi::NamePrompt { input, .. } = &mut self.playlist_ui {
                    input.delete();
                }
            }
            KeyCode::Left => {
                if let PlaylistUi::NamePrompt { input, .. } = &mut self.playlist_ui {
                    input.left();
                }
            }
            KeyCode::Right => {
                if let PlaylistUi::NamePrompt { input, .. } = &mut self.playlist_ui {
                    input.right();
                }
            }
            KeyCode::Home => {
                if let PlaylistUi::NamePrompt { input, .. } = &mut self.playlist_ui {
                    input.home();
                }
            }
            KeyCode::End => {
                if let PlaylistUi::NamePrompt { input, .. } = &mut self.playlist_ui {
                    input.end();
                }
            }
            KeyCode::Char(ch) if ctrl => {
                if let PlaylistUi::NamePrompt { input, .. } = &mut self.playlist_ui {
                    match ch.to_ascii_lowercase() {
                        'u' => input.clear(),
                        'w' => input.delete_word(),
                        'a' => input.home(),
                        'e' => input.end(),
                        _ => {}
                    }
                }
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::ALT) => {
                if let PlaylistUi::NamePrompt { input, .. } = &mut self.playlist_ui {
                    input.insert(ch);
                }
            }
            _ => {}
        }
    }

    fn confirm_playlist_name(&mut self, action: &NameAction, raw_name: &str) {
        let name = playlist::sanitize_name(raw_name);
        if name.is_empty() {
            self.set_status("Playlist name is empty.", true);
            return;
        }

        let result = match action {
            NameAction::SaveQueue => self
                .playlists
                .save(&name, &self.queue)
                .map(|_| format!("Saved \"{name}\" ({} tracks)", self.queue.len())),
            NameAction::Rename { old } => self.playlists.rename(old, &name).map(|renamed| {
                if renamed {
                    format!("Renamed to \"{name}\"")
                } else {
                    "Playlist not found.".to_string()
                }
            }),
        };

        match result {
            Ok(message) => {
                self.playlist_ui = PlaylistUi::None;
                self.set_status(message, false);
            }
            Err(error) => self.set_status(format!("Could not save playlist: {error:#}"), true),
        }
    }

    fn handle_playlist_picker_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return; // only the global Ctrl+C/Ctrl+Q quit applies here
        }

        let PlaylistUi::Picker {
            mode,
            level,
            selected,
            names,
        } = &mut self.playlist_ui
        else {
            return;
        };

        // Clone what actions need so the borrow on `self.playlist_ui` ends
        // before App methods are called.
        let mode = mode.clone();
        let level = level.clone();
        let index = *selected;
        let playlist_name = match &level {
            PickerLevel::Playlists => names.get(index).cloned(),
            PickerLevel::Tracks { playlist, .. } => Some(playlist.clone()),
        };

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if !names.is_empty() {
                    *selected = (*selected + 1).min(names.len() - 1);
                }
            }
            KeyCode::Home | KeyCode::Char('g') => *selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                if !names.is_empty() {
                    *selected = names.len() - 1;
                }
            }
            KeyCode::Esc => {
                if matches!(level, PickerLevel::Tracks { .. }) {
                    if let PlaylistUi::Picker {
                        level, selected, ..
                    } = &mut self.playlist_ui
                    {
                        *level = PickerLevel::Playlists;
                        *selected = 0;
                    }
                } else {
                    self.playlist_ui = PlaylistUi::None;
                }
            }
            KeyCode::Enter => match (&mode, &level, playlist_name) {
                (PickerMode::Open, PickerLevel::Playlists, Some(name)) => {
                    self.load_playlist(&name, 0);
                }
                (PickerMode::AddTo { track }, PickerLevel::Playlists, Some(name)) => {
                    self.add_track_to_playlist(&name, track);
                }
                (PickerMode::Open, PickerLevel::Tracks { .. }, Some(name)) => {
                    self.load_playlist(&name, index);
                }
                _ => {}
            },
            KeyCode::Char('a')
                if matches!(level, PickerLevel::Playlists) && matches!(mode, PickerMode::Open) =>
            {
                if let Some(name) = playlist_name {
                    self.append_playlist(&name);
                }
            }
            KeyCode::Char('e')
                if matches!(level, PickerLevel::Playlists) && matches!(mode, PickerMode::Open) =>
            {
                if let Some(name) = playlist_name {
                    self.open_playlist_tracks(&name);
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => match &level {
                PickerLevel::Playlists if matches!(mode, PickerMode::Open) => {
                    if let Some(name) = playlist_name {
                        self.delete_playlist(&name);
                    }
                }
                PickerLevel::Tracks { .. } => {
                    if let Some(name) = playlist_name {
                        self.remove_track_from_playlist(&name, index);
                    }
                }
                _ => {}
            },
            KeyCode::Char('r')
                if matches!(level, PickerLevel::Playlists) && matches!(mode, PickerMode::Open) =>
            {
                if let Some(name) = playlist_name {
                    self.open_rename_prompt(&name);
                }
            }
            _ => {}
        }
    }

    fn refresh_picker(&mut self) {
        if let PlaylistUi::Picker {
            names, selected, ..
        } = &mut self.playlist_ui
        {
            *names = self.playlists.list();
            *selected = if names.is_empty() {
                0
            } else {
                (*selected).min(names.len() - 1)
            };
        }
    }

    fn open_rename_prompt(&mut self, old: &str) {
        let mut input = TextInput::default();
        input.insert_str(old);
        self.playlist_ui = PlaylistUi::NamePrompt {
            action: NameAction::Rename {
                old: old.to_string(),
            },
            input,
        };
    }

    fn open_playlist_tracks(&mut self, name: &str) {
        let tracks = match self.playlists.load(name) {
            Ok(Some(Playlist { tracks, .. })) => tracks,
            _ => Vec::new(),
        };
        self.playlist_ui = PlaylistUi::Picker {
            mode: PickerMode::Open,
            level: PickerLevel::Tracks {
                playlist: playlist::sanitize_name(name),
                tracks,
            },
            selected: 0,
            names: self.playlists.list(),
        };
    }

    fn load_playlist(&mut self, name: &str, start: usize) {
        match self.playlists.load(name) {
            Ok(Some(Playlist { tracks, .. })) if !tracks.is_empty() => {
                let start = start.min(tracks.len() - 1);
                self.queue = tracks;
                self.queue_selected = start;
                self.playlist_ui = PlaylistUi::None;
                self.set_status(format!("Playing \"{name}\""), false);
                self.play_queue_index(start);
            }
            Ok(Some(_)) => self.set_status("Playlist is empty.", true),
            Ok(None) => self.set_status("Playlist not found.", true),
            Err(error) => self.set_status(format!("Could not load playlist: {error:#}"), true),
        }
    }

    fn append_playlist(&mut self, name: &str) {
        match self.playlists.load(name) {
            Ok(Some(Playlist { tracks, .. })) if !tracks.is_empty() => {
                let added = tracks.len();
                self.queue.extend(tracks);
                self.playlist_ui = PlaylistUi::None;
                self.set_status(format!("Appended {added} tracks from \"{name}\""), false);
            }
            Ok(Some(_)) => self.set_status("Playlist is empty.", true),
            Ok(None) => self.set_status("Playlist not found.", true),
            Err(error) => self.set_status(format!("Could not load playlist: {error:#}"), true),
        }
    }

    fn add_track_to_playlist(&mut self, name: &str, track: &Track) {
        match self.playlists.add_track(name, track) {
            Ok(true) => {
                self.playlist_ui = PlaylistUi::None;
                self.set_status(
                    format!("Added \"{}\" to \"{name}\"", truncate_title(track)),
                    false,
                );
            }
            Ok(false) => self.set_status("Playlist not found.", true),
            Err(error) => self.set_status(format!("Could not update playlist: {error:#}"), true),
        }
    }

    fn delete_playlist(&mut self, name: &str) {
        match self.playlists.delete(name) {
            Ok(true) => {
                self.set_status(format!("Deleted \"{name}\""), false);
                self.refresh_picker();
            }
            Ok(false) => self.set_status("Playlist not found.", true),
            Err(error) => self.set_status(format!("Could not delete playlist: {error:#}"), true),
        }
    }

    fn remove_track_from_playlist(&mut self, name: &str, index: usize) {
        match self.playlists.remove_track(name, index) {
            Ok(true) => {
                if let PlaylistUi::Picker {
                    level: PickerLevel::Tracks { tracks, .. },
                    selected,
                    ..
                } = &mut self.playlist_ui
                {
                    if index < tracks.len() {
                        tracks.remove(index);
                    }
                    *selected = if tracks.is_empty() {
                        0
                    } else {
                        (*selected).min(tracks.len() - 1)
                    };
                }
                self.set_status(format!("Removed track from \"{name}\""), false);
            }
            Ok(false) => self.set_status("Track not found.", true),
            Err(error) => self.set_status(format!("Could not update playlist: {error:#}"), true),
        }
    }
}

fn truncate_title(track: &Track) -> String {
    let title = &track.title;
    if title.chars().count() > 40 {
        let truncated: String = title.chars().take(37).collect();
        format!("{truncated}...")
    } else {
        title.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Quality, Source};

    struct FakePlayer;

    impl Player for FakePlayer {
        fn try_recv(&self) -> std::result::Result<MpvMessage, TryRecvError> {
            Err(TryRecvError::Empty)
        }

        fn load(&mut self, _url: &str, _headers: &[(String, String)]) -> anyhow::Result<()> {
            Ok(())
        }

        fn toggle_pause(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn seek_relative(&mut self, _seconds: f64) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_volume(&mut self, _volume: f64) -> anyhow::Result<()> {
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn test_app() -> App {
        App::with_player(Box::new(FakePlayer), true, false)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn track(quality: Quality) -> Track {
        Track::new(
            "Song".into(),
            "Artist".into(),
            Some(180.0),
            "https://example.com/a".into(),
            Source::YouTube,
            quality,
        )
    }

    #[test]
    fn search_box_types_shortcut_letters_as_text() {
        let mut app = test_app();
        for ch in ['q', '@', '#', '?', 'f'] {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(app.input.value(), "q@#?f");
        assert!(!app.should_quit);
        assert!(!app.help);
        // The panel is visible by default; typing `#` in the search box must
        // not toggle it.
        assert!(app.lyrics.visible);
        assert_eq!(app.source, SearchSource::YouTube);
        assert_eq!(app.quality_filter, QualityFilter::All);
    }

    #[test]
    fn ctrl_c_quits_from_search_box() {
        let mut app = test_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
        assert!(app.input.is_empty());
    }

    #[test]
    fn q_quits_from_results_view() {
        let mut app = test_app();
        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn filter_shortcut_cycles_quality_filter_in_results_view() {
        let mut app = test_app();
        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Char('f')));
        assert_eq!(app.quality_filter, QualityFilter::Lossless);
        assert!(app.input.is_empty());
    }

    #[test]
    fn toggle_lyrics_switches_panel_visibility() {
        let mut app = test_app();
        assert!(app.lyrics.visible);

        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Char('#')));
        assert!(!app.lyrics.visible);

        app.handle_key(key(KeyCode::F(3)));
        assert!(app.lyrics.visible);
    }

    #[test]
    fn toggle_lyrics_is_rejected_when_disabled() {
        let mut app = App::with_player(Box::new(FakePlayer), false, false);
        assert!(!app.lyrics.visible);
        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Char('#')));
        assert!(!app.lyrics.visible);
        assert!(app.status_is_error);
    }

    #[test]
    fn lyrics_status_reflects_fetch_outcomes() {
        use crate::lyrics::LyricLine;

        let mut state = LyricsState::default();
        assert_eq!(state.status(), LyricsStatus::Idle);

        state.loading = true;
        assert_eq!(state.status(), LyricsStatus::Loading);

        state.loading = false;
        state.error = Some("network down".into());
        assert_eq!(state.status(), LyricsStatus::Unavailable);
        state.error = None;

        state.lines = Some(Lyrics::default());
        assert_eq!(state.status(), LyricsStatus::Unavailable);

        state.instrumental = true;
        assert_eq!(state.status(), LyricsStatus::Instrumental);
        state.instrumental = false;

        state.lines = Some(Lyrics {
            synced: Some(vec![LyricLine {
                time: 0.0,
                text: "la".into(),
            }]),
            ..Lyrics::default()
        });
        assert_eq!(state.status(), LyricsStatus::Ready);
    }

    #[test]
    fn quality_filter_narrows_view_without_dropping_results() {
        let mut app = test_app();
        app.all_results = vec![
            track(Quality::Lossless),
            track(Quality::High),
            track(Quality::Medium),
        ];
        app.apply_quality_filter();
        assert_eq!(app.results.len(), 3);

        app.quality_filter = QualityFilter::High;
        app.apply_quality_filter();
        assert_eq!(app.results.len(), 2);
        assert_eq!(app.all_results.len(), 3);

        app.cycle_quality_filter();
        assert_eq!(app.quality_filter, QualityFilter::Medium);
        app.apply_quality_filter();
        assert_eq!(app.results.len(), 3);
        assert_eq!(app.all_results.len(), 3);
    }

    fn test_app_with_playlist_dir(tag: &str) -> (App, std::path::PathBuf) {
        let mut app = test_app();
        let dir =
            std::env::temp_dir().join(format!("tuimu-app-playlists-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        app.playlists = PlaylistStore::open(dir.clone(), "test".into());
        (app, dir)
    }

    #[test]
    fn save_queue_prompt_saves_playlist() {
        let (mut app, dir) = test_app_with_playlist_dir("save");
        app.queue = vec![track(Quality::High), track(Quality::Low)];

        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Char('S')));
        assert!(matches!(app.playlist_ui, PlaylistUi::NamePrompt { .. }));

        for ch in "Road Trip".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(app.playlist_ui, PlaylistUi::None));
        assert!(!app.status_is_error);
        let saved = playlist::load(&dir, "Road Trip")
            .expect("load")
            .expect("saved");
        assert_eq!(saved.tracks.len(), 2);
    }

    #[test]
    fn save_prompt_rejects_empty_queue() {
        let (mut app, _dir) = test_app_with_playlist_dir("empty-queue");
        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Char('S')));
        assert!(matches!(app.playlist_ui, PlaylistUi::None));
        assert!(app.status_is_error);
    }

    #[test]
    fn playlist_prompt_keys_do_not_reach_search() {
        let (mut app, _dir) = test_app_with_playlist_dir("routing");
        app.queue = vec![track(Quality::High)];
        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Char('S')));
        for ch in "mix".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        assert!(app.input.is_empty());
        assert!(matches!(
            &app.playlist_ui,
            PlaylistUi::NamePrompt { input, .. } if input.value() == "mix"
        ));

        // Global quit still works while the prompt is open.
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn add_to_playlist_picker_adds_track() {
        let (mut app, dir) = test_app_with_playlist_dir("add");
        playlist::save(&dir, "favs", &[]).expect("save");
        app.results = vec![track(Quality::High)];
        app.result_selected = 0;

        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Char('s')));
        assert!(matches!(
            &app.playlist_ui,
            PlaylistUi::Picker { mode: PickerMode::AddTo { .. }, names, .. } if names.len() == 1
        ));

        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.playlist_ui, PlaylistUi::None));
        assert!(!app.status_is_error);
        let saved = playlist::load(&dir, "favs").expect("load").expect("saved");
        assert_eq!(saved.tracks.len(), 1);
    }

    #[test]
    fn s_from_queue_adds_selected_item_to_playlist() {
        let (mut app, dir) = test_app_with_playlist_dir("queue-add");
        playlist::save(&dir, "favs", &[track(Quality::High)]).expect("save");
        app.queue = vec![track(Quality::High), track(Quality::Low)];
        app.queue_selected = 1; // cursor on the second track

        app.focus = Focus::Queue;
        app.handle_key(key(KeyCode::Char('s')));
        assert!(matches!(
            &app.playlist_ui,
            PlaylistUi::Picker { mode: PickerMode::AddTo { .. }, names, .. } if names.len() == 1
        ));

        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.playlist_ui, PlaylistUi::None));
        assert!(!app.status_is_error);
        let saved = playlist::load(&dir, "favs").expect("load").expect("saved");
        assert_eq!(saved.tracks.len(), 2);
    }

    #[test]
    fn s_from_empty_queue_shows_error() {
        let (mut app, _dir) = test_app_with_playlist_dir("queue-empty");
        app.focus = Focus::Queue;
        app.handle_key(key(KeyCode::Char('s')));
        assert!(matches!(app.playlist_ui, PlaylistUi::None));
        assert!(app.status_is_error);
    }

    #[test]
    fn picker_navigates_and_closes() {
        let (mut app, dir) = test_app_with_playlist_dir("navigate");
        playlist::save(&dir, "a-mix", &[track(Quality::High)]).expect("save");
        playlist::save(&dir, "b-mix", &[track(Quality::Low)]).expect("save");

        app.focus = Focus::Queue;
        app.handle_key(key(KeyCode::Char('o')));
        assert!(matches!(
            &app.playlist_ui,
            PlaylistUi::Picker { names, selected: 0, .. } if names.len() == 2
        ));

        app.handle_key(key(KeyCode::Char('j')));
        assert!(matches!(
            &app.playlist_ui,
            PlaylistUi::Picker { selected: 1, .. }
        ));

        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.playlist_ui, PlaylistUi::None));
    }

    #[test]
    fn picker_deletes_playlist() {
        let (mut app, dir) = test_app_with_playlist_dir("delete");
        playlist::save(&dir, "gone", &[track(Quality::High)]).expect("save");

        app.focus = Focus::Queue;
        app.handle_key(key(KeyCode::Char('o')));
        app.handle_key(key(KeyCode::Char('d')));

        assert!(matches!(
            &app.playlist_ui,
            PlaylistUi::Picker { names, .. } if names.is_empty()
        ));
        assert!(playlist::load(&dir, "gone").expect("load").is_none());
    }

    #[test]
    fn picker_loads_playlist_into_queue() {
        let (mut app, dir) = test_app_with_playlist_dir("load");
        playlist::save(&dir, "mix", &[track(Quality::High), track(Quality::Low)]).expect("save");

        app.focus = Focus::Queue;
        app.handle_key(key(KeyCode::Char('o')));
        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(app.playlist_ui, PlaylistUi::None));
        assert_eq!(app.queue.len(), 2);
        assert_eq!(app.current, Some(0));
        assert!(!app.status_is_error);
    }

    #[test]
    fn picker_edits_and_removes_tracks() {
        let (mut app, dir) = test_app_with_playlist_dir("edit");
        playlist::save(&dir, "mix", &[track(Quality::High), track(Quality::Low)]).expect("save");

        app.focus = Focus::Queue;
        app.handle_key(key(KeyCode::Char('o')));
        app.handle_key(key(KeyCode::Char('e')));
        assert!(matches!(
            &app.playlist_ui,
            PlaylistUi::Picker { level: PickerLevel::Tracks { tracks, .. }, .. } if tracks.len() == 2
        ));

        app.handle_key(key(KeyCode::Char('d')));
        let saved = playlist::load(&dir, "mix").expect("load").expect("saved");
        assert_eq!(saved.tracks.len(), 1);

        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(
            &app.playlist_ui,
            PlaylistUi::Picker {
                level: PickerLevel::Playlists,
                ..
            }
        ));
    }

    #[test]
    fn rename_prompt_renames_playlist() {
        let (mut app, dir) = test_app_with_playlist_dir("rename");
        playlist::save(&dir, "old", &[track(Quality::High)]).expect("save");

        app.focus = Focus::Queue;
        app.handle_key(key(KeyCode::Char('o')));
        app.handle_key(key(KeyCode::Char('r')));
        // The prompt is prefilled with "old"; replace it with "new".
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for ch in "new".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(app.playlist_ui, PlaylistUi::None));
        assert!(playlist::load(&dir, "old").expect("load").is_none());
        assert_eq!(
            playlist::load(&dir, "new")
                .expect("load")
                .expect("saved")
                .tracks
                .len(),
            1
        );
    }

    #[test]
    fn start_with_tracks_queues_and_plays() {
        let mut app = test_app();
        app.start_with_tracks(vec![track(Quality::High), track(Quality::Low)]);
        assert_eq!(app.queue.len(), 2);
        assert_eq!(app.current, Some(0));

        app.start_with_tracks(Vec::new());
        assert_eq!(app.queue.len(), 2);
    }

    #[test]
    fn v_toggles_visualizer_panel() {
        let mut app = App::with_player(Box::new(FakePlayer), true, true);
        assert!(app.viz_visible);

        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Char('v')));
        assert!(!app.viz_visible);

        app.handle_key(key(KeyCode::Char('v')));
        assert!(app.viz_visible);
    }

    #[test]
    fn v_reports_disabled_visualizer() {
        let mut app = test_app(); // viz disabled
        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Char('v')));
        assert!(!app.viz_visible);
        assert!(app.status_is_error);
    }

    #[test]
    fn visualizer_bars_render_without_panicking() {
        let mut app = App::with_player(Box::new(FakePlayer), true, true);
        app.viz_bars = (0..BANDS)
            .map(|i| (i as f32 + 2.0) / (BANDS as f32 + 2.0))
            .collect();

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| crate::ui::draw(frame, &app))
            .expect("draw with both panels");

        // Lyrics hidden: visualizer takes the full right column.
        app.lyrics.visible = false;
        terminal
            .draw(|frame| crate::ui::draw(frame, &app))
            .expect("draw viz only");

        // Tiny terminal must not panic either.
        let mut tiny =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).expect("terminal");
        tiny.draw(|frame| crate::ui::draw(frame, &app))
            .expect("draw tiny");
    }

    #[test]
    fn playlist_popups_render_without_panicking() {
        let (mut app, _dir) = test_app_with_playlist_dir("render");
        playlist::save(app.playlists.dir(), "mix", &[track(Quality::High)]).expect("save");
        app.focus = Focus::Queue;

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).expect("terminal");

        app.handle_key(key(KeyCode::Char('o')));
        terminal
            .draw(|frame| crate::ui::draw(frame, &app))
            .expect("draw picker");

        app.handle_key(key(KeyCode::Char('e')));
        terminal
            .draw(|frame| crate::ui::draw(frame, &app))
            .expect("draw tracks");

        app.handle_key(key(KeyCode::Esc));
        app.queue = vec![track(Quality::High)];
        app.handle_key(key(KeyCode::Char('S')));
        for ch in "My Mix".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        terminal
            .draw(|frame| crate::ui::draw(frame, &app))
            .expect("draw name prompt");
    }
}
