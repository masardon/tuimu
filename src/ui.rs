use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap},
};

use crate::app::{App, Focus, LyricsStatus, NameAction, PickerLevel, PickerMode, PlaylistUi};
use crate::model::{Quality, RepeatMode, Source, Track, format_duration, truncate};
use crate::visualizer::{BANDS, VisualizerStatus};

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

    // Left column: search stacked over queue, each exactly half. The right
    // half holds the lyrics panel (`#` / F3) over the visualizer (`v`).
    let show_lyrics = app.lyrics_enabled && app.lyrics.visible;
    let show_viz = app.viz_enabled && app.viz_visible;
    let columns = if show_lyrics || show_viz {
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[1])
    } else {
        Layout::horizontal([Constraint::Percentage(100)]).split(chunks[1])
    };
    let main = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(columns[0]);
    draw_search(frame, app, main[0]);
    draw_queue(frame, app, main[1]);
    if show_lyrics && show_viz {
        // Lyrics get three quarters, the visualizer the bottom quarter.
        let right = Layout::vertical([Constraint::Percentage(75), Constraint::Percentage(25)])
            .split(columns[1]);
        draw_lyrics_panel(frame, app, right[0]);
        draw_visualizer(frame, app, right[1]);
    } else if show_lyrics {
        draw_lyrics_panel(frame, app, columns[1]);
    } else if show_viz {
        draw_visualizer(frame, app, columns[1]);
    }
    draw_now_playing(frame, app, chunks[2]);
    draw_status(frame, app, chunks[3]);
    draw_footer(frame, app, chunks[4]);

    if app.help {
        draw_help(frame, area);
    }
    draw_playlist_popup(frame, app, area);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let focus = match app.focus {
        Focus::Search => "search",
        Focus::Results => "results",
        Focus::Queue => "queue",
    };

    let line = Line::from(vec![
        Span::styled(
            " ♫ TuiMu ",
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
    let focused = app.focus == Focus::Search || app.focus == Focus::Results;
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
        Constraint::Min(10),
        Constraint::Length(20),
        Constraint::Length(7),
        Constraint::Length(10),
    ];
    let current_url = app.current_track().map(|track| track.url.as_str());
    let rows = app.results.iter().enumerate().map(|(index, track)| {
        let is_current = current_url == Some(track.url.as_str());
        result_row(index, track, is_current)
    });
    let table = Table::new(rows, widths)
        .header(
            Row::new(["", "SRC", "TITLE", "ARTIST", "TIME", "QUALITY~"])
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
    let title = truncate(&track.title, 70);
    let artist = truncate(&track.artist, 20);
    let quality = format!("({})", track.quality_label());
    let title_style = if is_current {
        Style::default().fg(GREEN).bold()
    } else {
        Style::default()
    };
    let quality_style = match track.quality {
        Quality::Low => Style::default().fg(Color::Red),
        Quality::Medium => Style::default().fg(Color::Yellow),
        Quality::High => Style::default().fg(Color::Green),
        Quality::Lossless => Style::default().fg(Color::Cyan),
    };

    Row::new(vec![
        Cell::from(format!("{marker:>2}")).style(Style::default().fg(GREEN)),
        Cell::from(track.source.badge()).style(Style::default().fg(source_color(track.source))),
        Cell::from(title).style(title_style),
        Cell::from(artist).style(Style::default().fg(MUTED)),
        Cell::from(Text::from(track.duration_label()).alignment(Alignment::Right))
            .style(Style::default().fg(MUTED)),
        Cell::from(quality).style(quality_style),
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
        Constraint::Length(10),
    ];
    let rows = app.queue.iter().enumerate().map(|(index, track)| {
        let is_current = app.current == Some(index);
        let marker = if is_current { "▶" } else { " " };
        let title = truncate(&track.title, 70);
        let artist = truncate(&track.artist, 14);
        let quality = format!("({})", track.quality_label());
        let title_style = if is_current {
            Style::default().fg(GREEN).bold()
        } else {
            Style::default()
        };
        let quality_style = match track.quality {
            Quality::Low => Style::default().fg(Color::Red),
            Quality::Medium => Style::default().fg(Color::Yellow),
            Quality::High => Style::default().fg(Color::Green),
            Quality::Lossless => Style::default().fg(Color::Cyan),
        };
        Row::new(vec![
            Cell::from(format!("{marker:>2}")).style(Style::default().fg(GREEN)),
            Cell::from(format!("{:>2}", index + 1)).style(Style::default().fg(MUTED)),
            Cell::from(title).style(title_style),
            Cell::from(artist).style(Style::default().fg(MUTED)),
            Cell::from(Text::from(track.duration_label()).alignment(Alignment::Right))
                .style(Style::default().fg(MUTED)),
            Cell::from(quality).style(quality_style),
        ])
        .style(if index.is_multiple_of(2) {
            Style::default()
        } else {
            Style::default().bg(Color::Rgb(25, 25, 34))
        })
    });

    let table = Table::new(rows, widths)
        .header(
            Row::new(["", "#", "TITLE", "ARTIST", "TIME", "QUALITY~"])
                .style(Style::default().fg(MUTED).bold()),
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

    // Add quality filter indicator
    if app.quality_filter != crate::model::QualityFilter::All {
        spans.push(Span::styled(
            format!(" [Filter: {}]", app.quality_filter.label()),
            Style::default().fg(Color::Cyan),
        ));
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
            ("f", "filter"),
            ("@", "source"),
            ("#", "lyrics"),
            ("S", "save playlist"),
            ("s", "add to playlist"),
            ("o", "playlists"),
            ("v", "visualizer"),
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
            ("f", "filter"),
            ("#", "lyrics"),
            ("S", "save playlist"),
            ("s", "add to playlist"),
            ("o", "playlists"),
            ("v", "visualizer"),
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
                "yt-dlp finds the audio · mpv plays it · LrcLib provides lyrics",
                Style::default().fg(ACCENT_DIM).italic(),
            )),
        ])
        .wrap(Wrap { trim: true })
        .style(Style::default().bg(PANEL_BG)),
        area,
    );
}

fn draw_lyrics_panel(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(false))
        .title(Span::styled(" Lyrics ", Style::default().fg(ACCENT).bold()))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let title_line = if app.lyrics.title.is_empty() {
        Line::from(Span::styled(
            " Waiting for playback… ",
            Style::default().fg(MUTED).italic(),
        ))
    } else {
        Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(
                truncate(&app.lyrics.title, 30),
                Style::default().fg(Color::White).bold(),
            ),
            Span::styled(
                format!(" — {}", truncate(&app.lyrics.artist, 20)),
                Style::default().fg(MUTED),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(title_line), Rect { height: 1, ..inner });

    let content_area = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if content_area.height == 0 {
        return;
    }

    let content: Vec<Line> = if !app.lyrics_enabled {
        hint_lines("Lyrics disabled (--no-lyrics)")
    } else if app.current_track().is_none() {
        hint_lines("Lyrics will appear when playback starts")
    } else {
        match app.lyrics.status() {
            LyricsStatus::Loading => vec![Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(spinner(app.tick), Style::default().fg(YELLOW)),
                Span::styled(" Loading lyrics…", Style::default().fg(Color::White)),
            ])],
            LyricsStatus::Unavailable => vec![Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled("✗", Style::default().fg(RED)),
                Span::styled(" Lyrics Unavailable", Style::default().fg(Color::White)),
            ])],
            LyricsStatus::Instrumental => vec![Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled("♪", Style::default().fg(GREEN)),
                Span::styled(" Instrumental", Style::default().fg(Color::White)),
            ])],
            LyricsStatus::Ready | LyricsStatus::Idle => lyrics_lines(app, content_area),
        }
    };

    frame.render_widget(
        Paragraph::new(content).wrap(Wrap { trim: true }),
        content_area,
    );
}

fn hint_lines(message: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        format!(" {message}"),
        Style::default().fg(MUTED).italic(),
    ))]
}

fn lyrics_lines(app: &App, area: Rect) -> Vec<Line<'static>> {
    let Some(lyrics) = &app.lyrics.lines else {
        return hint_lines("Lyrics will appear when playback starts");
    };
    let visible = area.height as usize;
    let mut content = Vec::new();

    if let Some(synced) = &lyrics.synced {
        let total = synced.len();
        let current = lyrics.current_line(app.player.position);
        let anchor = current.unwrap_or(0);
        let start = if total <= visible {
            0
        } else {
            anchor.saturating_sub(visible / 2).min(total - visible)
        };
        let end = (start + visible).min(total);
        for (offset, line) in synced[start..end].iter().enumerate() {
            let is_current = current == Some(start + offset);
            let prefix = if is_current {
                Span::styled("▶ ", Style::default().fg(GREEN).bold())
            } else {
                Span::styled("  ", Style::default())
            };
            let text = if is_current {
                Span::styled(line.text.clone(), Style::default().fg(GREEN))
            } else {
                Span::raw(line.text.clone())
            };
            content.push(Line::from(vec![prefix, text]));
        }
    } else if let Some(plain) = &lyrics.plain {
        // Plain lyrics have no timestamps to follow, so render from the top.
        for text in plain.lines().take(visible) {
            content.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::raw(text.to_string()),
            ]));
        }
    }

    content
}

const BAR_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn draw_visualizer(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(false))
        .title(Span::styled(
            " Visualizer ",
            Style::default().fg(ACCENT).bold(),
        ))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.viz_status() == VisualizerStatus::Unavailable {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " Needs PulseAudio / PipeWire (parec)",
                Style::default().fg(MUTED).italic(),
            )),
            inner,
        );
        return;
    }

    // One bar per column; with fewer columns than bands, sample the bands.
    let columns = inner.width as usize;
    let height = inner.height as usize;
    let mut rows: Vec<Vec<(char, Color)>> = vec![Vec::with_capacity(columns); height];
    for column in 0..columns {
        let band = column * BANDS / columns.max(1);
        let level = app.viz_bars().get(band).copied().unwrap_or(0.0);
        let total_eighths = (level * height as f32 * 8.0).round() as usize;
        let full = total_eighths / 8;
        let remainder = total_eighths % 8;

        let color = band_gradient(band);
        for (row, cells) in rows.iter_mut().enumerate().take(height) {
            let glyph = if row < full {
                '█'
            } else if row == full && remainder > 0 {
                BAR_CHARS[remainder - 1]
            } else {
                ' '
            };
            cells.push((glyph, color));
        }
    }

    // Render bottom-up so bars grow from the panel floor.
    let lines: Vec<Line> = rows
        .iter()
        .rev()
        .map(|row| {
            Line::from(
                row.iter()
                    .map(|(glyph, color)| {
                        Span::styled(glyph.to_string(), Style::default().fg(*color))
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Gradient from accent (bass) to green (treble) across the band range.
fn band_gradient(band: usize) -> Color {
    let t = (band as f32 / (BANDS - 1) as f32).clamp(0.0, 1.0);
    let channel = |a: u16, b: u16| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color::Rgb(channel(190, 120), channel(150, 220), channel(255, 160))
}

fn draw_playlist_popup(frame: &mut Frame, app: &App, area: Rect) {
    match &app.playlist_ui {
        PlaylistUi::None => {}
        PlaylistUi::NamePrompt { action, input } => {
            draw_playlist_name_prompt(frame, area, action, input);
        }
        PlaylistUi::Picker {
            mode,
            level,
            selected,
            names,
        } => draw_playlist_picker(frame, area, mode, level, *selected, names),
    }
}

fn popup_area(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn draw_playlist_name_prompt(
    frame: &mut Frame,
    area: Rect,
    action: &NameAction,
    input: &crate::input::TextInput,
) {
    let (title, confirm_label) = match action {
        NameAction::SaveQueue => (" Save queue as playlist ", "Enter save · Esc cancel"),
        NameAction::Rename { .. } => (" Rename playlist ", "Enter rename · Esc cancel"),
    };

    let popup = popup_area(area, 52, 6);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(title, Style::default().fg(ACCENT).bold()))
        .style(Style::default().bg(Color::Rgb(17, 17, 24)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 2 {
        return;
    }

    let value = input.value();
    let cursor = input.cursor();
    let text = if value.is_empty() {
        Span::styled("Type a name…", Style::default().fg(MUTED).italic())
    } else {
        Span::raw(value.clone())
    };
    // Keep the cursor visible for long names by windowing from the left.
    let visible_width = inner.width.saturating_sub(3) as usize;
    let start = cursor.saturating_sub(visible_width.saturating_sub(1));
    let shown: String = value.chars().skip(start).take(visible_width).collect();
    let text = if value.is_empty() {
        text
    } else {
        Span::raw(shown)
    };

    let input_area = Rect { height: 1, ..inner };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::default().fg(ACCENT).bold()),
            text,
        ])),
        input_area,
    );
    let cursor_x = inner
        .x
        .saturating_add(2)
        .saturating_add((cursor.saturating_sub(start)) as u16)
        .min(inner.right().saturating_sub(1));
    frame.set_cursor_position((cursor_x, inner.y));

    let hint_area = Rect {
        y: inner.y + 1,
        height: 1,
        ..inner
    };
    frame.render_widget(
        Paragraph::new(Span::styled(confirm_label, Style::default().fg(MUTED))),
        hint_area,
    );
}

fn draw_playlist_picker(
    frame: &mut Frame,
    area: Rect,
    mode: &PickerMode,
    level: &PickerLevel,
    selected: usize,
    names: &[String],
) {
    let title = match mode {
        PickerMode::Open => " Playlists ",
        PickerMode::AddTo { .. } => " Add to playlist ",
    };
    let hints = match (mode, level) {
        (_, PickerLevel::Tracks { .. }) => "Enter play · d remove · Esc back",
        (PickerMode::AddTo { .. }, PickerLevel::Playlists) => "Enter add track · Esc cancel",
        (PickerMode::Open, PickerLevel::Playlists) => {
            "Enter load · a append · e tracks · d delete · r rename · Esc close"
        }
    };

    let popup = popup_area(area, 60, 20);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(title, Style::default().fg(ACCENT).bold()))
        .style(Style::default().bg(Color::Rgb(17, 17, 24)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 2 {
        return;
    }

    let rows_area = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let hint_area = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    frame.render_widget(
        Paragraph::new(Span::styled(hints, Style::default().fg(MUTED))),
        hint_area,
    );

    let visible = rows_area.height as usize;
    let content: Vec<Line> = match level {
        PickerLevel::Playlists => {
            if names.is_empty() {
                hint_lines(match mode {
                    PickerMode::Open => "No playlists yet. Press S in the queue view to save one.",
                    PickerMode::AddTo { .. } => {
                        "No playlists yet. Save one first with S from the queue view."
                    }
                })
            } else {
                let start = window_start(selected, visible, names.len());
                names[start..start + visible.min(names.len() - start)]
                    .iter()
                    .enumerate()
                    .map(|(offset, name)| {
                        selectable_row(offset + start == selected, name.clone(), None)
                    })
                    .collect()
            }
        }
        PickerLevel::Tracks { tracks, .. } => {
            if tracks.is_empty() {
                hint_lines("This playlist is empty.")
            } else {
                let start = window_start(selected, visible, tracks.len());
                tracks[start..start + visible.min(tracks.len() - start)]
                    .iter()
                    .enumerate()
                    .map(|(offset, track)| {
                        selectable_row(
                            offset + start == selected,
                            truncate(&track.title, 36),
                            Some(truncate(&track.artist, 18)),
                        )
                    })
                    .collect()
            }
        }
    };

    frame.render_widget(Paragraph::new(content).wrap(Wrap { trim: true }), rows_area);
}

fn window_start(selected: usize, visible: usize, total: usize) -> usize {
    if total <= visible {
        0
    } else {
        selected.saturating_sub(visible / 2).min(total - visible)
    }
}

fn selectable_row(is_selected: bool, title: String, artist: Option<String>) -> Line<'static> {
    let mut spans = Vec::new();
    if is_selected {
        spans.push(Span::styled("▶ ", Style::default().fg(GREEN).bold()));
        spans.push(Span::styled(
            title,
            Style::default().fg(Color::White).bold(),
        ));
        if let Some(artist) = artist {
            spans.push(Span::styled(
                format!(" — {artist}"),
                Style::default().fg(MUTED),
            ));
        }
    } else {
        spans.push(Span::styled("  ", Style::default()));
        spans.push(Span::styled(title, Style::default().fg(MUTED)));
        if let Some(artist) = artist {
            spans.push(Span::styled(
                format!(" — {artist}"),
                Style::default().fg(Color::Rgb(100, 100, 115)),
            ));
        }
    }
    Line::from(spans)
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
            " Help · ? / Esc to close ",
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
        Line::from(Span::raw("")),
        section("FOCUS"),
        help_line("Tab", "search → results → queue"),
        help_line("Shift+Tab", "cycle backwards"),
        help_line("/ or i", "edit search"),
        help_line("Esc", "return to search"),
        help_line("F1 / ?", "toggle this help"),
    ];
    let right = vec![
        section("QUEUE & SEARCH"),
        help_line("a / A", "queue one / all results"),
        help_line("d / Del", "remove queue item"),
        help_line("J / K", "move queue item down / up"),
        help_line("g / G", "jump to first / last"),
        help_line("@", "switch YouTube / SoundCloud"),
        help_line("q", "quit"),
        Line::from(Span::raw("")),
        section("QUALITY"),
        help_line("f", "cycle quality filter"),
        help_line("W", "toggle high quality only"),
        help_line("Q / T", "sort by quality / duration"),
        Line::from(Span::raw("")),
        section("PLAYLISTS"),
        help_line("S", "save queue as playlist"),
        help_line("s", "add result / queue item to playlist"),
        help_line("o", "open playlist picker"),
        help_line("Enter/a", "load / append playlist"),
        help_line("e", "browse playlist tracks"),
        help_line("d / r", "delete / rename playlist"),
        Line::from(Span::raw("")),
        section("PANELS"),
        help_line("# / F3", "show/hide lyrics panel"),
        help_line("v", "show/hide visualizer"),
        help_line("Auto", "lyrics follow the current track"),
        Line::from(Span::raw("")),
        section("SOURCES"),
        help_line("Search", "YouTube, SoundCloud"),
        help_line("SoundCloud", "30s previews without login"),
        help_line("Paste URL", "anything yt-dlp supports"),
        help_line("Spotify", "track links matched to YouTube"),
        Line::from(Span::raw("")),
        Line::from(Span::styled(
            "No login. No account. Just yt-dlp, mpv, and LrcLib.",
            Style::default().fg(GREEN).italic(),
        )),
        help_line("Ctrl+C", "quit"),
    ];

    frame.render_widget(Paragraph::new(left).wrap(Wrap { trim: true }), columns[0]);
    frame.render_widget(Paragraph::new(right).wrap(Wrap { trim: true }), columns[1]);
}

fn section<'a>(title: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(title, Style::default().fg(ACCENT).bold()),
        Span::raw(""),
    ])
}

fn help_line<'a>(key: &'a str, description: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {key:<10}"), Style::default().fg(ACCENT).bold()),
        Span::raw(description),
    ])
}

fn spinner(tick: u64) -> &'static str {
    const SPINNERS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    SPINNERS[((tick / 3) % SPINNERS.len() as u64) as usize]
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(MUTED)
    }
}

fn highlight_style(focused: bool) -> Style {
    if focused {
        Style::default().bg(SELECT_BG)
    } else {
        Style::default()
    }
}

fn source_color(source: Source) -> Color {
    match source {
        Source::YouTube => Color::Rgb(220, 60, 60),
        Source::SoundCloud => Color::Rgb(30, 140, 255),
        Source::Bandcamp => Color::Rgb(10, 150, 100),
        Source::Spotify => Color::Rgb(30, 215, 96),
        Source::Web => Color::Rgb(100, 100, 100),
    }
}

fn repeat_label(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::One => "one",
    }
}
