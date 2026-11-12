//! Key hints grouped by navigation and playback, in their keyboard order.

use crate::*;

pub(crate) fn render_footer(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let enter = if app.home_groups_visible()
        && app
            .cur_items()
            .get(app.browse.selected)
            .is_some_and(LibItem::is_header)
    {
        if app.browse.library.home_group_expanded(app.browse.selected) {
            "fold"
        } else {
            "expand"
        }
    } else {
        enter_label(app.cur_items().get(app.browse.selected))
    };
    let waveform = if app.view.waveform_visible {
        "waveform on"
    } else {
        "waveform off"
    };
    let mut navigation = Vec::new();
    if !app.view.zen {
        navigation.push(("←→", "section"));
    }
    navigation.extend([("⇥/⇧⇥", "view"), ("v", waveform), ("z", "zen")]);
    if !app.view.zen {
        navigation.extend([
            ("↑↓", "move"),
            ("⏎", enter),
            ("P", "play context"),
            ("/", "search"),
            ("a", "actions"),
            ("r", "reload"),
            ("Esc", "back"),
        ]);
        if !app.home_groups_visible() {
            navigation.push(("o", "sort"));
        }
    } else {
        navigation.push(("a", "actions"));
    }
    let playback = vec![
        ("␣", "play/pause"),
        ("n/b", "skip"),
        (
            "S",
            if app.transport.shuffle {
                "shuffle on"
            } else {
                "shuffle off"
            },
        ),
        (
            "R",
            match app.transport.repeat {
                RepeatMode::Off => "repeat off",
                RepeatMode::One => "repeat one",
                RepeatMode::All => "repeat all",
            },
        ),
        ("⇧←→", "seek"),
        ("+/-", "vol"),
        ("e", "eq"),
        ("q", "quit"),
    ];
    let lines: Vec<Line> = [navigation, playback]
        .into_iter()
        .map(|hints| {
            let mut spans = Vec::new();
            let mut used = 0;
            for (key, label) in hints {
                let width = Line::from(format!("{key} {label}  ")).width();
                if used + width > area.width as usize {
                    continue;
                }
                used += width;
                spans.push(Span::styled(key, Style::default().fg(theme.primary.into())));
                spans.push(Span::styled(format!(" {label}  "), theme.muted()));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}
