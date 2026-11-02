use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::input::TextInput;
use crate::lyrics::{self, Lyrics};
use crate::model::{Quality, QualityFilter, RepeatMode, SearchSource, Track};
use crate::mpv::{Mpv, MpvMessage};
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
    pub open: bool,
    pub track_url: Option<String>,
    pub title: String,
    pub artist: String,
    pub lines: Option<Lyrics>,
    pub loading: bool,
    pub error: Option<String>,
    pub scroll: usize,
    pub follow: bool,
    pub instrumental: bool,
    pub source: Option<String>,
    pub is_indonesian: bool,
}

pub struct App {
    pub should_quit: bool,
    pub focus: Focus,
    pub input: TextInput,
    pub source: SearchSource,
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
    pub quality_filter: QualityFilter,
    // Quality cache to avoid re-computing quality for the same tracks
    quality_cache: HashMap<String, Quality>,
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
    mpv: Mpv,
    mpv_closed: bool,
}

impl App {
    pub fn new(mpv: Mpv, _lyrics_enabled: bool) -> Self {
        let (search_tx, search_rx) = mpsc::channel();
        let (play_tx, play_rx) = mpsc::channel();
        let (lyrics_tx, lyrics_rx) = mpsc::channel();
        Self {
            should_quit: false,
            focus: Focus::Search,
            input: TextInput::default(),
            source: SearchSource::YouTube,
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
            lyrics: LyricsState::default(),
            lyrics_enabled: true,
            quality_filter: QualityFilter::High, // Default to high quality
            quality_cache: HashMap::new(),
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
            if self.lyrics.track_url.as_deref() == Some(&message.track_url)
                || self.current_track_url() == Some(&message.track_url)
            {
                self.lyrics_pending.remove(&message.track_url);
                self.lyrics_cache.insert(message.track_url.clone(), message.result.clone());
                if self.lyrics.open && self.lyrics.track_url.as_deref() == Some(&message.track_url) {
                    match &message.result {
                        Ok(lyrics) => {
                            self.lyrics.lines = Some(lyrics.clone());
                            self.lyrics.loading = false;
                            self.lyrics.error = None;
                            self.lyrics.instrumental = lyrics.instrumental;
                            self.lyrics.source = lyrics.source.clone();
                            self.lyrics.is_indonesian = lyrics.is_indonesian;
                            if self.lyrics.follow {
                                self.scroll_to_current_lyric();
                            }
                        }
                        Err(error) => {
                            self.lyrics.lines = None;
                            self.lyrics.loading = false;
                            self.lyrics.error = Some(error.clone());
                        }
                    }
                }
            } else {
                self.lyrics_pending.remove(&message.track_url);
                self.lyrics_cache.insert(message.track_url.clone(), message.result);
            }
        }

        if self.lyrics.open && self.lyrics.follow {
            self.scroll_to_current_lyric();
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
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.focus == Focus::Search && !self.help {
            let text = text.replace(['\r', '\n'], " ");
            self.input.insert_str(text.trim());
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Handle help key globally - highest priority
        if key.code == KeyCode::Char('?') {
            self.help = !self.help;
            return;
        }
        
        if key.code == KeyCode::Char('@') {
            self.source = self.source.next();
            self.set_status(format!("Search source: {}", self.source.label()), false);
            return;
        }
        
        if key.code == KeyCode::Char('#') {
            self.toggle_lyrics();
            return;
        }
        
        if key.code == KeyCode::Char('q') {
            self.cycle_quality_filter();
            return;
        }

        // Handle Ctrl+C and Ctrl+Q for quitting
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
            self.should_quit = true;
            return;
        }

        // Handle help window
        if self.help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
            ) {
                self.help = false;
            }
            return;
        }

        // Handle lyrics window
        if self.lyrics.open {
            self.handle_lyrics_key(key);
            return;
        }

        // Handle focus-specific keys
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
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('#') => self.toggle_lyrics(),
            KeyCode::Char('Q') => self.sort_by_quality(),
            KeyCode::Char('T') => self.sort_by_duration(),
            KeyCode::Char('W') => self.toggle_high_quality_only(),
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
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('#') => self.toggle_lyrics(),
            KeyCode::Char('Q') => self.sort_by_quality(),
            KeyCode::Char('T') => self.sort_by_duration(),
            KeyCode::Char('W') => self.toggle_high_quality_only(),
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
                let count = lookup.tracks.len();
                self.results = lookup.tracks;
                self.apply_quality_filter();
                self.result_selected = 0;
                let mut message = if count == 0 {
                    "No results.".to_string()
                } else if count == 1 {
                    "1 result. Press Enter to play it.".to_string()
                } else {
                    format!("{count} results. Press Enter to play the selection.")
                };
                if let Some(note) = lookup.note {
                    message = format!("{note} {message}");
                }
                self.set_status(message, false);
            }
            Err(error) => {
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
            self.set_status("Lyrics are disabled. Enable them in the configuration.", true);
            return;
        }

        if self.lyrics.open {
            self.lyrics.open = false;
            return;
        }

        // Open lyrics window even if no track is playing
        self.lyrics.open = true;
        self.lyrics.loading = false;
        self.lyrics.error = None;
        self.lyrics.scroll = 0;
        self.lyrics.follow = true;
        
        if let Some(track) = self.current_track() {
            let track_url = track.url.clone();
            let title = track.title.clone();
            let artist = track.artist.clone();
            let duration = track.duration;

            let needs_fetch = !self.lyrics_pending.contains(&track_url) && !self.lyrics_cache.contains_key(&track_url);
            if needs_fetch {
                self.lyrics_pending.insert(track_url.clone());

                self.lyrics.loading = true;
                self.lyrics.error = None;
                self.lyrics.title = title.clone();
                self.lyrics.artist = artist.clone();
                self.lyrics.track_url = Some(track_url.clone());

                let tx = self.lyrics_tx.clone();
                thread::Builder::new()
                    .name("lyrics".into())
                    .spawn(move || {
                        let result = lyrics::fetch(&artist, &title, duration)
                            .map_err(|error| format!("Could not fetch lyrics: {error:#}"));
                        let _ = tx.send(LyricsMessage {
                            track_url,
                            result,
                        });
                    })
                    .unwrap();
            }
            self.lyrics.open = true;
            self.lyrics.follow = true;
            self.lyrics.scroll = 0;
        } else {
            self.set_status("No track playing.", true);
        }
    }

    fn handle_lyrics_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.lyrics.open = false;
            }
            KeyCode::Char(' ') => {
                self.lyrics.follow = !self.lyrics.follow;
                if self.lyrics.follow {
                    self.scroll_to_current_lyric();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.lyrics.scroll = self.lyrics.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(lines) = &self.lyrics.lines {
                    self.lyrics.scroll = (self.lyrics.scroll + 1).min(lines.line_count().saturating_sub(1));
                }
            }
            KeyCode::PageUp => {
                self.lyrics.scroll = self.lyrics.scroll.saturating_sub(5);
            }
            KeyCode::PageDown => {
                if let Some(lines) = &self.lyrics.lines {
                    self.lyrics.scroll = (self.lyrics.scroll + 5).min(lines.line_count().saturating_sub(1));
                }
            }
            KeyCode::Home => {
                self.lyrics.scroll = 0;
            }
            KeyCode::End => {
                if let Some(lines) = &self.lyrics.lines {
                    self.lyrics.scroll = lines.line_count().saturating_sub(1);
                }
            }
            KeyCode::Char('c') if ctrl => {
                self.lyrics.open = false;
            }
            _ => {}
        }
    }

    fn scroll_to_current_lyric(&mut self) {
        if !self.lyrics.follow || self.lyrics.lines.is_none() {
            return;
        }
        if let (Some(lines), Some(_current)) = (&self.lyrics.lines, self.current_track()) {
            if let Some(line_index) = lines.current_line(self.player.position) {
                let half = self.lyrics_scroll_height() / 2;
                if line_index < self.lyrics.scroll {
                    self.lyrics.scroll = line_index;
                } else if line_index >= self.lyrics.scroll + half {
                    self.lyrics.scroll = (line_index + 1).saturating_sub(half);
                }
            }
        }
    }

    fn lyrics_scroll_height(&self) -> usize {
        10
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
        self.lyrics = LyricsState::default();
        self.lyrics.track_url = Some(track.url.clone());
        self.lyrics.title = track.title.clone();
        self.lyrics.artist = track.artist.clone();

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
        let original_count = self.results.len();
        let before_filter = self.quality_filter;
        
        // Optimize for large result sets
        if original_count > 1000 {
            // For very large sets, use a more efficient approach
            let filtered: Vec<Track> = self.results
                .iter()
                .filter(|track| match self.quality_filter {
                    QualityFilter::All => true,
                    QualityFilter::Lossless => track.quality == Quality::Lossless,
                    QualityFilter::High => track.quality >= Quality::High,
                    QualityFilter::Medium => track.quality >= Quality::Medium,
                    QualityFilter::Low => track.quality >= Quality::Low,
                })
                .cloned()
                .collect();
            self.results = filtered;
        } else {
            // For smaller sets, use retain for better performance
            self.results.retain(|track| match self.quality_filter {
                QualityFilter::All => true,
                QualityFilter::Lossless => track.quality == Quality::Lossless,
                QualityFilter::High => track.quality >= Quality::High,
                QualityFilter::Medium => track.quality >= Quality::Medium,
                QualityFilter::Low => track.quality >= Quality::Low,
            });
        }
        
        if self.results.len() < original_count {
            let _filtered_count = original_count - self.results.len();
            let quality_name = before_filter.label();
            
            self.set_status(
                format!(
                    "Filtered: {} of {} tracks (quality: {})",
                    self.results.len(),
                    original_count,
                    quality_name
                ),
                false
            );
            
            // Show detailed quality breakdown only for smaller sets
            if original_count <= 100 {
                let quality_breakdown = self.get_quality_breakdown();
                if !quality_breakdown.is_empty() {
                    self.set_status(
                        format!("Quality breakdown: {}", quality_breakdown),
                        false
                    );
                }
            }
            
            // Adjust selected index if it's out of bounds
            if self.result_selected >= self.results.len() {
                self.result_selected = self.results.len().saturating_sub(1);
            }
        }
    }

    fn get_quality_breakdown(&self) -> String {
        let mut breakdown = std::collections::HashMap::new();
        
        for track in &self.results {
            *breakdown.entry(track.quality).or_insert(0) += 1;
        }
        
        let mut breakdown: Vec<_> = breakdown.into_iter().collect();
        breakdown.sort_by(|a, b| b.0.cmp(&a.0));
        
        breakdown
            .into_iter()
            .map(|(quality, count)| format!("{}: {}", quality.label(), count))
            .collect::<Vec<_>>()
            .join(", ")
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

    pub fn get_quality_filter(&self) -> QualityFilter {
        self.quality_filter
    }

    pub fn sort_by_quality(&mut self) {
        self.results.sort_by(|a, b| b.quality_score().cmp(&a.quality_score()));
        self.set_status("Results sorted by quality (highest first)".to_string(), false);
    }

    pub fn sort_by_duration(&mut self) {
        self.results.sort_by(|a, b| {
            a.duration.unwrap_or(0.0).total_cmp(&b.duration.unwrap_or(0.0))
        });
        self.set_status("Results sorted by duration".to_string(), false);
    }

    pub fn set_quality_preference(&mut self, quality: QualityFilter) {
        self.quality_filter = quality;
        self.apply_quality_filter();
        self.set_status(
            format!("Quality preference set to {}", quality.label()),
            false
        );
    }

    pub fn toggle_high_quality_only(&mut self) {
        if self.quality_filter == QualityFilter::High || self.quality_filter == QualityFilter::Lossless {
            self.set_quality_preference(QualityFilter::All);
        } else {
            self.set_quality_preference(QualityFilter::High);
        }
    }
}
