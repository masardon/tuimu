# tuimu

A fast, free terminal music player. Search YouTube and SoundCloud, paste any URL that [yt-dlp](https://github.com/yt-dlp/yt-dlp) supports, queue tracks, and play them with [mpv](https://mpv.io/).

No accounts, no logins, no paid APIs.

## Requirements

- [Rust](https://www.rust-lang.org/) (2024 edition) to build
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) - finds and extracts audio
- [mpv](https://mpv.io/) - plays audio over JSON IPC
- [curl](https://curl.se/) - optional, only needed for Spotify track metadata lookup

## Install

```bash
git clone <repo> tuimu
cd tuimu
cargo build --release
./target/release/tuimu
```

Or run in development: `cargo run`

## Usage

1. Type a search query and press `Enter`. Use `F2` to switch between YouTube and SoundCloud.
2. Navigate results with `j`/`k`, press `Enter` to replace the queue and play, or `a` to queue a single track and `A` to queue everything.
3. Switch to the queue with `Tab`, reorder with `J`/`K`, remove with `d`, and play with `Enter`.
4. Control playback with `Space`, `n`/`p`, `←`/`→` to seek, `+`/`-` to change volume, `m` to mute, `r` to cycle repeat (`off`/`all`/`one`).
5. Paste a URL directly into the search box (or any `http(s)` URL) to play/queue it. yt-dlp handles playlists.
6. Paste a single Spotify track URL - `tuimu` pulls the title from Spotify's public oEmbed and matches it to a free YouTube stream (Spotify playback requires login).
7. Press `F1` for help, `Ctrl+C` to quit.

## Notes

- SoundCloud tracks without a full download are often 30-second previews when unauthenticated. This is a SoundCloud limitation, not a `tuimu` bug.
- YouTube stream extraction uses yt-dlp's Android player client for reliability; the app relaunches mpv per source to send the required `Referer` headers without breaking playback.
- All audio extraction goes through yt-dlp and is passed to mpv as a direct URL - no browser session or cookies are used.

## Environment variables

- `TUIMU_YTDLP` - override yt-dlp executable (default: `yt-dlp`)
- `TUIMU_MPV` - override mpv executable (default: `mpv`)
- `TUIMU_CURL` - override curl executable (default: `curl`)
