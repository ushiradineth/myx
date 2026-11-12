//! Album-art rendering via `ratatui-image`.
//!
//! Detects the terminal's graphics protocol (kitty / sixel / iTerm2) at
//! startup and falls back to unicode half-blocks so *something* always renders.
//! The encoded protocol is cached per render area — re-encoding only happens when
//! the cover box changes size, keeping the render loop cheap.

use crossterm::cursor::{RestorePosition, SavePosition};
use crossterm::queue;
use image::DynamicImage;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::widgets::Widget;
use ratatui::Frame;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FontSize, Image, Resize};
use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::OnceLock;

pub struct Cover {
    img: DynamicImage,
    picker: Picker,
    /// (area it was encoded for, encoded protocol).
    ///
    /// Behind a `RefCell` so rendering can take `&self`: the TUI is
    /// single-threaded and `Cover::render` runs at most once per cover per
    /// frame, so the borrow never overlaps another one — no reentrancy, no
    /// aliasing. That guarantee is a runtime panic rather than a compile error,
    /// so moving rendering off this thread would have to move the cache too.
    cached: RefCell<Option<(Rect, Protocol)>>,
}

impl Cover {
    /// Build a `Picker` by querying the terminal, falling back to half-blocks.
    ///
    /// Must be called after raw mode is enabled so the query can round-trip, and
    /// before any thread is spawned — see [`untmuxed_sixel_picker`].
    pub fn make_picker(preferred: Option<&str>) -> Picker {
        // An explicit choice beats every heuristic below — that is the whole
        // point of the escape hatch.
        let forced = std::env::var("MYX_PROTOCOL")
            .ok()
            .or_else(|| preferred.map(String::from))
            .and_then(|want| parse_protocol(&want));
        let (detected, font_size) = if forced == Some(ProtocolType::Halfblocks) {
            (None, None)
        } else {
            terminal_capabilities()
        };
        let font_size = font_size
            .or_else(|| {
                crossterm::terminal::window_size()
                    .ok()
                    .and_then(|window| cell_size(&window))
            })
            .unwrap_or_else(|| FontSize::new(10, 20));
        // This constructor accepts known metrics without spawning a stdin reader.
        #[allow(deprecated)]
        let mut picker = Picker::from_fontsize(font_size);
        if let Some(protocol) = detected {
            picker.set_protocol_type(protocol);
        }

        // The cover has to survive a window switch, and sixel is the only
        // protocol tmux stores in its own pane buffer and repaints itself.
        // Everything else rides through as passthrough, untracked, and is gone
        // the moment tmux repaints the pane from that buffer.
        //
        // Sending it *unwrapped* is what makes tmux store it, which also strips
        // the passthrough every other protocol needs — so a forced kitty or
        // iTerm2 must never come out of this branch, or its escapes reach tmux
        // bare and get eaten.
        if forced.is_none_or(|p| p == ProtocolType::Sixel) && tmux_stores_sixel() {
            return untmuxed_sixel_picker(picker.font_size());
        }

        if let Some(proto) = forced {
            picker.set_protocol_type(proto);
            return picker;
        }

        if std::env::var("TERM_PROGRAM").is_ok_and(|t| t.contains("WarpTerminal")) {
            // Warp answers the kitty query but does not place unicode
            // placeholders, which is how `ratatui-image` draws kitty — the cells
            // come out empty and the cover is a see-through hole. WezTerm has
            // the same gap and needs no help here: `ratatui-image` blacklists
            // kitty for it already.
            picker.set_protocol_type(ProtocolType::Iterm2);
        } else if picker.protocol_type() == ProtocolType::Halfblocks && outer_terminal_is_kitty() {
            // Inside tmux the graphics query goes unanswered even when the outer
            // terminal draws images — the cell-size reply still arrives, so it
            // looks like a legitimate halfblocks terminal.
            picker.set_protocol_type(ProtocolType::Kitty);
        }

        picker
    }

    /// Load a cover image from disk. Returns `None` if the file can't be decoded.
    pub fn load(path: &str, picker: Picker) -> Option<Self> {
        let img = image::open(path).ok()?;
        Some(Self::from_image(img, picker))
    }

    /// Build a cover from an already-decoded image (so the caller can also derive
    /// a reactive theme from the same pixels).
    pub fn from_image(img: DynamicImage, picker: Picker) -> Self {
        Self {
            img,
            picker,
            cached: RefCell::new(None),
        }
    }

    /// Render the cover into `area`, re-encoding only when the area changes.
    /// Drop the cached encode so the next render re-encodes and ratatui
    /// sees a fresh cell, forcing retransmission.
    pub fn invalidate_cache(&mut self) {
        *self.cached.borrow_mut() = None;
    }

    /// Whether drawing into `area` would have to re-encode, meaning the image
    /// must go to the terminal again.
    pub fn needs_send(&self, area: Rect) -> bool {
        self.cached
            .borrow()
            .as_ref()
            .map(|(cached_area, _)| *cached_area != area)
            .unwrap_or(true)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if !self.ensure_cached(area) {
            return;
        }
        let cached = self.cached.borrow();
        if let Some((_, protocol)) = &*cached {
            frame.render_widget(Image::new(protocol), area);
        }
    }

    /// Replay the cached protocol straight to a terminal writer.
    ///
    /// This is used after a popup that covered the image closes. Going through
    /// the regular terminal diff can discard a byte-identical image anchor, so
    /// render it against a fresh buffer and send that diff in the same
    /// synchronized update as the popup-free frame.
    pub fn render_direct<W: Write>(&self, writer: &mut W, area: Rect) -> io::Result<()> {
        if !self.ensure_cached(area) {
            return Ok(());
        }

        let cached = self.cached.borrow();
        let Some((_, protocol)) = &*cached else {
            return Ok(());
        };
        let previous = Buffer::empty(area);
        let mut current = Buffer::empty(area);
        Image::new(protocol).render(area, &mut current);

        queue!(writer, SavePosition)?;
        {
            let mut backend = CrosstermBackend::new(&mut *writer);
            backend.draw(previous.diff_iter(&current))?;
        }
        queue!(writer, RestorePosition)?;
        writer.flush()
    }

    /// Make sure the protocol cache matches `area`. A failed encode leaves the
    /// cache empty and both rendering paths become a no-op.
    fn ensure_cached(&self, area: Rect) -> bool {
        if area.width == 0 || area.height == 0 {
            return false;
        }
        let mut cached = self.cached.borrow_mut();
        let needs_encode = cached
            .as_ref()
            .map(|(cached_area, _)| *cached_area != area)
            .unwrap_or(true);

        if needs_encode {
            match self.picker.new_protocol(
                self.img.clone(),
                Size::new(area.width, area.height),
                // SDK artwork can be thumbnail-sized. Fit only shrinks images;
                // Scale also enlarges them while preserving the aspect ratio.
                Resize::Scale(None),
            ) {
                Ok(protocol) => *cached = Some((area, protocol)),
                Err(_) => return false,
            }
        }
        true
    }
}

fn cell_size(window: &crossterm::terminal::WindowSize) -> Option<FontSize> {
    let width = window.width.checked_div(window.columns)?;
    let height = window.height.checked_div(window.rows)?;
    (width > 0 && height > 0).then(|| FontSize::new(width, height))
}

#[cfg(unix)]
fn terminal_capabilities() -> (Option<ProtocolType>, Option<FontSize>) {
    use ratatui_image::picker::cap_parser::{Parser, QueryStdioOptions, Response};
    use std::os::fd::AsFd;
    let Ok(fd) = io::stdin().as_fd().try_clone_to_owned() else {
        return (None, None);
    };
    // Buffered Stdin can prefetch a keystroke beyond the final query response.
    let mut input = std::fs::File::from(fd);
    let mut options = QueryStdioOptions::default();
    if std::env::var_os("WEZTERM_EXECUTABLE").is_some()
        || std::env::var_os("KONSOLE_VERSION").is_some()
    {
        options.blacklist_protocols = vec![ProtocolType::Kitty, ProtocolType::Sixel];
    }
    let query = Parser::query(std::env::var_os("TMUX").is_some(), options);
    let mut output = io::stdout().lock();
    if output
        .write_all(query.as_bytes())
        .and_then(|_| output.flush())
        .is_err()
    {
        return (None, None);
    }
    let responses = read_capabilities(&mut input, std::time::Duration::from_millis(200));
    let mut protocol = None;
    let mut font = None;
    for response in responses {
        match response {
            Response::Kitty => protocol = Some(ProtocolType::Kitty),
            Response::Sixel if protocol.is_none() => protocol = Some(ProtocolType::Sixel),
            Response::CellSize(Some((width, height))) if width > 0 && height > 0 => {
                font = Some(FontSize::new(width, height));
            }
            _ => {}
        }
    }
    (protocol, font)
}

#[cfg(not(unix))]
fn terminal_capabilities() -> (Option<ProtocolType>, Option<FontSize>) {
    (None, None)
}

#[cfg(unix)]
fn read_capabilities(
    input: &mut (impl io::Read + std::os::fd::AsRawFd),
    timeout: std::time::Duration,
) -> Vec<ratatui_image::picker::cap_parser::Response> {
    use ratatui_image::picker::cap_parser::{Parser, Response};
    let deadline = std::time::Instant::now() + timeout;
    let mut parser = Parser::new();
    let mut responses = Vec::new();
    let mut remaining = 4096;
    while remaining > 0 {
        let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
            break;
        };
        let mut pollfd = libc::pollfd {
            fd: input.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the borrowed input owns a valid descriptor and pollfd lives for the call.
        let ready = unsafe {
            libc::poll(
                &mut pollfd,
                1,
                left.as_millis().max(1).min(i32::MAX as u128) as i32,
            )
        };
        if ready < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if ready <= 0 || pollfd.revents & libc::POLLIN == 0 {
            break;
        }
        // Read one byte at a time: a key immediately after the final status
        // reply must stay in the TTY for the event reader.
        let mut bytes = [0; 1];
        let count = match input.read(&mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        remaining -= count;
        for byte in &bytes[..count] {
            for response in parser.push(char::from(*byte)) {
                if response == Response::Status {
                    return responses;
                }
                responses.push(response);
            }
        }
    }
    responses
}

/// The requested protocol, or `None` for anything unrecognised — a typo must
/// fall back to detection rather than to a protocol the terminal can't draw.
fn parse_protocol(want: &str) -> Option<ProtocolType> {
    match want.to_ascii_lowercase().as_str() {
        "kitty" => Some(ProtocolType::Kitty),
        "iterm2" => Some(ProtocolType::Iterm2),
        "sixel" => Some(ProtocolType::Sixel),
        "halfblocks" => Some(ProtocolType::Halfblocks),
        _ => None,
    }
}

/// What tmux says about the client attached *right now*, as
/// `<termfeatures>|<termname>`. Empty outside tmux.
fn tmux_client_info() -> &'static str {
    static INFO: OnceLock<String> = OnceLock::new();
    INFO.get_or_init(|| {
        if std::env::var_os("TMUX").is_none() {
            return String::new();
        }
        std::process::Command::new("tmux")
            .args(["display", "-p", "#{client_termfeatures}|#{client_termname}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    })
}

/// Split a `tmux display` reply into the two things we ask it about: whether the
/// client can store sixel, and whether it is kitty.
fn parse_client_info(raw: &str) -> (bool, bool) {
    match raw.split_once('|') {
        Some((features, term)) => (features.contains("sixel"), term.contains("kitty")),
        None => (false, false),
    }
}

/// Whether this tmux both runs us and can store sixel images itself. tmux only
/// reports `sixel` in its terminal features when it was built with sixel
/// support and the outer terminal advertises it.
fn tmux_stores_sixel() -> bool {
    parse_client_info(tmux_client_info()).0
}

/// Whether the terminal actually drawing our output is kitty.
///
/// Inside tmux this has to come from tmux: `KITTY_WINDOW_ID` records whichever
/// terminal *created* the session and stays in its environment forever, so a
/// session reattached from somewhere else would still claim to be kitty and get
/// kitty escapes it can't draw.
fn outer_terminal_is_kitty() -> bool {
    if std::env::var_os("TMUX").is_some() {
        return parse_client_info(tmux_client_info()).1;
    }
    std::env::var_os("KITTY_WINDOW_ID").is_some()
}

/// A sixel picker that does *not* wrap its escapes in tmux passthrough.
///
/// `ratatui-image` adds that wrapper whenever the environment looks like tmux,
/// which is exactly what stops tmux from parsing the image and keeping it. The
/// markers are hidden only for the moment the picker reads them.
///
/// # Safety
///
/// `set_var` mutates the process-wide environment block, which can reallocate it
/// under a concurrent `getenv` of *any* variable. [`Cover::make_picker`] is
/// therefore called from `main` before the tokio runtime and the player engine
/// exist. Terminal probing is synchronous and has already returned here.
///
/// ponytail: the clean fix is a `ratatui-image` API for opting out of the tmux
/// wrapper; until then, the call-site ordering is the guarantee.
fn untmuxed_sixel_picker(font_size: ratatui_image::FontSize) -> Picker {
    let saved = [
        ("TERM", std::env::var("TERM").ok()),
        ("TERM_PROGRAM", std::env::var("TERM_PROGRAM").ok()),
    ];
    unsafe {
        std::env::set_var("TERM", "xterm-256color");
        std::env::remove_var("TERM_PROGRAM");
    }
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font_size);
    for (key, value) in saved {
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
    picker.set_protocol_type(ProtocolType::Sixel);
    picker
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use std::time::Instant;

    #[cfg(unix)]
    #[test]
    fn capability_timeout_leaves_the_first_key_for_the_event_reader() {
        use std::io::{Read, Write};
        let (mut input, mut terminal) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(read_capabilities(&mut input, std::time::Duration::from_millis(10)).is_empty());
        terminal.write_all(b"v").unwrap();
        let mut key = [0];
        input
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        input.read_exact(&mut key).unwrap();
        assert_eq!(key, *b"v");
    }

    #[cfg(unix)]
    #[test]
    fn synchronous_probe_reads_capabilities_and_stops_on_status() {
        use ratatui_image::picker::cap_parser::Response;
        use std::io::{Read, Write};
        let (mut input, mut terminal) = std::os::unix::net::UnixStream::pair().unwrap();
        input
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        terminal.write_all(b"\x1b[6;20;10t\x1b[0nv").unwrap();
        let responses = read_capabilities(&mut input, std::time::Duration::from_secs(1));
        assert!(responses.contains(&Response::CellSize(Some((10, 20)))));
        let mut key = [0];
        input.read_exact(&mut key).unwrap();
        assert_eq!(key, *b"v");
    }

    #[test]
    fn font_metrics_use_os_pixels_and_reject_unknown_dimensions() {
        let mut window = crossterm::terminal::WindowSize {
            rows: 30,
            columns: 80,
            width: 800,
            height: 600,
        };
        assert_eq!(
            cell_size(&window).map(|size| (size.width, size.height)),
            Some((10, 20))
        );
        window.width = 0;
        assert!(cell_size(&window).is_none());
        window.columns = 0;
        assert!(cell_size(&window).is_none());
    }

    #[test]
    fn a_protocol_name_maps_to_its_protocol_and_a_typo_to_detection() {
        assert_eq!(parse_protocol("Sixel"), Some(ProtocolType::Sixel));
        assert_eq!(parse_protocol("kitty"), Some(ProtocolType::Kitty));
        assert_eq!(parse_protocol("iterm2"), Some(ProtocolType::Iterm2));
        assert_eq!(parse_protocol("halfblocks"), Some(ProtocolType::Halfblocks));
        assert_eq!(parse_protocol("kity"), None);
    }

    #[test]
    fn tmux_reports_what_the_attached_client_can_do() {
        let (sixel, kitty) = parse_client_info("256,RGB,sixel,title|xterm-256color\n");
        assert!(sixel);
        assert!(!kitty);

        let (sixel, kitty) = parse_client_info("256,RGB,title|xterm-kitty\n");
        assert!(!sixel);
        assert!(kitty);

        // Outside tmux there is no reply at all, and nothing may be inferred:
        // guessing kitty here would send escapes a plain terminal prints raw.
        assert_eq!(parse_client_info(""), (false, false));
    }

    #[test]
    fn cached_cover_can_be_replayed_without_a_blank_frame() {
        for striped in [false, true] {
            let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(4, 4, |_, y| {
                if striped && y % 2 == 1 {
                    image::Rgb([220, 120, 24])
                } else {
                    image::Rgb([24, 120, 220])
                }
            }));
            let cover = Cover::from_image(img, Picker::halfblocks());
            let mut output = Vec::new();

            cover
                .render_direct(&mut output, Rect::new(3, 2, 4, 2))
                .expect("direct cover render");

            assert!(!output.is_empty());
            let output = String::from_utf8_lossy(&output);
            if striped {
                assert!(output.contains('▀') || output.contains('▄'));
            } else {
                // Identical pixel halves use colored spaces, not a half-block
                // glyph. Check colors before backend ANSI/color-depth conversion.
                assert!(output.contains(' '));
                let area = Rect::new(3, 2, 4, 2);
                let mut buffer = Buffer::empty(area);
                let cached = cover.cached.borrow();
                Image::new(&cached.as_ref().unwrap().1).render(area, &mut buffer);
                for y in area.y..area.bottom() {
                    for x in area.x..area.right() {
                        assert_eq!(buffer[(x, y)].bg, ratatui::style::Color::Rgb(24, 120, 220));
                    }
                }
            }
            assert!(!cover.needs_send(Rect::new(3, 2, 4, 2)));
        }
    }

    #[test]
    fn small_covers_scale_to_the_artwork_area_in_every_protocol() {
        for protocol in [
            ProtocolType::Halfblocks,
            ProtocolType::Kitty,
            ProtocolType::Iterm2,
            ProtocolType::Sixel,
        ] {
            // No terminal query: fix the cell geometry so the allocated area is
            // 300x300 pixels, larger than the SDK's thumbnail-sized covers.
            #[allow(deprecated)]
            let mut picker = Picker::from_fontsize((10, 20).into());
            picker.set_protocol_type(protocol);
            for (width, height, expected) in [
                (64, 64, Size::new(30, 15)),
                (640, 640, Size::new(30, 15)),
                (64, 32, Size::new(30, 8)),
            ] {
                let cover =
                    Cover::from_image(DynamicImage::new_rgb8(width, height), picker.clone());
                assert!(cover.ensure_cached(Rect::new(3, 2, 30, 15)));
                assert_eq!(
                    cover.cached.borrow().as_ref().unwrap().1.size(),
                    expected,
                    "{protocol:?}: {width}x{height} cover"
                );
            }
        }
    }

    /// What one cover re-encode costs on the UI thread, per protocol. Ignored
    /// because it measures rather than asserts:
    ///   cargo test --lib -- --ignored --nocapture encode_cost
    #[test]
    #[ignore]
    fn encode_cost() {
        let mut img = image::RgbImage::new(640, 640);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let img = DynamicImage::ImageRgb8(img);
        let area = Rect::new(0, 0, 30, 15);

        for proto in [
            ProtocolType::Halfblocks,
            ProtocolType::Kitty,
            ProtocolType::Iterm2,
            ProtocolType::Sixel,
        ] {
            let mut picker = Picker::halfblocks();
            picker.set_protocol_type(proto);
            let mut cover = Cover::from_image(img.clone(), picker);
            let runs = 20;
            let t = Instant::now();
            for _ in 0..runs {
                cover.invalidate_cache();
                let _ = cover.picker.new_protocol(
                    cover.img.clone(),
                    Size::new(area.width, area.height),
                    Resize::Fit(None),
                );
            }
            println!("{proto:?}: {:?} per encode", t.elapsed() / runs);
        }
    }
}
