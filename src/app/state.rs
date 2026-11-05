//! The rest of `App`'s parts: services, theme, transport, browse, search, view, session.

use crate::*;

/// Long-lived services the UI talks to. All three are used through `&self`
/// (the `Arc<Mutex<_>>` is only ever cloned), so grouping them costs no
/// borrow flexibility.
pub(crate) struct Services {
    pub(crate) engine: Player,
    pub(crate) picker: Picker,
    pub(crate) webapi: Arc<Mutex<WebApi>>,
}

pub(crate) const FADE_MS: u64 = 1500;

/// The palette currently on screen, plus the cross-fade walking it towards
/// the incoming track's palette. `displayed` is what every widget reads;
/// `target` is only used to snap exactly on completion.
pub(crate) struct ThemeState {
    pub(crate) displayed: Theme,
    pub(crate) target: Theme,
    pub(crate) fade: Option<ThemeFade>,
}

/// The palette before any cover has arrived, with the background as configured.
pub(crate) fn startup_theme() -> Theme {
    Theme {
        transparent: myx::config::get().transparent,
        ..TOKYONIGHT
    }
}

impl ThemeState {
    pub(crate) fn start_fade(&mut self, to: Theme) {
        // A cover's palette knows nothing of the config; the startup flag rides along.
        let to = Theme {
            transparent: self.target.transparent,
            ..to
        };
        self.fade = Some(ThemeFade::new(
            self.displayed,
            to,
            Duration::from_millis(FADE_MS),
        ));
        self.target = to;
    }

    /// Flip the background between the terminal's and the palette's. A running
    /// fade is restarted towards the same palette so it lands with the new flag.
    pub(crate) fn set_transparent(&mut self, on: bool) {
        self.displayed.transparent = on;
        self.target.transparent = on;
        if self.fade.is_some() {
            self.start_fade(self.target);
        }
    }

    pub(crate) fn advance(&mut self) {
        if let Some(fade) = &self.fade {
            self.displayed = fade.current();
            if fade.is_done() {
                self.displayed = self.target;
                self.fade = None;
            }
        }
    }
}

/// The daemon's transport read model, with optimistic local control feedback.
pub(crate) struct Transport {
    pub(crate) canonical: Box<Snapshot>,
    pub(crate) queue_offset: usize,
    pub(crate) queue_total: usize,
    pub(crate) queue_revision: u64,
    pub(crate) shuffle: bool,
    pub(crate) repeat: RepeatMode,
    pub(crate) volume: u8, // 0..=100 (mirrors the 50% mixer default)
    pub(crate) queue: Vec<String>,
    pub(crate) queue_uris: Vec<String>,
    // Whether real playback has started this session (gates resume-on-play).
    pub(crate) playback_started: bool,
    // Current browser request label, not playback restoration state.
    pub(crate) source_name: String,
    /// Daemon DSP settings displayed by the equalizer controls.
    pub(crate) equalizer: EqualizerSettings,
}

impl Transport {
    /// Attaching is a read. Only subsequent explicit input may send controls.
    pub(crate) fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            canonical: Box::new(snapshot.clone()),
            queue_offset: 0,
            queue_total: snapshot.queue_upcoming_total,
            queue_revision: snapshot.queue_revision,
            shuffle: snapshot.shuffle,
            repeat: snapshot.repeat,
            volume: snapshot.volume,
            queue: snapshot
                .queue
                .upcoming
                .iter()
                .map(|entry| {
                    entry
                        .title
                        .clone()
                        .unwrap_or_else(|| "Loading metadata…".into())
                })
                .collect(),
            queue_uris: snapshot
                .queue
                .upcoming
                .iter()
                .map(|entry| entry.uri.clone())
                .collect(),
            playback_started: snapshot.track.is_some(),
            source_name: String::new(),
            equalizer: snapshot.equalizer,
        }
    }
}

/// The library browser: what's loaded, where the cursor is, and the drill-in
/// stack. The viewport offset is not here — it lives in `FrameOut`, since the
/// renderer owns it across frames.
pub(crate) struct BrowseState {
    pub(crate) library: Library,
    pub(crate) section: Section,
    pub(crate) selected: usize,
    pub(crate) sort: SortMode,
    // Drill-in stack (artist → album → …). Topmost is what's shown.
    pub(crate) details: Vec<Detail>,
}

/// The `/` search overlay: whether the prompt is capturing keys, the typed
/// query, and the results that temporarily replace the library list.
pub(crate) struct SearchState {
    pub(crate) input_mode: bool,
    // The prompt's editor. Read through `query()`, reset through `clear()`.
    pub(crate) input: tui_textarea::TextArea<'static>,
    pub(crate) searching: bool,
    // A submitted query whose results have not landed yet. `searching` means
    // "the search view is active"; this means "the wire is hot" — the empty
    // list renders "searching…" instead of "(empty)" while it's set.
    pub(crate) in_flight: bool,
    pub(crate) search_results: Vec<LibItem>,
}

impl SearchState {
    /// The typed query. First line only — the prompt is single-line (Enter is
    /// intercepted), so this also defuses any newline a paste might smuggle in.
    pub(crate) fn query(&self) -> &str {
        self.input.lines().first().map_or("", String::as_str)
    }

    /// Empty the editor (fresh buffer, cursor at column 0).
    pub(crate) fn clear(&mut self) {
        self.input = tui_textarea::TextArea::default();
    }
}

/// What the user is looking at: the right pane's mode, the zen (sidebar
/// hidden) toggle, the lyrics backing the Lyrics view, and the actions
/// overlay drawn on top of everything.
pub(crate) struct ViewState {
    // Which view fills the right pane.
    pub(crate) mode: RightView,
    // Sidebar hidden, so the right view (and its cover) gets the whole width.
    pub(crate) zen: bool,
    // Lyrics: (timestamp_ms, line). Synced when timestamps are non-zero.
    pub(crate) lyrics: Vec<(u32, String)>,
    pub(crate) lyrics_synced: bool,
    // Context actions menu overlay (opened with `a`).
    pub(crate) actions: Option<ActionMenu>,
    // Ten-band equalizer overlay (opened with `e`).
    pub(crate) equalizer: Option<EqualizerOverlay>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EqualizerOverlay {
    pub(crate) selected_band: usize,
}

/// Cross-cutting session bookkeeping: which metadata fetch is still in flight
/// and the input timestamps that make Ctrl-C and double-click work.
#[derive(Default)]
pub(crate) struct SessionState {
    pub(crate) authority: Option<CatalogAuthority>,
    // Track URI whose metadata was last requested. Fetches run on separate
    // blocking tasks and can land out of order when skipping quickly, so a
    // reply for any other track is stale and must be dropped.
    pub(crate) pending_meta: Option<String>,
    // Timestamp of last Ctrl-C — a second press within 1.5s quits.
    pub(crate) last_ctrl_c: Option<Instant>,
    pub(crate) last_click: Option<(u16, Instant)>,
    pub(crate) search_request: RequestGate,
    pub(crate) detail_request: RequestGate,
    pub(crate) menu_request: RequestGate,
    pub(crate) action_request: RequestGate,
    pub(crate) artwork_request: RequestGate,
    pub(crate) lyrics_request: RequestGate,
    pub(crate) lyrics_key: Option<(String, String, String, String, u32)>,
    pub(crate) search_error: Option<ApiError>,
    pub(crate) lyrics_error: Option<ApiError>,
    pub(crate) menu_error: Option<ApiError>,
    pub(crate) search_notice: Option<String>,
    pub(crate) mutation_uncertain: bool,
    pub(crate) menu_item: Option<LibItem>,
}

/// Opaque daemon authority, never an account identifier or an OAuth credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogAuthority {
    pub(crate) session_id: String,
    pub(crate) auth_revision: u64,
}

impl CatalogAuthority {
    pub(crate) fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            session_id: snapshot.session_id.clone(),
            auth_revision: snapshot.auth_revision,
        }
    }
}

impl SessionState {
    /// Account-derived navigation is closed on replacement, so only the selected
    /// section is demand-loaded again. Keep the gates themselves: resetting their
    /// counters would let an old id collide with the first new-account request.
    pub(crate) fn observe_authority(
        &mut self,
        snapshot: &Snapshot,
        browse: &mut BrowseState,
        search: &mut SearchState,
        view: &mut ViewState,
    ) -> bool {
        let authority = CatalogAuthority::from_snapshot(snapshot);
        if self.authority.as_ref() == Some(&authority) {
            return false;
        }
        self.authority = Some(authority);
        for gate in [
            &mut self.search_request,
            &mut self.detail_request,
            &mut self.menu_request,
            &mut self.action_request,
            &mut self.artwork_request,
            &mut self.lyrics_request,
        ] {
            gate.cancel();
        }
        self.pending_meta = Some(
            snapshot
                .track
                .as_ref()
                .map(|track| track.uri.clone())
                .unwrap_or_default(),
        );
        self.lyrics_key = None;
        self.search_error = None;
        self.lyrics_error = None;
        self.menu_error = None;
        self.search_notice = None;
        self.mutation_uncertain = false;
        self.menu_item = None;
        self.last_click = None;
        browse.library.clear_authority();
        browse.details.clear();
        browse.selected = 0;
        search.search_results.clear();
        search.searching = false;
        search.input_mode = false;
        search.in_flight = false;
        view.actions = None;
        view.lyrics.clear();
        view.lyrics_synced = false;
        true
    }
}

/// A generation is separate from a URI/query: opening the same target again
/// must still reject work from an earlier visit, including its errors.
#[derive(Default, Clone)]
pub(crate) struct RequestGate {
    generation: u64,
    active: Option<u64>,
}

impl RequestGate {
    pub(crate) fn start(&mut self) -> u64 {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("request generation exhausted");
        self.active = Some(self.generation);
        self.generation
    }
    pub(crate) fn cancel(&mut self) {
        self.active = None;
    }
    pub(crate) fn pending(&self) -> bool {
        self.active.is_some()
    }
    pub(crate) fn finish(&mut self, id: u64) -> bool {
        if self.active != Some(id) {
            return false;
        }
        self.active = None;
        true
    }
}
