//! The library browser's data: sections, rows, sort order, and drill-ins.

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RightView {
    NowPlaying,
    Lyrics,
    Queue,
}

impl RightView {
    pub(crate) const ALL: [RightView; 3] =
        [RightView::NowPlaying, RightView::Lyrics, RightView::Queue];
    pub(crate) fn label(self) -> &'static str {
        match self {
            RightView::NowPlaying => "Now Playing",
            RightView::Lyrics => "Lyrics",
            RightView::Queue => "Queue",
        }
    }
    pub(crate) fn shift(self, delta: isize) -> RightView {
        let i = RightView::ALL.iter().position(|&v| v == self).unwrap_or(0) as isize;
        let n = RightView::ALL.len() as isize;
        RightView::ALL[(i + delta).rem_euclid(n) as usize]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Section {
    Home,
    Recent,
    Playlists,
    Liked,
    Albums,
    Artists,
}

impl Section {
    pub(crate) const ALL: [Section; 6] = [
        Section::Home,
        Section::Liked,
        Section::Playlists,
        Section::Albums,
        Section::Artists,
        Section::Recent,
    ];
    pub(crate) fn label(self) -> &'static str {
        match self {
            Section::Home => "Home",
            Section::Recent => "Recent",
            Section::Playlists => "Playlists",
            Section::Liked => "Liked",
            Section::Albums => "Albums",
            Section::Artists => "Artists",
        }
    }
    pub(crate) fn index(self) -> usize {
        Section::ALL.iter().position(|&s| s == self).unwrap_or(0)
    }
    pub(crate) fn shift(self, delta: isize) -> Section {
        let n = Section::ALL.len() as isize;
        let i = (self.index() as isize + delta).rem_euclid(n) as usize;
        Section::ALL[i]
    }
}

/// What kind of thing a library row represents. Replaces the old
/// `is_track` / `is_header` / `is_play` boolean triple — only one of the four
/// states was ever true at a time, but booleans couldn't enforce that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemKind {
    /// A playable track (song / episode).
    Track,
    /// A non-selectable section label (e.g. "Popular", "Albums").
    Header,
    /// A synthetic "▶︎ Play …" row that plays its URI as a context.
    Play,
    /// A navigable context: playlist, album, or artist.
    Context,
}

/// A library entry. Behavior on Enter is driven by the `kind` field:
/// Header = non-selectable label; Track = play as a track list; Play = play this
/// URI as a context; Context = open (drill into) this context.
#[derive(Clone)]
pub(crate) struct LibItem {
    pub(crate) name: String,
    pub(crate) subtitle: String,
    pub(crate) uri: String,
    pub(crate) kind: ItemKind,
    pub(crate) order: u32, // original fetch position (for the "Added" sort)
}

impl LibItem {
    pub(crate) fn track(name: String, subtitle: String, uri: String) -> Self {
        Self {
            name,
            subtitle,
            uri,
            kind: ItemKind::Track,
            order: 0,
        }
    }
    pub(crate) fn ctx(name: String, subtitle: String, uri: String) -> Self {
        Self {
            name,
            subtitle,
            uri,
            kind: ItemKind::Context,
            order: 0,
        }
    }
    pub(crate) fn play(name: String, uri: String) -> Self {
        Self {
            name,
            subtitle: String::new(),
            uri,
            kind: ItemKind::Play,
            order: 0,
        }
    }
    pub(crate) fn header(name: &str) -> Self {
        Self {
            name: name.to_string(),
            subtitle: String::new(),
            uri: String::new(),
            kind: ItemKind::Header,
            order: 0,
        }
    }

    /// Convenience: is this row a playable track?
    pub(crate) fn is_track(&self) -> bool {
        self.kind == ItemKind::Track
    }
    /// Convenience: is this row a non-selectable header?
    pub(crate) fn is_header(&self) -> bool {
        self.kind == ItemKind::Header
    }
    /// Convenience: is this row a synthetic "Play" action?
    pub(crate) fn is_play(&self) -> bool {
        self.kind == ItemKind::Play
    }
}

/// Sort order for browsable lists.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortMode {
    Added,
    Title,
    Artist,
}

impl SortMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            SortMode::Added => "added",
            SortMode::Title => "title",
            SortMode::Artist => "artist",
        }
    }
    pub(crate) fn next(self) -> SortMode {
        match self {
            SortMode::Added => SortMode::Title,
            SortMode::Title => SortMode::Artist,
            SortMode::Artist => SortMode::Added,
        }
    }
}

/// Sort a list in place, keeping leading header/play rows pinned at the top.
pub(crate) fn sort_list(items: &mut [LibItem], mode: SortMode) {
    let pin = items
        .iter()
        .take_while(|i| i.is_header() || i.is_play())
        .count();
    let tail = &mut items[pin..];
    match mode {
        SortMode::Added => tail.sort_by_key(|i| i.order),
        SortMode::Title => tail.sort_by_key(|i| i.name.to_lowercase()),
        SortMode::Artist => tail.sort_by_key(|i| i.subtitle.to_lowercase()),
    }
}

/// A drill-in detail view (artist / album / playlist contents).
pub(crate) struct Detail {
    pub(crate) context_uri: String,
    pub(crate) title: String,
    pub(crate) items: Vec<LibItem>,
    pub(crate) parent_selected: usize,
}

/// Result of activating (Enter on) a library item.
pub(crate) enum Activated {
    None,
    Open(String, String), // drill into a context (uri, name)
}

#[derive(Default, Clone)]
pub(crate) struct Library {
    pub(crate) home: Vec<LibItem>,
    pub(crate) recent: Vec<LibItem>,
    pub(crate) playlists: Vec<LibItem>,
    pub(crate) liked: Vec<LibItem>,
    pub(crate) albums: Vec<LibItem>,
    pub(crate) artists: Vec<LibItem>,
    // Sections that have received a delivery since boot / the last refresh.
    // An undelivered section is *loading*, not empty — the UI renders the
    // difference (issue #25). Lives here because `set` is the one delivery
    // point for every section.
    loaded: std::collections::HashSet<Section>,
    requests: std::collections::HashMap<Section, super::RequestGate>,
    errors: std::collections::HashMap<Section, crate::api::ApiError>,
}

impl Library {
    pub(crate) fn items(&self, s: Section) -> &[LibItem] {
        match s {
            Section::Home => &self.home,
            Section::Recent => &self.recent,
            Section::Playlists => &self.playlists,
            Section::Liked => &self.liked,
            Section::Albums => &self.albums,
            Section::Artists => &self.artists,
        }
    }
    pub(crate) fn items_mut(&mut self, s: Section) -> &mut Vec<LibItem> {
        match s {
            Section::Home => &mut self.home,
            Section::Recent => &mut self.recent,
            Section::Playlists => &mut self.playlists,
            Section::Liked => &mut self.liked,
            Section::Albums => &mut self.albums,
            Section::Artists => &mut self.artists,
        }
    }
    pub(crate) fn set(&mut self, s: Section, items: Vec<LibItem>) {
        self.errors.remove(&s);
        self.loaded.insert(s);
        match s {
            Section::Home => self.home = items,
            Section::Recent => self.recent = items,
            Section::Playlists => self.playlists = items,
            Section::Liked => self.liked = items,
            Section::Albums => self.albums = items,
            Section::Artists => self.artists = items,
        }
    }

    /// Has this section received a delivery since boot / the last refresh?
    pub(crate) fn is_loaded(&self, s: Section) -> bool {
        self.loaded.contains(&s)
    }

    /// Invalidate membership after a confirmed or uncertain mutation. Preserve
    /// old visible data until a complete, successful replacement arrives.
    pub(crate) fn invalidate(&mut self) {
        self.loaded.clear();
        self.errors.clear();
        for gate in self.requests.values_mut() {
            gate.cancel();
        }
    }
    /// Unlike an ordinary refresh, an authority change must not retain rows
    /// from the old account, including sections that are currently hidden.
    pub(crate) fn clear_authority(&mut self) {
        self.invalidate();
        for section in Section::ALL {
            self.items_mut(section).clear();
        }
    }
    pub(crate) fn error(&self, section: Section) -> Option<&crate::api::ApiError> {
        self.errors.get(&section)
    }
    pub(crate) fn request(&mut self, section: Section, force: bool) -> Option<u64> {
        let gate = self.requests.entry(section).or_default();
        if !force
            && (gate.pending()
                || self.loaded.contains(&section)
                || self.errors.contains_key(&section))
        {
            return None;
        }
        self.errors.remove(&section);
        Some(gate.start())
    }
    pub(crate) fn receive(
        &mut self,
        section: Section,
        reply: crate::api::Reply<Vec<LibItem>>,
    ) -> bool {
        if !self.requests.entry(section).or_default().finish(reply.id) {
            return false;
        }
        match reply.result {
            Ok(items) => self.set(section, items),
            Err(error) => {
                self.errors.insert(section, error);
            }
        }
        true
    }
}

#[cfg(test)]
mod loaded_tracking_tests {
    use super::*;

    #[test]
    fn sections_start_unloaded_and_load_on_delivery() {
        let mut lib = Library::default();
        assert!(!lib.is_loaded(Section::Liked));
        lib.set(Section::Liked, Vec::new()); // empty delivery still counts
        assert!(lib.is_loaded(Section::Liked));
        assert!(!lib.is_loaded(Section::Albums)); // others untouched
    }

    #[test]
    fn failed_refresh_preserves_valid_rows_and_does_not_retry_each_frame() {
        let mut lib = Library::default();
        lib.set(Section::Home, vec![LibItem::header("valid rows")]);
        let id = lib.request(Section::Home, true).unwrap();
        lib.receive(
            Section::Home,
            crate::api::Reply {
                id,
                result: Err(crate::api::ApiError::Restricted),
            },
        );
        assert_eq!(lib.items(Section::Home)[0].name, "valid rows");
        assert!(lib.error(Section::Home).is_some());
        assert!(lib.request(Section::Home, false).is_none());
    }
}
