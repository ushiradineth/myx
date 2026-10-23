//! Input while the actions menu overlay is open, plus the clipboard escape
//! hatch its "copy link" entries use.

use crate::*;

/// Handle input while the actions menu is open.
pub(crate) fn handle_action_key(app: &mut App, code: KeyCode, chans: &UiChannels) {
    match code {
        KeyCode::Esc | KeyCode::Char('a') => {
            app.session.menu_request.cancel();
            app.session.menu_item = None;
            app.view.actions = None;
            app.status.clear();
            return;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(m) = app.view.actions.as_mut() {
                m.selected = m.selected.saturating_sub(1);
            }
            return;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(m) = app.view.actions.as_mut() {
                m.selected = (m.selected + 1).min(m.items.len().saturating_sub(1));
            }
            return;
        }
        KeyCode::Enter => {}
        _ => return,
    }

    // Enter: act on the selected entry.
    let kind = app
        .view
        .actions
        .as_ref()
        .and_then(|m| m.items.get(m.selected))
        .map(|i| i.kind.clone());
    let Some(kind) = kind else { return };
    match kind {
        ActionKind::AddToPlaylistMenu { track_uri } => {
            app.session.menu_item = None;
            app.session.menu_error = None;
            let id = app.session.menu_request.start();
            app.view.actions = Some(ActionMenu {
                title: "Loading playlists…".into(),
                items: Vec::new(),
                selected: 0,
            });
            spawn_playlist_menu(app.svc.webapi.clone(), track_uri, id, chans.menu.clone());
        }
        ActionKind::Play { uri, name } => {
            // Previously called engine.play_context directly, leaving
            // source/source_name stale: PLAYING FROM showed the wrong context
            // and resume-on-launch replayed the previous one.
            let shuffle = app.transport.shuffle;
            app.play_context_row(uri, name, shuffle);
            app.view.actions = None;
        }
        ActionKind::Open { uri, name } => {
            app.session.search_request.cancel();
            app.search.in_flight = false;
            let id = app.session.detail_request.start();
            spawn_detail_fetch(app.svc.webapi.clone(), uri, name, id, chans.detail.clone());
            app.view.actions = None;
        }
        ActionKind::CopyLink { uri } => {
            app.status = if copy_to_clipboard(&uri_to_url(&uri)) {
                "link copied".to_string()
            } else {
                "clipboard unavailable".to_string()
            };
            app.view.actions = None;
        }
        other => {
            if app.session.action_request.pending() {
                app.status = "library write awaiting confirmation".into();
                return;
            }
            let id = app.session.action_request.start();
            spawn_action(
                &app.svc.engine,
                app.svc.webapi.clone(),
                other,
                id,
                chans.astatus.clone(),
            );
            app.view.actions = None;
        }
    }
    if app.view.actions.is_none() {
        app.session.menu_request.cancel();
        app.session.menu_item = None;
    }
}

/// Copy text to the system clipboard via whatever tool is available.
pub(crate) fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: [(&str, &[&str]); 4] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["-b", "-i"]),
        ("pbcopy", &[]),
    ];
    for (cmd, args) in candidates {
        if let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut sin) = child.stdin.take() {
                let _ = sin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return true;
        }
    }
    false
}
