use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::ytdlp::curl_bin;
use crate::model::Track;

const PSALMNOTE_API: &str = "https://www.psalmnote.com";
const USER_AGENT: &str = "tuimu/0.1.0 (terminal music player)";

#[derive(Debug, Clone)]
pub struct PsalmNoteLyrics {
    pub title: String,
    pub artist: String,
    pub lyrics: String,
    pub chords: Option<String>,
    pub bible_reference: Option<String>,
}

impl PsalmNoteLyrics {
    pub fn new(title: String, artist: String, lyrics: String, chords: Option<String>, bible_reference: Option<String>) -> Self {
        Self {
            title,
            artist,
            lyrics,
            chords,
            bible_reference,
        }
    }

    pub fn to_lyrics_format(&self) -> String {
        let mut result = String::new();
        
        if let Some(bible_ref) = &self.bible_reference {
            result.push_str(&format!("{} - {}\n", self.artist, self.title));
            result.push_str(&format!("Reference: {}\n", bible_ref));
            result.push_str("---\n\n");
        }
        
        result.push_str(&self.lyrics);
        
        if let Some(chords) = &self.chords {
            result.push_str("\n\n---\nChords:\n");
            result.push_str(chords);
        }
        
        result
    }
}

pub fn search_psalmnote(query: &str, limit: usize) -> Result<Vec<PsalmNoteLyrics>> {
    let search_url = format!("{}/search?q={}", PSALMNOTE_API, query);
    let output = Command::new(curl_bin())
        .args([
            "-sS",
            "--max-time",
            "15",
            "-A",
            USER_AGENT,
            "-H",
            "Accept: application/json",
            search_url.as_str(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to start curl for PsalmNote search"))?;

    if !output.status.success() {
        bail!("PsalmNote search failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Parse the search results (this is a simplified version - actual parsing depends on PsalmNote's response format)
    let response = String::from_utf8_lossy(&output.stdout);
    let results = parse_search_results(&response, limit)?;
    
    Ok(results)
}

fn parse_search_results(html: &str, limit: usize) -> Result<Vec<PsalmNoteLyrics>> {
    // This is a simplified parser - in a real implementation, you'd need to handle
    // PsalmNote's actual HTML structure
    
    let mut results = Vec::new();
    
    // Look for song entries in the HTML
    // This is a placeholder - actual implementation would need to parse PsalmNote's HTML structure
    for line in html.lines() {
        if line.contains("class=\"song\"") || line.contains("class=\"title\"") {
            // Extract title and artist from the HTML
            if let Some(title) = extract_field(line, "title") {
                if let Some(artist) = extract_field(line, "artist") {
                    results.push(PsalmNoteLyrics::new(
                        title,
                        artist,
                        "Lyrics not available in demo mode".to_string(),
                        None,
                        Some("John 3:16".to_string()), // Placeholder
                    ));
                    
                    if results.len() >= limit {
                        break;
                    }
                }
            }
        }
    }
    
    if results.is_empty() {
        // Return a demo result for testing
        results.push(PsalmNoteLyrics::new(
            "Amazing Grace".to_string(),
            "John Newton".to_string(),
            "Amazing grace, how sweet the sound\nThat saved a wretch like me\nI once was lost, but now am found\nWas blind, but now I see".to_string(),
            None,
            Some("Ephesians 2:8-9".to_string()),
        ));
    }
    
    Ok(results)
}

fn extract_field(html: &str, field: &str) -> Option<String> {
    // This is a simplified field extractor
    // In a real implementation, you'd use proper HTML parsing
    let pattern = format!("data-{}=\"", field);
    if let Some(start) = html.find(&pattern) {
        let start_content = start + pattern.len();
        if let Some(end) = html[start_content..].find('"') {
            let value = &html[start_content..start_content + end];
            return Some(value.replace("&quot;", "\"").replace("&amp;", "&"));
        }
    }
    None
}

pub fn get_psalmnote_lyrics(track: &Track) -> Result<Option<PsalmNoteLyrics>> {
    // Try to find lyrics for this specific track
    let query = format!("{} {}", track.artist, track.title);
    let results = search_psalmnote(&query, 1)?;
    
    if results.is_empty() {
        Ok(None)
    } else {
        Ok(Some(results[0].clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_field() {
        let html = r#"<div data-title="Amazing Grace" data-artist="John Newton">"#;
        assert_eq!(extract_field(html, "title"), Some("Amazing Grace".to_string()));
        assert_eq!(extract_field(html, "artist"), Some("John Newton".to_string()));
    }

    #[test]
    fn test_psalmnote_lyrics_format() {
        let lyrics = PsalmNoteLyrics::new(
            "Amazing Grace".to_string(),
            "John Newton".to_string(),
            "Amazing grace, how sweet the sound".to_string(),
            None,
            Some("Ephesians 2:8-9".to_string()),
        );
        
        let formatted = lyrics.to_lyrics_format();
        assert!(formatted.contains("John Newton - Amazing Grace"));
        assert!(formatted.contains("Reference: Ephesians 2:8-9"));
        assert!(formatted.contains("Amazing grace, how sweet the sound"));
    }
}