# tuimu

A fast, free terminal music player. Search YouTube and SoundCloud, paste any URL that [yt-dlp](https://github.com/yt-dlp/yt-dlp) supports, queue tracks, and play them with [mpv](https://mpv.io/).

No accounts, no logins, no paid APIs.

[![GitHub](https://img.shields.io/github/stars/masardon/tuimu?style=social)](https://github.com/masardon/tuimu)
[![GitHub issues](https://img.shields.io/github/issues/masardon/tuimu)](https://github.com/masardon/tuimu/issues)
[![GitHub license](https://img.shields.io/github/license/masardon/tuimu)](https://github.com/masardon/tuimu/blob/main/LICENSE)

## Requirements

- [Rust](https://www.rust-lang.org/) (2024 edition) to build
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) - finds and extracts audio
- [mpv](https://mpv.io/) - plays audio over JSON IPC
- [curl](https://curl.se/) - optional, only needed for Spotify track metadata lookup

## Install

### Using Cargo (Recommended)

```bash
git clone https://github.com/masardon/tuimu.git
cd tuimu
cargo build --release
./target/release/tuimu
```

Or run in development: `cargo run`

### Without Cargo (Pre-built Binary)

1. Download the pre-built binary for your Linux distribution from the releases page.
2. Make the binary executable:
   ```bash
   chmod +x tuimu
   ```
3. Move it to a directory in your PATH:
   ```bash
   sudo mv tuimu /usr/local/bin/
   ```
4. Run it from anywhere:
   ```bash
   tuimu
   ```

### Using the Installation Script

1. Download the installation script:
   ```bash
   curl -O https://raw.githubusercontent.com/masardon/tuimu/main/install.sh
   ```
2. Make it executable:
   ```bash
   chmod +x install.sh
   ```
3. Run it:
   ```bash
   ./install.sh
   ```

### Building for Release

1. Clean caches:
   ```bash
   ./clean-cache.sh
   ```
2. Build the release:
   ```bash
   ./build-release.sh
   ```
3. The script will create a tar.gz archive with the binary and documentation.

## Usage

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| **?** / **F1** | Toggle help window |
| **@** | Switch between YouTube and SoundCloud |
| **#** / **F3** | Toggle lyrics window |
| **Enter** | Play selected track / Start search |
| **Esc** | Close lyrics/help / Go to results |
| **Tab** | Switch focus (Search → Results → Queue) |
| **BackTab** | Switch focus in reverse |
| **j/k** | Navigate results/queue |
| **g** | Go to first item |
| **G** | Go to last item |
| **a** | Queue selected track |
| **A** | Queue all tracks |
| **Space** | Play/pause |
| **n/p** | Next/previous track |
| **←/→** | Seek backward/forward |
| **+/-** | Increase/decrease volume |
| **m** | Toggle mute |
| **r** | Cycle repeat mode (off/all/one) |
| **d** | Remove from queue |
| **J/K** | Move queue item down/up |
| **f** | Cycle quality filter |
| **W** | Toggle high-quality-only filter |
| **Q** / **T** | Sort results by quality / duration |
| **q** | Quit (outside the search box) |

While the search box is focused, printable keys (including `q`, `@`, `#`, and `?`) are typed into the query. Switch focus with `Tab` to use those shortcuts.

### Basic Workflow

1. **Search for music**: 
   - Type a search query and press `Enter`
   - Use `@` to switch between YouTube and SoundCloud
   - Paste any URL directly into the search box (YouTube, SoundCloud, Spotify, etc.)

2. **Navigate results**:
   - Use `j`/`k` to navigate through search results
   - Press `Enter` to replace the queue and play the selected track
   - Press `a` to queue a single track
   - Press `A` to queue all tracks

3. **Manage queue**:
   - Switch to the queue with `Tab`
   - Reorder tracks with `J`/`K`
   - Remove tracks with `d`
   - Play with `Enter`

4. **Control playback**:
   - Use `Space` to play/pause
   - Use `n`/`p` for next/previous track
   - Use `←`/`→` to seek
   - Use `+`/`-` to change volume
   - Use `m` to mute
   - Use `r` to cycle repeat modes

5. **View lyrics**:
   - Press `#` (or `F3`) to toggle the lyrics window
   - The lyrics window works even when no track is playing
   - Press `Esc` to close the lyrics window

6. **Get help**:
   - Press `?` to toggle the help window
   - Press `Esc` to close the help window

7. **Quit**:
   - Press `q` or `Ctrl+C` to quit

## Advanced Features

### Spotify Integration
Paste a single Spotify track URL - `tuimu` pulls the title from Spotify's public oEmbed and matches it to a free YouTube stream (Spotify playback requires login).

### URL Support
- Directly paste any URL that yt-dlp supports
- Handles playlists automatically
- Supports YouTube, SoundCloud, Spotify, and many other platforms

### Environment Variables

- `TUIMU_YTDLP` - override yt-dlp executable (default: `yt-dlp`)
- `TUIMU_MPV` - override mpv executable (default: `mpv`)
- `TUIMU_CURL` - override curl executable (default: `curl`)

## Scripts

### clean-cache.sh
A script to clean up all caches before building or releasing tuimu:
```bash
./clean-cache.sh
```

### install.sh
A script to install tuimu without requiring Cargo:
```bash
./install.sh
```

### build-release.sh
A script to build tuimu for release and package it:
```bash
./build-release.sh
```

## Cache Management

### Before Release/Build

To ensure a clean build and release:

1. **Use the clean-cache script** (recommended):
   ```bash
   ./clean-cache.sh
   ```

2. **Manual cleanup** (if needed):
   ```bash
   # Clean Rust build cache
   cargo clean
   
   # Clear Cargo registry and git cache
   rm -rf ~/.cargo/registry/cache/
   rm -rf ~/.cargo/git/db/
   
   # Clear yt-dlp cache (if used)
   rm -rf ~/.cache/yt-dlp/
   
   # Clear mpv cache (if used)
   rm -rf ~/.mpv/
   
   # Clear application cache (if any)
   rm -rf ~/.local/share/tuimu/
   ```

### For Regular Maintenance

To clear caches periodically:

```bash
# Use the clean-cache script
./clean-cache.sh

# Or manually clear all caches
rm -rf ~/.cargo/registry/cache/
rm -rf ~/.cargo/git/db/
rm -rf ~/.cache/yt-dlp/
rm -rf ~/.mpv/
rm -rf ~/.local/share/tuimu/
```

## Notes

- SoundCloud tracks without a full download are often 30-second previews when unauthenticated. This is a SoundCloud limitation, not a `tuimu` bug.
- The quality shown next to each track is an estimate based on title tags and source metadata, not a measurement of the actual stream.
- YouTube stream extraction uses yt-dlp's Android player client for reliability; the app relaunches mpv per source to send the required `Referer` headers without breaking playback.
- All audio extraction goes through yt-dlp and is passed to mpv as a direct URL - no browser session or cookies are used.
- The lyrics feature works with or without a currently playing track.
- Printable keys are captured by the search box while it is focused; shortcuts such as `?`, `@`, and `#` are active in the results and queue views.

## Troubleshooting

If you encounter issues:

1. **Ensure all dependencies are installed**:
   ```bash
   # Ubuntu/Debian
   sudo apt-get install rustc yt-dlp mpv curl
   
   # Fedora
   sudo dnf install rust yt-dlp mpv curl
   
   # Arch
   sudo pacman -S rust yt-dlp mpv curl
   ```

2. **Check if executables are in your PATH**:
   ```bash
   which yt-dlp mpv rustc curl
   ```

3. **Try with verbose output**:
   ```bash
   RUST_LOG=debug tuimu
   ```

4. **Reset the application state**:
   ```bash
   rm -rf ~/.local/share/tuimu/
   ```