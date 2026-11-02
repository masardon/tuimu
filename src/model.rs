use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    YouTube,
    SoundCloud,
    Bandcamp,
    Spotify,
    Web,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::YouTube => "YouTube",
            Self::SoundCloud => "SoundCloud",
            Self::Bandcamp => "Bandcamp",
            Self::Spotify => "Spotify",
            Self::Web => "Web",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            Self::YouTube => "YT",
            Self::SoundCloud => "SC",
            Self::Bandcamp => "BC",
            Self::Spotify => "SP",
            Self::Web => "WWW",
        }
    }

    pub fn from_url(url: &str) -> Self {
        let url = url.to_ascii_lowercase();
        if url.contains("youtube.com")
            || url.contains("youtu.be")
            || url.contains("music.youtube.com")
        {
            Self::YouTube
        } else if url.contains("soundcloud.com") {
            Self::SoundCloud
        } else if url.contains("bandcamp.com") {
            Self::Bandcamp
        } else if url.contains("spotify.com") {
            Self::Spotify
        } else {
            Self::Web
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchSource {
    YouTube,
    SoundCloud,
}

impl SearchSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::YouTube => "YouTube",
            Self::SoundCloud => "SoundCloud",
        }
    }

    pub fn search_prefix(self) -> &'static str {
        match self {
            Self::YouTube => "ytsearch",
            Self::SoundCloud => "scsearch",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::YouTube => Self::SoundCloud,
            Self::SoundCloud => Self::YouTube,
        }
    }
}

impl fmt::Display for SearchSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Clone, Debug)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub duration: Option<f64>,
    pub url: String,
    pub source: Source,
}

impl Track {
    pub fn duration_label(&self) -> String {
        self.duration
            .map(format_duration)
            .unwrap_or_else(|| "--:--".into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::All => "all",
            Self::One => "one",
        }
    }
}

pub fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "--:--".into();
    }
    let total = seconds.round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

pub fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 1 {
        return "…".into();
    }
    let mut out: String = value.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
