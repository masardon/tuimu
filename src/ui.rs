use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap},
};

use crate::app::{App, Focus};
use crate::model::{RepeatMode, Source, Track, format_duration, truncate};

const ACCENT: Color = Color::Rgb(190, 150, 255);
const ACCENT_DIM: Color = Color::Rgb(105, 85, 140);
const GREEN: Color = Color::Rgb(120, 220, 160);
const RED: Color = Color::Rgb(245, 110, 120);
const YELLOW: Color = Color::Rgb(235, 200, 110);
const MUTED: Color = Color::Rgb(135, 135, 150);
const PANEL_BG: Color = Color::Rgb(22, 22, 30);
const SELECT_BG: Color = Color::Rgb(46, 39, 64);

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .split(area);

    draw_header(frame, app, chunks[0]);

    let body = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(chunks[1]);
    draw_search(frame, app, body[0]);
    draw_queue(frame, app, body[1]);
    draw_now_playing(frame, app, chunks[2]);
    draw_status(frame, app, chunks[3]);
    draw_footer(frame, app, chunks[4]);

    if app.help {
        draw_help(frame, area);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let focus = match app.focus {
        Focus::Search => "search",
        Focus::Results => "results",
        Focus::Queue => "queue",
    };

    let line = Line::from(vec![
        Span::styled(
            " ♫ tuimu ",
            Style::default().fg(Color::Black).bg(ACCENT).bold(),
        ),
        Span::raw("  free music from the terminal "),
        Span::styled(
            format!("│ {} ", app.source.label()),
            Style::default().fg(MUTED),
        ),
        Span::styled(format!("│ focus: {focus}"), Style::default().fg(ACCENT_DIM)),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(PANEL_BG)),
        area,
    );
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Search;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(focused))
        .title(Span::styled(
            format!(" Search · {} ", app.source.label()),
            Style::default()
                .fg(if focused { ACCENT } else { MUTED })
                .bold(),
        ))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 || inner.width == 0 {
        return;
    }

    let parts = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    let input_area = parts[0];
    let divider_area = parts[1];
    let list_area = parts[2];

    draw_input(frame, app, input_area, focused);
    frame.render_widget(
        Paragraph::new("─".repeat(divider_area.width as usize))
            .style(Style::default().fg(ACCENT_DIM)),
        divider_area,
    );

    if app.results.is_empty() {
        let message = if app.loading {
            format!("{} searching {}…", spinner(app.tick), app.source.label())
        } else {
            "No results yet. Type a query above and press Enter.".to_string()
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(MUTED).italic())
                .wrap(Wrap { trim: true }),
            list_area,
        );
        return;
    }

    let widths = [
        Constraint::Length(2),
        Constraint::Length(4),
        Constraint::Min(12),
        Constraint::Length(22),
        Constraint::Length(7),
    ];
    let current_url = app.current_track().map(|track| track.url.as_str());
    let rows = app.results.iter().enumerate().map(|(index, track)| {
        let is_current = current_url == Some(track.url.as_str());
        result_row(index, track, is_current)
    });
    let table = Table::new(rows, widths)
        .header(
            Row::new(["", "SRC", "TITLE", "ARTIST", "TIME"])
                .style(Style::default().fg(MUTED).bold()),
        )
        .column_spacing(1)
        .row_highlight_style(highlight_style(focused))
        .highlight_symbol(if focused { "▌ " } else { "  " });

    let mut state = TableState::default().with_selected(Some(
        app.result_selected.min(app.results.len().saturating_sub(1)),
    ));
    frame.render_stateful_widget(table, list_area, &mut state);
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    if area.width < 3 || area.height == 0 {
        return;
    }
    let prompt_width = 2usize;
    let visible_width = area.width as usize - prompt_width;
    let value = app.input.value();
    let cursor = app.input.cursor();
    let start = cursor.saturating_sub(visible_width.saturating_sub(1));
    let visible: String = value.chars().skip(start).take(visible_width).collect();

    let text = if app.input.is_empty() {
        Span::styled(
            format!("Search {} or paste a URL…", app.source.label()),
            Style::default().fg(MUTED).italic(),
        )
    } else {
        Span::raw(visible)
    };
    let line = Line::from(vec![
        Span::styled("› ", Style::default().fg(ACCENT).bold()),
        text,
    ]);
    frame.render_widget(Paragraph::new(line), area);

    if focused {
        let x = area
            .x
            .saturating_add(prompt_width as u16)
            .saturating_add((cursor.saturating_sub(start)) as u16)
            .min(area.right().saturating_sub(1));
        frame.set_cursor_position((x, area.y));
    }
}

fn result_row(index: usize, track: &Track, is_current: bool) -> Row<'static> {
    let marker = if is_current { "▶" } else { "" };
    let title = truncate(&track.title, 80);
    let artist = truncate(&track.artist, 24);
    let title_style = if is_current {
        Style::default().fg(GREEN).bold()
    } else {
        Style::default()
    };
    Row::new(vec![
        Cell::from(format!("{marker:>2}")).style(Style::default().fg(GREEN)),
        Cell::from(track.source.badge()).style(Style::default().fg(source_color(track.source))),
        Cell::from(title).style(title_style),
        Cell::from(artist).style(Style::default().fg(MUTED)),
        Cell::from(Text::from(track.duration_label()).alignment(Alignment::Right))
            .style(Style::default().fg(MUTED)),
    ])
    .style(if index.is_multiple_of(2) {
        Style::default()
    } else {
        Style::default().bg(Color::Rgb(25, 25, 34))
    })
}

fn draw_queue(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Queue;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(focused))
        .title(Span::styled(
            format!(" Queue · {} ", app.queue.len()),
            Style::default()
                .fg(if focused { ACCENT } else { MUTED })
                .bold(),
        ))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }
    if app.queue.is_empty() {
        frame.render_widget(
            Paragraph::new(
                "Queue is empty.\nPress Enter on a result to play it, or `a` to queue it.",
            )
            .style(Style::default().fg(MUTED).italic())
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let widths = [
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Min(10),
        Constraint::Length(14),
        Constraint::Length(6),
    ];
    let rows = app.queue.iter().enumerate().map(|(index, track)| {
        let is_current = app.current == Some(index);
        let marker = if is_current { "▶" } else { " " };
        let title = truncate(&track.title, 80);
        let artist = truncate(&track.artist, 14);
        let title_style = if is_current {
            Style::default().fg(GREEN).bold()
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(format!("{marker:>2}")).style(Style::default().fg(GREEN)),
            Cell::from(format!("{:>2}", index + 1)).style(Style::default().fg(MUTED)),
            Cell::from(title).style(title_style),
            Cell::from(artist).style(Style::default().fg(MUTED)),
            Cell::from(Text::from(track.duration_label()).alignment(Alignment::Right))
                .style(Style::default().fg(MUTED)),
        ])
        .style(if index.is_multiple_of(2) {
            Style::default()
        } else {
            Style::default().bg(Color::Rgb(25, 25, 34))
        })
    });

    let table = Table::new(rows, widths)
        .header(
            Row::new(["", "#", "TITLE", "ARTIST", "TIME"]).style(Style::default().fg(MUTED).bold()),
        )
        .column_spacing(1)
        .row_highlight_style(highlight_style(focused))
        .highlight_symbol(if focused { "▌ " } else { "  " });

    let mut state =
        TableState::default().with_selected(Some(app.queue_selected.min(app.queue.len() - 1)));
    frame.render_stateful_widget(table, inner, &mut state);
}

fn draw_now_playing(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT_DIM))
        .title(Span::styled(
            " Now playing ",
            Style::default().fg(ACCENT).bold(),
        ))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let (icon, icon_color) = if app.player.loading {
        (spinner(app.tick), YELLOW)
    } else if app.current.is_none() {
        ("■", MUTED)
    } else if app.player.paused {
        ("⏸", YELLOW)
    } else {
        ("▶", GREEN)
    };

    let current = app.current_track();
    let title_line = if let Some(track) = current {
        Line::from(vec![
            Span::styled(format!(" {icon} "), Style::default().fg(icon_color).bold()),
            Span::styled(
                truncate(&track.title, 64),
                Style::default().fg(Color::White).bold(),
            ),
            Span::styled(
                format!("  —  {}", truncate(&track.artist, 36)),
                Style::default().fg(MUTED),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(format!(" {icon} "), Style::default().fg(icon_color)),
            Span::styled(
                "Nothing playing — press Enter on a result",
                Style::default().fg(MUTED).italic(),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(title_line), Rect { height: 1, ..inner });

    if inner.height >= 2 {
        let duration = app.player.duration;
        let ratio = if duration > 0.0 {
            (app.player.position / duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let time_label = format!(
            "{} / {}",
            format_duration(app.player.position),
            format_duration(duration)
        );
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(ACCENT).bg(Color::Rgb(38, 38, 48)))
            .ratio(ratio)
            .label(Span::styled(time_label, Style::default().fg(Color::White)));
        frame.render_widget(
            gauge,
            Rect {
                y: inner.y + 1,
                height: 1,
                ..inner
            },
        );
    }

    if inner.height >= 3 {
        let source = current.map(|track| track.source.label()).unwrap_or("—");
        let info = format!(
            " vol {:>3.0}%   ·   repeat {}   ·   source {}   ·   queue {}",
            app.player.volume,
            repeat_label(app.repeat),
            source,
            app.queue.len()
        );
        frame.render_widget(
            Paragraph::new(info).style(Style::default().fg(MUTED)),
            Rect {
                y: inner.y + 2,
                height: 1,
                ..inner
            },
        );
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let loading = app.loading || app.player.loading;
    let mut spans = Vec::new();
    if loading {
        spans.push(Span::styled(
            format!(" {} ", spinner(app.tick)),
            Style::default().fg(YELLOW),
        ));
    } else {
        spans.push(Span::raw(" "));
    }
    let color = if app.status_is_error {
        RED
    } else if loading {
        YELLOW
    } else {
        MUTED
    };
    spans.push(Span::styled(&app.status, Style::default().fg(color)));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let hints: &[(&str, &str)] = match app.focus {
        Focus::Search => &[
            ("Enter", "search"),
            ("Esc", "results"),
            ("Tab", "next"),
            ("Ctrl+U", "clear"),
            ("F2", "source"),
            ("F1", "help"),
            ("Ctrl+C", "quit"),
        ],
        Focus::Results => &[
            ("Enter", "play"),
            ("a", "queue"),
            ("A", "queue all"),
            ("Space", "pause"),
            ("n/p", "next/prev"),
            ("←/→", "seek"),
            ("+/-", "volume"),
            ("/", "search"),
            ("Tab", "queue"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Focus::Queue => &[
            ("Enter", "play"),
            ("d", "remove"),
            ("J/K", "move"),
            ("Space", "pause"),
            ("n/p", "next/prev"),
            ("r", "repeat"),
            ("/", "search"),
            ("Tab", "search"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };

    let mut spans = Vec::new();
    for (index, (key, label)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ·  ", Style::default().fg(ACCENT_DIM)));
        }
        spans.push(Span::styled(*key, Style::default().fg(ACCENT).bold()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, Style::default().fg(MUTED)));
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(spans),
            Line::from(Span::styled(
                "yt-dlp finds the audio · mpv plays it · no account required",
                Style::default().fg(ACCENT_DIM).italic(),
            )),
        ])
        .wrap(Wrap { trim: true })
        .style(Style::default().bg(PANEL_BG)),
        area,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let width = area.width.saturating_sub(4).min(100);
    let height = area.height.saturating_sub(2).min(26);
    if width < 20 || height < 8 {
        return;
    }
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " Help · F1 / Esc to close ",
            Style::default().fg(ACCENT).bold(),
        ))
        .style(Style::default().bg(Color::Rgb(17, 17, 24)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 {
        return;
    }

    let columns =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);
    let left = vec![
        section("PLAYBACK"),
        help_line("Enter", "play selection / queue item"),
        help_line("Space", "play or pause"),
        help_line("n / p", "next / previous track"),
        help_line("← / →", "seek -5 / +5 seconds"),
        help_line("+ / -", "volume"),
        help_line("m", "mute"),
        help_line("r", "repeat off → all → one"),
        Line::raw(""),
        section("FOCUS"),
        help_line("Tab", "search → results → queue"),
        help_line("Shift+Tab", "cycle backwards"),
        help_line("/ or i", "edit search"),
        help_line("Esc", "return to search"),
    ];
    let right = vec![
        section("QUEUE & SEARCH"),
        help_line("a / A", "queue one / all results"),
        help_line("d / Del", "remove queue item"),
        help_line("J / K", "move queue item down / up"),
        help_line("g / G", "jump to first / last"),
        help_line("F2", "switch YouTube / SoundCloud"),
        Line::raw(""),
        section("SOURCES"),
        help_line("Search", "YouTube, SoundCloud"),
        help_line("SoundCloud", "30s previews without login"),
        help_line("Paste URL", "anything yt-dlp supports"),
        help_line("Spotify", "track links matched to YouTube"),
        Line::raw(""),
        Line::from(Span::styled(
            "No login. No account. Just yt-dlp and mpv.",
            Style::default().fg(GREEN).italic(),
        )),
        help_line("Ctrl+C", "quit"),
    ];

    frame.render_widget(Paragraph::new(left).wrap(Wrap { trim: true }), columns[0]);
    frame.render_widget(Paragraph::new(right).wrap(Wrap { trim: true }), columns[1]);
}

fn help_line<'a>(key: &'a str, description: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {key:<10}"), Style::default().fg(ACCENT).bold()),
        Span::styled(description, Style::default().fg(MUTED)),
    ])
}

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default().fg(YELLOW).bold(),
    ))
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(ACCENT_DIM)
    }
}

fn highlight_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .bg(SELECT_BG)
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(Color::Rgb(32, 30, 42)).fg(Color::Gray)
    }
}

fn source_color(source: Source) -> Color {
    match source {
        Source::YouTube => Color::Rgb(255, 105, 105),
        Source::SoundCloud => Color::Rgb(255, 165, 90),
        Source::Bandcamp => Color::Rgb(100, 205, 200),
        Source::Spotify => Color::Rgb(110, 220, 130),
        Source::Web => MUTED,
    }
}

fn repeat_label(repeat: RepeatMode) -> &'static str {
    repeat.label()
}

fn spinner(tick: u64) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[(tick as usize / 2) % FRAMES.len()]
}
