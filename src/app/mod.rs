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
    /// First non-header index (where a fresh selection should land).
    pub(crate) fn first_selectable(&self) -> usize {
        self.cur_items()
            .iter()
            .position(|i| !i.is_header())
            .unwrap_or(0)
    }
    /// Move the selection by `dir`, skipping header rows, clamped at the ends.
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
            if !items[i as usize].is_header() {
                self.browse.selected = i as usize;
                return;
            }
        }
    }
    /// If the selection landed on a header (e.g. after data loads), bump it off.
    pub(crate) fn normalize_selection(&mut self) {
        if self
            .cur_items()
            .get(self.browse.selected)
            .is_some_and(|i| i.is_header())
        {
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
            if self.search.searching {
                self.play_context_row(item.uri, item.name, false);
                return Activated::None;
            }
            // Inside a drill-in → play its context at this track (real queue).
            if let Some(d) = self.browse.details.last() {
                let ctx = d.context_uri.clone();
                self.transport.source_name = d.title.clone();
                self.status = format!("starting {}…", item.name);
                let result = if let Some(position) = item.source_position {
                    self.svc.engine.play_context_at_position(
                        ctx,
                        Some(item.uri.clone()),
                        position,
                        0,
                        self.transport.shuffle,
                    )
                } else {
                    // Artist Popular rows are direct playables, not album/playlist offsets.
                    self.svc.engine.play_track_at(item.uri.clone(), 0)
                };
                if let Err(e) = result {
                    self.status = format!("couldn't play: {e:#}");
                }
                return Activated::None;
            }
            // Section track list.
            self.status = format!("starting {}…", item.name);
            let result = if self.browse.section == Section::Liked {
                self.transport.source_name = "Liked Songs".to_string();
                self.svc.engine.play_liked(
                    Some(item.uri.clone()),
                    item.source_position,
                    0,
                    self.transport.shuffle,
                )
            } else {
                self.transport.source_name = self.browse.section.label().to_string();
                let start_index = self.cur_items()[..self.browse.selected]
                    .iter()
                    .filter(|row| row.is_track())
                    .count();
                let uris = self
                    .cur_items()
                    .iter()
                    .filter(|row| row.is_track())
                    .map(|row| row.uri.clone())
                    .collect();
                match u32::try_from(start_index) {
                    Ok(index) => {
                        self.svc
                            .engine
                            .play_tracks_at_index(uris, index, 0, self.transport.shuffle)
                    }
                    Err(_) => Err(anyhow::anyhow!("Selection index exceeds playback bounds")),
                }
            };
            if let Err(e) = result {
                self.status = format!("couldn't play: {e:#}");
            }
            return Activated::None;
        }
        // Otherwise it's a context (artist / album / playlist) — open it.
        self.status = format!("opening {}…", item.name);
        Activated::Open(item.uri, item.name)
    }
}
