//! The application state — the thing the other three layers are about.
//!
//! `ui/` reads `&App` and writes `FrameOut`; `input/` mutates `App`; `api/`
//! touches neither and talks HTTP over channels. This module is the state in
//! the middle. One module per part of the model, so the file to open is the one
//! named after it; `App` itself lives here, since every one of those parts
//! hangs off it.
//!
//! It would be tidier if this module depended on none of the others, and it
//! nearly does — with two exceptions, both in `event.rs`, where handling an
//! player event spawns viewer artwork and lyrics work directly. Those
//! reach into `api/`. The intended shape is for `event.rs` to
//! send a request over a channel and let `main.rs` — the wiring layer, which is
//! allowed to know both sides — service it. Until that lands, this is a real
//! edge in the graph, not an aspiration, so don't add more of them.

mod action;
mod event;
mod frame;
mod library;
mod playback;
mod state;

pub(crate) use action::*;
pub(crate) use event::*;
pub(crate) use frame::*;
pub(crate) use library::*;
pub(crate) use playback::*;
pub(crate) use state::*;

pub(crate) struct App {
    pub(crate) svc: Services,
    pub(crate) theme: ThemeState,
    pub(crate) playback: PlaybackState,
    pub(crate) status: String,
    pub(crate) browse: BrowseState,
    pub(crate) transport: Transport,
    pub(crate) search: SearchState,
    pub(crate) view: ViewState,
    pub(crate) session: SessionState,
    // What the album art box owes the next frame. See ArtRepaint.
    pub(crate) art_repaint: ArtRepaint,
}

impl App {
    pub(crate) fn home_groups_visible(&self) -> bool {
        self.browse.section == Section::Home
            && self.browse.details.is_empty()
            && !self.search.searching
    }

    pub(crate) fn row_selectable(&self, index: usize) -> bool {
        self.cur_items()
            .get(index)
            .is_some_and(|item| !item.is_header() || self.home_groups_visible())
    }

    pub(crate) fn cur_items(&self) -> &[LibItem] {
        if let Some(d) = self.browse.details.last() {
            &d.items
        } else if self.search.searching {
            &self.search.search_results
        } else {
            self.browse.library.items(self.browse.section)
        }
    }
    pub(crate) fn cur_list_mut(&mut self) -> &mut Vec<LibItem> {
        if let Some(d) = self.browse.details.last_mut() {
            &mut d.items
        } else if self.search.searching {
            &mut self.search.search_results
        } else {
            self.browse.library.items_mut(self.browse.section)
        }
    }
    /// First actionable row (including Home disclosures).
    pub(crate) fn first_selectable(&self) -> usize {
        self.cur_items()
            .iter()
            .enumerate()
            .position(|(index, _)| self.row_selectable(index))
            .unwrap_or(0)
    }
    /// Move the selection by `dir`, skipping static labels, clamped at the ends.
    pub(crate) fn move_sel(&mut self, dir: isize) {
        let items = self.cur_items();
        let n = items.len() as isize;
        if n == 0 {
            return;
        }
        let mut i = self.browse.selected as isize;
        loop {
            i += dir;
            if i < 0 || i >= n {
                return;
            }
            if self.row_selectable(i as usize) {
                self.browse.selected = i as usize;
                return;
            }
        }
    }
    /// Repair a stale cursor or one that landed on a static label after delivery.
    pub(crate) fn normalize_selection(&mut self) {
        if !self.row_selectable(self.browse.selected) {
            self.browse.selected = self.first_selectable();
        }
    }
    /// The single entry point for "play this context URI".
    ///
    /// Every caller must route through here so `source_name` stays in
    /// sync with what is actually playing — they back the Queue view's
    /// PLAYING FROM header.
    /// `name` is a parameter rather than being derived from `details.last()`
    /// because the drill-in stack is empty when playing straight from a list.
    pub(crate) fn play_context_row(&mut self, uri: String, name: String, shuffle: bool) {
        self.status = format!("starting {name}…");
        self.transport.source_name = name;
        if let Err(e) = self.svc.engine.play_context(uri, shuffle) {
            self.status = format!("couldn't play: {e:#}");
        }
    }

    /// Play whatever's selected (in the current section, or in search results).
    /// Act on the selected item. Returns what the caller should do next.
    pub(crate) fn activate(&mut self) -> Activated {
        let Some(item) = self.cur_items().get(self.browse.selected).cloned() else {
            return Activated::None;
        };
        if item.is_header() {
            if self.home_groups_visible() {
                self.browse.library.toggle_home_group(self.browse.selected);
            }
            return Activated::None;
        }
        if item.is_play() {
            // Special synthetic rows: play the Liked list (optionally shuffled).
            if item.uri == "myx:action:liked-play" {
                self.transport.source_name = "Liked Songs".to_string();
                self.status = "starting Liked Songs…".to_string();
                if let Err(e) = self
                    .svc
                    .engine
                    .play_liked(None, None, 0, self.transport.shuffle)
                {
                    self.status = format!("couldn't play: {e:#}");
                }
                return Activated::None;
            }
            // Inside a drill-in the enclosing title is the better label
            // ("Chill Vibes"); standalone play rows fall back to their own.
            let name = self
                .browse
                .details
                .last()
                .map(|d| d.title.clone())
                .unwrap_or_else(|| item.name.clone());
            let shuffle = self.transport.shuffle;
            self.play_context_row(item.uri, name, shuffle);
            return Activated::None;
        }
        if item.is_track() {
            self.status = format!("starting {}…", item.name);
            let detail = self.browse.details.last();
            let command = selected_track_command(&item, detail, self.transport.shuffle);
            self.transport.source_name = detail
                .filter(|_| item.source_position.is_some())
                .map(|detail| detail.title.clone())
                .unwrap_or_else(|| item.name.clone());
            if let Err(e) = self.svc.engine.command(command) {
                self.status = format!("couldn't play: {e:#}");
            }
            return Activated::None;
        }
        // Otherwise it's a context (artist / album / playlist) — open it.
        self.status = format!("opening {}…", item.name);
        Activated::Open(item.uri, item.name)
    }
}

fn selected_track_command(
    item: &LibItem,
    detail: Option<&Detail>,
    shuffle: bool,
) -> myx::domain::PlayerCommand {
    if let Some((detail, position)) = detail.zip(item.source_position) {
        return myx::domain::PlayerCommand::PlayContextAt {
            uri: detail.context_uri.clone(),
            track: Some(item.uri.clone()),
            row_position: position,
            position_ms: 0,
            shuffle,
        };
    }
    // History, Home and Liked rows are selections, not Spotify contexts. The
    // SDK playable-URI route does not depend on unreliable URI-array offsets.
    myx::domain::PlayerCommand::PlayContext {
        uri: item.uri.clone(),
        track: None,
        position_ms: 0,
        shuffle,
    }
}

#[cfg(test)]
mod selected_track_tests {
    use super::*;
    use myx::domain::PlayerCommand;

    #[test]
    fn bare_track_selection_is_exact_even_with_shuffle_duplicates_or_source_coordinates() {
        let duplicate = LibItem::track("A".into(), String::new(), "spotify:track:a".into());
        let selected = LibItem::track("B".into(), String::new(), "spotify:track:b".into())
            .at_source_position(1024);
        let rows = [duplicate.clone(), selected, duplicate];
        for row in &rows {
            for shuffle in [false, true] {
                assert!(matches!(selected_track_command(row, None, shuffle),
                    PlayerCommand::PlayContext { uri, track: None, position_ms: 0, shuffle: actual }
                    if uri == row.uri && actual == shuffle));
            }
        }
    }

    #[test]
    fn album_and_playlist_rows_keep_their_original_context_coordinates() {
        let row = LibItem::track("Selected".into(), String::new(), "spotify:track:b".into())
            .at_source_position(42);
        for kind in ["album", "playlist"] {
            let detail = Detail {
                context_uri: format!("spotify:{kind}:collection"),
                title: "Collection".into(),
                items: Vec::new(),
                parent_selected: 0,
            };
            assert!(matches!(selected_track_command(&row, Some(&detail), true),
                PlayerCommand::PlayContextAt { uri, track: Some(track), row_position: 42, position_ms: 0, shuffle: true }
                if uri == detail.context_uri && track == row.uri));
        }
    }

    #[test]
    fn popular_rows_are_playables_not_artist_context_offsets() {
        let row = LibItem::track("Selected".into(), String::new(), "spotify:track:b".into());
        let detail = Detail {
            context_uri: "spotify:artist:artist".into(),
            title: "Artist".into(),
            items: Vec::new(),
            parent_selected: 0,
        };
        assert!(matches!(selected_track_command(&row, Some(&detail), false),
            PlayerCommand::PlayContext { uri, track: None, .. } if uri == row.uri));
    }
}
