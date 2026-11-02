mod app;
mod input;
mod lyrics;
mod model;
mod mpv;
mod ui;
mod ytdlp;

use std::io::{self, Stdout};
use std::panic;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::App;
use crate::mpv::Mpv;

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn main() {
    if let Err(error) = run() {
        eprintln!("tuimu: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }

    ytdlp::check_dependencies()?;
    let mpv = Mpv::spawn().context("could not start the mpv playback engine")?;

    let mut terminal = init_terminal().context("could not initialize the terminal")?;
    install_panic_hook();

    let mut app = App::new(mpv);
    let result = event_loop(&mut terminal, &mut app);
    let restore_result = restore_terminal(&mut terminal);

    result.and(restore_result)
}

fn event_loop(terminal: &mut Tui, app: &mut App) -> Result<()> {
    while !app.should_quit {
        app.poll_background();
        terminal
            .draw(|frame| ui::draw(frame, app))
            .context("could not draw the interface")?;

        if event::poll(Duration::from_millis(80)).context("could not poll terminal events")? {
            match event::read().context("could not read terminal events")? {
                Event::Key(key) => {
                    // Handle special keys directly with highest priority
                    if key.code == KeyCode::Char('@') {
                        app.source = app.source.next();
                        app.set_status(format!("Search source: {}", app.source.label()), false);
                    } else if key.code == KeyCode::Char('#') {
                        app.toggle_lyrics();
                    } else {
                        // Handle other keys normally
                        app.handle_key(key);
                    }
                },
                Event::Paste(text) => app.handle_paste(&text),
                Event::Resize(_, _) => {}
                _ => {}
            }
        } else {
            app.on_tick();
        }

        app.poll_background();
    }
    Ok(())
}

fn init_terminal() -> Result<Tui> {
    enable_raw_mode().context("could not enable raw mode")?;
    let mut stdout = io::stdout();
    
    // Set terminal to handle all keys including F keys
    if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("could not enter the alternate screen");
    }

    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => Ok(terminal),
        Err(error) => {
            let _ = disable_raw_mode();
            Err(error).context("could not create the ratatui terminal")
        }
    }
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        Show
    )
    .context("could not restore the terminal")?;
    terminal
        .show_cursor()
        .context("could not show the cursor")?;
    Ok(())
}

fn install_panic_hook() {
    let original = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, DisableBracketedPaste, Show);
        let _ = disable_raw_mode();
        original(info);
    }));
}

fn print_help() {
    println!(
        "tuimu {version} — a free music player for the terminal\n\
         \n\
         USAGE:\n  tuimu\n\
         \n\
         REQUIREMENTS:\n\
           yt-dlp   finds and extracts audio (required)\n\
           mpv      plays audio over JSON IPC (required)\n\
           curl     reads Spotify metadata and LrcLib lyrics (optional)\n\
         \n\
         OPTIONS:\n\
           -h, --help   show this help\n\
         \n\
         ENVIRONMENT:\n\
           TUIMU_YTDLP  yt-dlp executable (default: yt-dlp)\n\
           TUIMU_MPV    mpv executable (default: mpv)\n\
           TUIMU_CURL   curl executable (default: curl)\n\
         \n\
         Search YouTube or SoundCloud, or paste any URL supported by yt-dlp.\n\
         SoundCloud usually returns 30-second previews without an account.\n\
         Spotify track URLs are matched to free YouTube audio because Spotify\n\
         playback itself requires an authenticated account.\n\
         Press l or F3 for lyrics (LrcLib, no account required).\n",
        version = env!("CARGO_PKG_VERSION")
    );
}
