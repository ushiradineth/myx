//! Terminal key input — the main keymap.

use crate::*;

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
    if app.view.zen && drives_library(code) {
        return false;
    }

    match code {
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
            app.transport.volume = (app.transport.volume + 5).min(100);
            let _ = app.svc.engine.set_volume(vol_u16(app.transport.volume));
        }
        KeyCode::Char('-') | KeyCode::Char('_') | KeyCode::Media(MediaKeyCode::LowerVolume) => {
            app.transport.volume = app.transport.volume.saturating_sub(5);
            let _ = app.svc.engine.set_volume(vol_u16(app.transport.volume));
        }
        KeyCode::Char('s') => {
            app.transport.shuffle = !app.transport.shuffle;
            let _ = app.svc.engine.shuffle(app.transport.shuffle);
        }
        // Play the highlighted playlist / album / artist outright. Enter still
        // opens; this is the direct route that used to require two Enters or
        // the actions menu.
        KeyCode::Char('P') => play_selected_context(app, false),
        KeyCode::Char('S') => {
            // Keep the visible toggle consistent with this explicit request.
            app.transport.shuffle = true;
            let _ = app.svc.engine.shuffle(true);
            play_selected_context(app, true);
        }
        KeyCode::Char('R') => {
            app.transport.repeat = match app.transport.repeat {
                RepeatMode::Off => RepeatMode::All,
                RepeatMode::All => RepeatMode::One,
                RepeatMode::One => RepeatMode::Off,
            };
            let _ = app.svc.engine.set_repeat(app.transport.repeat);
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
        // Tab / Shift+Tab (and [ ]) rotate the library sections.
        KeyCode::Tab | KeyCode::Char(']') => {
            app.session.search_request.cancel();
            app.session.detail_request.cancel();
            app.browse.details.clear();
            app.search.searching = false;
            app.browse.section = app.browse.section.shift(1);
            app.browse.selected = app.first_selectable();
        }
        KeyCode::BackTab | KeyCode::Char('[') => {
            app.session.search_request.cancel();
            app.session.detail_request.cancel();
            app.browse.details.clear();
            app.search.searching = false;
            app.browse.section = app.browse.section.shift(-1);
            app.browse.selected = app.first_selectable();
        }
        // Arrow keys rotate the right-pane view; Shift+arrows seek ±5s.
        KeyCode::Right if mods.contains(KeyModifiers::SHIFT) => {
            app.playback.seek_step(SEEK_STEP_MS)
        }
        KeyCode::Left if mods.contains(KeyModifiers::SHIFT) => {
            app.playback.seek_step(-SEEK_STEP_MS)
        }
        KeyCode::Right => {
            app.view.mode = app.view.mode.shift(1);
            if app.view.mode == RightView::Queue && app.transport.playback_started {
                request_visible_queue(app, chans);
            }
        }
        KeyCode::Left => {
            app.view.mode = app.view.mode.shift(-1);
            if app.view.mode == RightView::Queue && app.transport.playback_started {
                request_visible_queue(app, chans);
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
    if let Err(e) = app.svc.engine.request_queue() {
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
        KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Enter
            | KeyCode::Esc
            | KeyCode::Char('/' | '[' | ']' | 'j' | 'k' | 'o' | 'r' | 'P' | 'S')
    )
}
