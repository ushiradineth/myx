//! Offline state, key-route and Ratatui geometry checks for the requested TUI refinements.
use crate::ui::*;
use crate::*;
use ratatui::{backend::TestBackend, Terminal};

fn home() -> Vec<LibItem> {
    let mut rows = Vec::new();
    for (group, count) in [("Recent", 8), ("Top Tracks", 7), ("Top Artists", 3)] {
        rows.push(LibItem::header(group));
        for index in 0..count {
            rows.push(
                LibItem::track(
                    format!("{group} {index}"),
                    "Artist".into(),
                    // Deliberate duplicate URI: disclosure must not deduplicate rows.
                    "spotify:track:same".into(),
                )
                .at_source_position(index),
            );
        }
    }
    rows
}

#[test]
fn folding_is_a_five_row_projection_without_losing_source_identity_or_request_state() {
    let mut library = Library::default();
    let original = home();
    library.set(Section::Home, original.clone());
    assert_eq!(library.items(Section::Home).len(), 16); // 3 headers + 5 + 5 + 3
    assert!(library.request(Section::Home, false).is_none());
    library.toggle_home_group(0);
    assert_eq!(library.items(Section::Home).len(), 19);
    assert!(library.home_group_expanded(0));
    assert_eq!(library.items(Section::Home)[8].source_position, Some(7));
    // The next header moved after expansion; it still toggles the correct source group.
    library.toggle_home_group(9);
    assert_eq!(library.items(Section::Home).len(), 21);
    for (projected, source) in library.items(Section::Home).iter().zip(&original) {
        assert_eq!(projected.uri, source.uri);
        assert_eq!(projected.name, source.name);
        assert_eq!(projected.source_position, source.source_position);
    }
    library.toggle_home_group(0);
    assert_eq!(library.items(Section::Home).len(), 18);
    assert!(library.home_group_expanded(6));
    library.toggle_home_group(6);
    assert_eq!(library.items(Section::Home).len(), 16);
    assert_eq!(library.home.len(), original.len());
    assert!(library.request(Section::Home, false).is_none());
}

#[test]
fn replacement_and_authority_reset_never_resurrect_hidden_account_rows() {
    let mut library = Library::default();
    library.set(Section::Home, home());
    library.toggle_home_group(0);
    library.set(
        Section::Home,
        vec![
            LibItem::header("New"),
            LibItem::track("Only".into(), "".into(), "new".into()),
        ],
    );
    assert_eq!(library.items(Section::Home).len(), 2);
    library.clear_authority();
    assert!(library.items(Section::Home).is_empty());
    assert!(library.home.is_empty());
    library.set(Section::Home, home());
    assert!(!library.home_group_expanded(0));
}

#[test]
fn key_routes_separate_sections_views_seek_shuffle_and_explicit_context_play() {
    use UiKeyAction::*;
    for (code, expected) in [
        (KeyCode::Left, Section(-1)),
        (KeyCode::Right, Section(1)),
        (KeyCode::Tab, View(1)),
        (KeyCode::BackTab, View(-1)),
        (KeyCode::Char('S'), Shuffle),
        (KeyCode::Char('P'), PlayContext),
        (KeyCode::Char('v'), Waveform),
    ] {
        assert_eq!(ui_key_action(code, KeyModifiers::empty()), Some(expected));
    }
    assert_eq!(
        ui_key_action(KeyCode::Char('s'), KeyModifiers::empty()),
        None
    );
    assert_eq!(
        ui_key_action(KeyCode::Right, KeyModifiers::SHIFT),
        Some(Seek(5000))
    );
    assert_eq!(
        ui_key_action(KeyCode::Left, KeyModifiers::SHIFT),
        Some(Seek(-5000))
    );
    assert!(crate::app::Section::Home.shift(-1) == crate::app::Section::Recent);
    assert!(RightView::NowPlaying.shift(-1) == RightView::Queue);
    assert!(RightView::Queue.shift(1) == RightView::NowPlaying);
}

#[test]
fn rendered_view_labels_and_mouse_targets_agree_at_normal_and_narrow_widths() {
    for width in [1, 6, 12, 24, 32, 80] {
        for mode in RightView::ALL {
            let mut terminal = Terminal::new(TestBackend::new(width + 4, 4)).unwrap();
            let mut out = FrameOut::default();
            let area = Rect::new(2, 1, width, 1);
            terminal
                .draw(|f| render_view_header(f, mode, &mut out, TOKYONIGHT, area))
                .unwrap();
            assert!(!out.hits.tabs.is_empty());
            for (_, hit) in &out.hits.tabs {
                assert_eq!(hit.intersection(area), *hit);
                assert!(hit.width > 0);
                assert_ne!(terminal.backend().buffer()[(hit.x, hit.y)].symbol(), " ");
            }
            for pair in out.hits.tabs.windows(2) {
                assert!(pair[0].1.right() <= pair[1].1.x);
            }
            if width < 24 {
                assert_eq!(out.hits.tabs.len(), 1);
            }
        }
    }
}

#[test]
fn waveform_hide_reclaims_space_and_disables_demand_even_before_the_next_frame() {
    let area = Rect::new(4, 2, 60, 30);
    let shown = nowplaying_regions(area, true);
    let hidden = nowplaying_regions(area, false);
    assert_eq!(shown[1].height, 7);
    assert_eq!(hidden[0], area);
    assert_eq!(hidden[1].height, 0);
    assert_eq!(hidden[0].height - shown[0].height, 9);
    assert!(spectrum_demand(
        true,
        RightView::NowPlaying,
        false,
        Some(shown[1])
    ));
    // A stale rendered footprint cannot keep subscription alive after v.
    assert!(!spectrum_demand(
        false,
        RightView::NowPlaying,
        false,
        Some(shown[1])
    ));
    for mode in RightView::ALL {
        assert!(!spectrum_demand(true, mode, true, Some(shown[1])));
        assert!(!spectrum_demand(true, mode, false, None));
    }
    assert!(!spectrum_demand(
        true,
        RightView::Lyrics,
        false,
        Some(shown[1])
    ));
    assert!(!spectrum_demand(
        true,
        RightView::Queue,
        false,
        Some(shown[1])
    ));
    assert!(!spectrum_demand(
        true,
        RightView::NowPlaying,
        false,
        Some(hidden[1])
    ));
}

#[test]
fn waveform_fixture_renders_real_bars_only_in_its_visible_footprint() {
    for width in [1, 12, 60] {
        let area = Rect::new(2, 1, width, 20);
        let mut terminal = Terminal::new(TestBackend::new(width + 4, 24)).unwrap();
        let shown = nowplaying_regions(area, true);
        terminal
            .draw(|f| render_spectrum(f, &[1.0; NUM_BANDS], 1.0, TOKYONIGHT, shown[1]))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut bars = 0;
        for y in 0..24 {
            for x in 0..width + 4 {
                if buffer[(x, y)].symbol() != " " {
                    bars += 1;
                    assert!(shown[1].contains((x, y).into()));
                }
            }
        }
        assert!(bars > 0);
        let hidden = nowplaying_regions(area, false);
        terminal
            .draw(|f| render_spectrum(f, &[1.0; NUM_BANDS], 1.0, TOKYONIGHT, hidden[1]))
            .unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .all(|cell| cell.symbol() == " "));
    }
}
