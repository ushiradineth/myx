//! Terminal key input — the main keymap.

use crate::*;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UiKeyAction {
    Section(isize),
    View(isize),
    Seek(i64),
    Shuffle,
    PlayContext,
    Waveform,
}

pub(crate) fn ui_key_action(code: KeyCode, mods: KeyModifiers) -> Option<UiKeyAction> {
    Some(match code {
        KeyCode::Left if mods.contains(KeyModifiers::SHIFT) => UiKeyAction::Seek(-SEEK_STEP_MS),
        KeyCode::Right if mods.contains(KeyModifiers::SHIFT) => UiKeyAction::Seek(SEEK_STEP_MS),
        KeyCode::Left | KeyCode::Char('[') => UiKeyAction::Section(-1),
        KeyCode::Right | KeyCode::Char(']') => UiKeyAction::Section(1),
        KeyCode::Tab => UiKeyAction::View(1),
        KeyCode::BackTab => UiKeyAction::View(-1),
        KeyCode::Char('S') => UiKeyAction::Shuffle,
        KeyCode::Char('P') => UiKeyAction::PlayContext,
        KeyCode::Char('v') => UiKeyAction::Waveform,
        _ => return None,
    })
}

/// Returns true if the app should quit.
pub(crate) fn handle_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    chans: &UiChannels,
) -> bool {
    // --- Actions menu captures input while open ---
    // Double-press Ctrl-C to quit (works from anywhere). Single press arms it.
    if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
        let now = Instant::now();
        if app
            .session
            .last_ctrl_c
            .map(|t| now.duration_since(t) < Duration::from_millis(1500))
            .unwrap_or(false)
        {
            return true;
        }
        app.session.last_ctrl_c = Some(now);
        app.status = "press Ctrl-C again to quit".to_string();
        return false;
    }

    if app.view.actions.is_some() {
        handle_action_key(app, code, chans);
        return false;
    }

    if app.view.equalizer.is_some() {
        handle_equalizer_key(app, code);
        return false;
    }

    // --- Search input mode captures everything ---
    if app.search.input_mode {
        match code {
            KeyCode::Esc => app.search.input_mode = false,
            KeyCode::Enter => {
                app.search.input_mode = false;
                let q = app.search.query().trim().to_string();
                if !q.is_empty() {
                    app.browse.details.clear();
                    app.search.searching = true;
                    app.search.in_flight = true;
                    app.browse.selected = 0;
                    app.status = "searching…".to_string();
                    app.session.detail_request.cancel();
                    let id = app.session.search_request.start();
                    app.session.search_error = None;
                    spawn_search(app.svc.webapi.clone(), q, id, chans.search.clone());
                }
            }
            // Ctrl-U clears the query — readline muscle memory. The fork
            // binds Ctrl-U to undo; shadow it (same call as agent-runtime).
            KeyCode::Char('u') if mods.contains(crossterm::event::KeyModifiers::CONTROL) => {
                app.search.clear();
            }
            // Everything else — typing, cursor movement, word ops — is the
            // editor's business. Enter is intercepted above, so no newlines.
            _ => {
                app.search
                    .input
                    .input(crossterm::event::KeyEvent::new(code, mods));
            }
        }
        return false;
    }

    // Zen hides the library, so the keys that drive one do nothing rather than
    // moving a selection nobody can see. Placed after the overlays above, which
    // stay usable if one was already open when zen came on.
    let seek = matches!(code, KeyCode::Left | KeyCode::Right) && mods.contains(KeyModifiers::SHIFT);
    if app.view.zen && drives_library(code) && !seek {
        return false;
    }

    if let Some(action) = ui_key_action(code, mods) {
        match action {
            UiKeyAction::Section(delta) => {
                app.session.search_request.cancel();
                app.session.detail_request.cancel();
                app.browse.details.clear();
                app.search.searching = false;
                app.browse.section = app.browse.section.shift(delta);
                app.browse.selected = app.first_selectable();
            }
            UiKeyAction::View(delta) => {
                app.view.mode = app.view.mode.shift(delta);
                if app.view.mode == RightView::Queue && app.transport.playback_started {
                    request_visible_queue(app, chans);
                }
            }
            UiKeyAction::Seek(delta) => app.playback.seek_step(delta),
            UiKeyAction::Shuffle => {
                let _ = app.svc.engine.shuffle(!app.transport.shuffle);
            }
            UiKeyAction::PlayContext => play_selected_context(app, false),
            UiKeyAction::Waveform => {
                app.view.waveform_visible = !app.view.waveform_visible;
                app.art_repaint = ArtRepaint::Wipe;
            }
        }
        return false;
    }

    match code {
        KeyCode::PageDown | KeyCode::PageUp if app.view.mode == RightView::Queue => {
            let offset = if code == KeyCode::PageDown {
                app.transport
                    .queue_offset
                    .saturating_add(10)
                    .min(app.transport.queue_total.saturating_sub(1) / 10 * 10)
            } else {
                app.transport.queue_offset.saturating_sub(10)
            };
            if let Err(error) = app
                .svc
                .engine
                .request_queue_page(Some(app.transport.queue_revision), offset)
            {
                app.status = format!("queue page: {error}");
            } else {
                app.transport.queue_offset = offset;
                app.status = "loading queue page…".into();
            }
        }
        KeyCode::Char('e') => {
            app.view.equalizer = Some(EqualizerOverlay::default());
        }
        KeyCode::Char('/') => {
            app.session.search_request.cancel();
            app.session.detail_request.cancel();
            app.search.in_flight = false;
            app.search.input_mode = true;
            app.search.clear();
        }
        KeyCode::Char('q') => return true,
        KeyCode::Esc => {
            app.session.detail_request.cancel();
            app.session.search_request.cancel();
            app.search.in_flight = false;
            if let Some(d) = app.browse.details.pop() {
                app.browse.selected = d.parent_selected;
            } else if app.search.searching {
                app.search.searching = false;
                app.browse.selected = 0;
            }
            // Nothing to back out of — Esc no longer quits (use q or Ctrl-C twice).
        }
        KeyCode::Char(' ') | KeyCode::Char('p') | KeyCode::Media(MediaKeyCode::PlayPause) => {
            if let Err(error) = app.svc.engine.toggle() {
                app.status = format!("playback control failed: {error}");
            }
        }
        KeyCode::Media(MediaKeyCode::Stop) => {
            app.svc.engine.stop();
        }
        KeyCode::Char('n') | KeyCode::Media(MediaKeyCode::TrackNext) => {
            let _ = app.svc.engine.next();
        }
        KeyCode::Char('b') | KeyCode::Media(MediaKeyCode::TrackPrevious) => {
            let _ = app.svc.engine.prev();
        }
        KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Media(MediaKeyCode::RaiseVolume) => {
            let _ = app
                .svc
                .engine
                .set_volume(vol_u16((app.transport.volume + 5).min(100)));
        }
        KeyCode::Char('-') | KeyCode::Char('_') | KeyCode::Media(MediaKeyCode::LowerVolume) => {
            let _ = app
                .svc
                .engine
                .set_volume(vol_u16(app.transport.volume.saturating_sub(5)));
        }
        KeyCode::Char('R') => {
            let repeat = match app.transport.repeat {
                RepeatMode::Off => RepeatMode::All,
                RepeatMode::All => RepeatMode::One,
                RepeatMode::One => RepeatMode::Off,
            };
            let _ = app.svc.engine.set_repeat(repeat);
        }
        KeyCode::Char('r') => {
            if let Some(detail) = app.browse.details.last() {
                let (uri, name) = (detail.context_uri.clone(), detail.title.clone());
                let id = app.session.detail_request.start();
                spawn_detail_fetch(app.svc.webapi.clone(), uri, name, id, chans.detail.clone());
            } else if app.search.searching {
                let query = app.search.query().to_owned();
                let id = app.session.search_request.start();
                app.search.in_flight = true;
                app.session.search_error = None;
                spawn_search(app.svc.webapi.clone(), query, id, chans.search.clone());
            } else {
                request_library_section(app, chans, true);
            }
        }
        KeyCode::Char('o') => {
            if app.home_groups_visible() {
                return false;
            }
            app.browse.sort = app.browse.sort.next();
            let m = app.browse.sort;
            sort_list(app.cur_list_mut(), m);
            app.browse.selected = app.first_selectable();
            app.status = format!("sorted by {}", m.label());
        }
        KeyCode::Char('a') => {
            // Zen hides the library, so the menu belongs to what is playing —
            // acting on a selection nobody can see is how it ends up offering
            // "remove from Liked" for the wrong track.
            let item = if app.view.zen {
                app.playback
                    .now
                    .as_ref()
                    .filter(|n| !n.uri.is_empty())
                    .map(|n| LibItem::track(n.title.clone(), n.artist.clone(), n.uri.clone()))
            } else {
                app.cur_items().get(app.browse.selected).cloned()
            };
            if let Some(item) = item {
                if !item.is_header() && !item.is_play() {
                    // Instant menu (no network), then enrich when the API returns.
                    app.view.actions = build_action_menu(None, &item).ok();
                    app.session.menu_error = None;
                    app.session.menu_item = Some(item.clone());
                    let id = app.session.menu_request.start();
                    spawn_action_menu(app.svc.webapi.clone(), item, id, chans.menu.clone());
                }
            }
        }
        // The frame loop notices the layout change and wipes the art box.
        KeyCode::Char('z') => app.view.zen = !app.view.zen,
        // Shifted on purpose: a stray press repaints everything and rewrites
        // config.toml.
        KeyCode::Char('T') => {
            let on = !app.theme.target.transparent;
            app.theme.set_transparent(on);
            app.status = match myx::config::Config::save_transparent(on) {
                Ok(()) => format!("transparent background {}", if on { "on" } else { "off" }),
                Err(e) => format!("background switched, but config.toml was not saved: {e}"),
            };
        }
        KeyCode::Down | KeyCode::Char('j') => app.move_sel(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_sel(-1),
        // Needs a terminal that reports modified Enter (kitty, WezTerm, foot).
        KeyCode::Enter if mods.contains(KeyModifiers::SHIFT) => play_selected_context(app, false),
        KeyCode::Enter => match app.activate() {
            Activated::Open(uri, name) => {
                app.session.search_request.cancel();
                app.search.in_flight = false;
                let id = app.session.detail_request.start();
                spawn_detail_fetch(app.svc.webapi.clone(), uri, name, id, chans.detail.clone());
            }
            Activated::None => {}
        },
        _ => {}
    }
    false
}

fn request_visible_queue(app: &mut App, _chans: &UiChannels) {
    if let Err(e) = app.svc.engine.request_queue_page(
        Some(app.transport.queue_revision),
        app.transport.queue_offset,
    ) {
        app.status = format!("queue refresh failed: {e}");
    }
}

/// Keys whose whole effect is on the library pane.
///
/// Zen hides that pane, so these do nothing there rather than moving a selection
/// nobody can see — and `a` is deliberately absent, because it retargets onto
/// the playing track instead of going quiet.
pub(crate) fn drives_library(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Enter
            | KeyCode::Esc
            | KeyCode::Char('/' | '[' | ']' | 'j' | 'k' | 'o' | 'r' | 'P')
    )
}
