//! The render tree.
//!
//! One-way dependency: everything here reads `&App` and writes `FrameOut`;
//! nothing here mutates application state. One module per screen, so the file
//! to open is the one named after the thing on screen.

mod equalizer;
mod footer;
mod library;
mod lyrics;
mod nowplaying;
mod overlay;
mod queue;
mod visualizer;

pub(crate) use equalizer::*;
pub(crate) use footer::*;
pub(crate) use library::*;
pub(crate) use lyrics::*;
pub(crate) use nowplaying::*;
pub(crate) use overlay::*;
pub(crate) use queue::*;
pub(crate) use visualizer::*;

use crate::*;

pub(crate) fn render(f: &mut Frame, app: &App, out: &mut FrameOut, repaint: ArtRepaint) {
    if repaint == ArtRepaint::Wipe {
        if let Some(previous) = out.art {
            wipe_area(f, previous.intersection(f.area()));
        }
    }
    out.art = None;
    out.spectrum = None;
    let theme = app.theme.displayed;
    let area = f.area();
    f.render_widget(Block::default().style(theme.base()), area);
    let area = area.inner(Margin::new(2, 1));

    let rows = Layout::vertical([
        Constraint::Min(6),    // body (library | active view)
        Constraint::Length(1), // spacer
        Constraint::Length(2), // now-playing strip
        Constraint::Length(2), // footer
    ])
    .split(area);

    let right = if app.view.zen {
        // Hidden, not zero-width: a rendered sidebar still claims mouse rects.
        out.hits.lib = None;
        out.hits.scroll = None;
        rows[0]
    } else {
        let body = Layout::horizontal([Constraint::Percentage(30), Constraint::Min(24)])
            .spacing(3)
            .split(rows[0]);
        render_library(f, app, out, theme, body[0]);
        body[1]
    };
    let right_rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(right);
    render_view_header(f, app.view.mode, out, theme, right_rows[0]);
    let status = if app.session.mutation_uncertain {
        format!("{} · previous library write unconfirmed", app.status)
    } else {
        app.status.clone()
    };
    f.render_widget(Paragraph::new(status).style(theme.muted()), right_rows[1]);
    let right = right_rows[2];
    match app.view.mode {
        RightView::NowPlaying => render_nowplaying_view(f, app, out, theme, right, repaint),
        RightView::Lyrics => render_lyrics(f, app, theme, right),
        RightView::Queue => render_queue_view(f, app, theme, right),
    }

    render_now_strip(f, app, out, theme, rows[2]);
    render_footer(f, app, theme, rows[3]);

    if app.view.actions.is_some() {
        render_actions_overlay(f, app, theme, area);
    } else if app.view.equalizer.is_some() {
        // Keep album art visible: the editor lives in the active pane and
        // positions itself in free space around the cover.
        render_equalizer_overlay(f, app, out, theme, right);
    }
}

pub(crate) fn render_view_header(
    f: &mut Frame,
    mode: RightView,
    out: &mut FrameOut,
    theme: Theme,
    area: Rect,
) {
    out.hits.tabs.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let full_width: usize = RightView::ALL
        .iter()
        .map(|v| v.label().len())
        .sum::<usize>()
        + 6;
    let compact = (area.width as usize) < full_width;
    let labels = if compact {
        ["Playing", "Lyrics", "Queue"]
    } else {
        ["Now Playing", "Lyrics", "Queue"]
    };
    let total = labels.iter().map(|label| label.len()).sum::<usize>() + 6;
    let mut x = area.x;
    // Extremely narrow panes show only the active tab; no invisible hit targets.
    for (index, view) in RightView::ALL.iter().enumerate() {
        if total > area.width as usize && *view != mode {
            continue;
        }
        if x > area.x {
            x = x.saturating_add(3);
        }
        let width = (labels[index].len() as u16).min(area.right().saturating_sub(x));
        if width == 0 {
            continue;
        }
        let rect = Rect::new(x, area.y, width, area.height.min(1));
        let style = if *view == mode {
            theme.heading()
        } else {
            theme.muted()
        };
        f.render_widget(Paragraph::new(labels[index]).style(style), rect);
        out.hits.tabs.push((*view, rect));
        x = x.saturating_add(width);
    }
}

pub(crate) fn spectrum_demand(
    visible: bool,
    mode: RightView,
    overlay: bool,
    footprint: Option<Rect>,
) -> bool {
    visible
        && mode == RightView::NowPlaying
        && !overlay
        && footprint.is_some_and(|rect| rect.width > 0 && rect.height > 0)
}

/// Where the list viewport starts, given where it started last frame.
///
/// Keeps `margin` rows visible either side of the cursor (vim's `scrolloff`).
/// Threading the previous `offset` back in is what makes it sticky: the cursor
/// moves freely inside the window, which follows only when pushed.
pub(crate) fn scroll_offset(
    offset: usize,
    selected: usize,
    cap: usize,
    total: usize,
    margin: usize,
) -> usize {
    if cap == 0 || total <= cap {
        return 0;
    }
    // Wider than half the viewport and the two bounds below would fight.
    let margin = margin.min((cap - 1) / 2);
    offset
        .min(selected.saturating_sub(margin)) // cursor near the top → scroll up
        .max((selected + margin + 1).saturating_sub(cap)) // near the bottom → down
        .min(total - cap)
}

/// Blank `area` and force it out to the terminal, erasing an inline image.
///
/// `ratatui-image` marks the image's cells `Skip` without changing their
/// symbols, and a blank cell compares equal to the blank that was already there
/// — so an ordinary `Clear` writes nothing and the picture survives it.
/// `AlwaysUpdate` is what makes the diff emit them anyway.
pub(crate) fn wipe_area(f: &mut Frame, area: Rect) {
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
                cell.set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
    }
}

/// Keep the terminal's own pixels in `area` by telling the diff to leave those
/// cells alone — what `ratatui-image` does for the image it just drew, without
/// drawing it again.
pub(crate) fn hold_area(f: &mut Frame, area: Rect) {
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
}

/// Force whatever is already in `area` out to the terminal, so an overlay lands
/// on top of an inline image instead of being skipped over it.
pub(crate) fn force_area(f: &mut Frame, area: Rect) {
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
    }
}
