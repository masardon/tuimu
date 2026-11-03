use std::collections::HashMap;
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
    pub quality: Quality,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Quality {
    Low,     // < 128kbps, poor quality
    Medium,  // 128-256kbps, standard quality
    High,    // 256-320kbps, good quality
    Lossless, // FLAC, etc., lossless quality
}

impl Quality {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Lossless => "Lossless",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityFilter {
    All,
    Lossless,
    High,
    Medium,
    Low,
}

impl QualityFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Lossless => "Lossless",
            Self::High => "High",
            Self::Medium => "Medium",
            Self::Low => "Low",
        }
    }
}

impl Track {
    pub fn duration_label(&self) -> String {
        self.duration
            .map(format_duration)
            .unwrap_or_else(|| "--:--".into())
    }

    pub fn quality_label(&self) -> &'static str {
        match self.quality {
            Quality::Low => "Low",
            Quality::Medium => "Medium",
            Quality::High => "High",
            Quality::Lossless => "Lossless",
        }
    }

    pub fn quality_score(&self) -> u8 {
        match self.quality {
            Quality::Low => 1,
            Quality::Medium => 5,
            Quality::High => 8,
            Quality::Lossless => 10,
        }
    }

    pub fn detect_quality(title: &str, source: Source) -> Quality {
        let title_lower = title.to_lowercase();
        
        // Check for lossless indicators
        if title_lower.contains("flac")
            || title_lower.contains("wav")
            || title_lower.contains("alac")
            || title_lower.contains("24bit")
            || title_lower.contains(" hires")
            || title_lower.contains("highres")
        {
            return Quality::Lossless;
        }

        // Check for high-quality indicators
        if title_lower.contains("320")
            || title_lower.contains("hq")
            || title_lower.contains("high quality")
            || title_lower.contains("cd quality")
            || title_lower.contains("studio")
            || title_lower.contains("master")
        {
            return Quality::High;
        }

        // Check for medium-quality indicators
        if title_lower.contains("128")
            || title_lower.contains("192")
            || title_lower.contains("256")
            || title_lower.contains("mp3")
        {
            return Quality::Medium;
        }

        // Default quality based on source
        match source {
            Source::Bandcamp => Quality::High, // Bandcamp typically provides high quality
            Source::YouTube => Quality::Medium, // YouTube varies, usually medium
            Source::SoundCloud => Quality::Medium, // SoundCloud varies, usually medium
            Source::Spotify => Quality::Medium, // Spotify is typically 160-320kbps
            Source::Web => Quality::Low, // Unknown web sources are unpredictable
        }
    }

    #[allow(dead_code)]
    pub fn new_with_cache(title: String, artist: String, duration: Option<f64>, url: String, source: Source, quality_cache: &mut HashMap<String, Quality>) -> Self {
        // Check if we have cached quality for this track
        if let Some(cached_quality) = quality_cache.get(&url) {
            return Self {
                title,
                artist,
                duration,
                url,
                source,
                quality: *cached_quality,
            };
        }

        // Detect quality and cache it
        let quality = Self::detect_quality(&title, source);
        quality_cache.insert(url.clone(), quality);
        
        Self {
            title,
            artist,
            duration,
            url,
            source,
            quality,
        }
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

    #[test]
    fn detects_quality() {
        // Test lossless detection
        assert_eq!(
            Track::detect_quality("FLAC - Song Title", Source::YouTube),
            Quality::Lossless
        );
        assert_eq!(
            Track::detect_quality("24bit Hi-Res Audio", Source::YouTube),
            Quality::Lossless
        );
        assert_eq!(
            Track::detect_quality("WAV Format Song", Source::YouTube),
            Quality::Lossless
        );

        // Test high quality detection
        assert_eq!(
            Track::detect_quality("320kbps - High Quality", Source::YouTube),
            Quality::High
        );
        assert_eq!(
            Track::detect_quality("HQ Studio Master", Source::YouTube),
            Quality::High
        );
        assert_eq!(
            Track::detect_quality("CD Quality Audio", Source::YouTube),
            Quality::High
        );

        // Test medium quality detection
        assert_eq!(
            Track::detect_quality("128kbps Standard", Source::YouTube),
            Quality::Medium
        );
        assert_eq!(
            Track::detect_quality("192kbps MP3", Source::YouTube),
            Quality::Medium
        );

        // Test source-based defaults
        assert_eq!(
            Track::detect_quality("Regular Song", Source::Bandcamp),
            Quality::High
        );
        assert_eq!(
            Track::detect_quality("Regular Song", Source::YouTube),
            Quality::Medium
        );
        assert_eq!(
            Track::detect_quality("Regular Song", Source::SoundCloud),
            Quality::Medium
        );
    }

    #[test]
    fn quality_filter_logic() {
        // Test All filter
        assert!(matches!(QualityFilter::All, _));
        
        // Test Lossless filter
        assert_eq!(QualityFilter::Lossless.label(), "Lossless");
        
        // Test High filter
        assert_eq!(QualityFilter::High.label(), "High");
        
        // Test Medium filter
        assert_eq!(QualityFilter::Medium.label(), "Medium");
        
        // Test Low filter
        assert_eq!(QualityFilter::Low.label(), "Low");
    }
}
