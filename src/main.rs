//! Attachable terminal viewer and bounded command client for the Myx daemon.

mod api;
mod app;
mod input;
mod ui;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MediaKeyCode, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::buffer::CellDiffOption;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;
use ratatui_image::picker::Picker;

use api::*;
use app::*;
use input::*;
use myx::anim::ThemeFade;
use myx::audio::{
    EqualizerPreset, EqualizerSettings, EQ_FREQUENCIES_HZ, MAX_EQ_GAIN_DB, MIN_EQ_GAIN_DB,
    NUM_BANDS, NUM_EQ_BANDS,
};
use myx::cli::Cli;
use myx::client::{Connection, Player, WebApi};
use myx::components::{gradient_line, gradient_progress, left_bar_block};
use myx::config::Config;
use myx::cover::Cover;
use myx::domain::{PlaybackStatus, RepeatMode, Snapshot};
use myx::gradient::{self};
use myx::liblog::{install_logger, liblog};
use myx::lyrics::parse::parse_lrc;
use myx::reactive::derive_theme;
use myx::soloist::protocol::{ObservedMetadata as TrackMetadata, PlayerEvent as EngineEvent};
use myx::term::{init_terminal, restore_terminal, Term};
use myx::theme::{Theme, TOKYONIGHT};
use myx::util::{center_v, fmt_ms, track_id_from_uri, truncate, uri_to_url, urlencode, vol_u16};
use ui::{render, render_loading};

struct Startup {
    connection: Connection,
    snapshot: Snapshot,
    uri: Option<String>,
    volume: Option<u8>,
}

#[cfg(unix)]
struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }
    async fn recv(&mut self) -> io::Result<()> {
        tokio::select! {
            _ = self.interrupt.recv() => {},
            _ = self.terminate.recv() => {},
            _ = self.hangup.recv() => {},
        }
        Ok(())
    }
}

#[cfg(not(unix))]
struct ShutdownSignals;
#[cfg(not(unix))]
impl ShutdownSignals {
    fn new() -> io::Result<Self> {
        Ok(Self)
    }
    async fn recv(&mut self) -> io::Result<()> {
        tokio::signal::ctrl_c().await
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse_from(std::env::args())?;
    if cli.help {
        println!("myx [OPTIONS] [SPOTIFY_URI_OR_URL]\n  daemon run|start|stop|restart|status\n  service install|uninstall|status\n  config init|check|migrate\n  --config PATH          Select configuration\n  --output PATH          New config candidate (required for migrate)\n  --non-interactive      Never open browser authorization\n  --headless --duration SECONDS  Observe playback without a terminal\n  --volume 0..100        Set playback volume\n  --json                 Machine-readable management status\n  --help, --version");
        return Ok(());
    }
    if cli.version {
        println!("myx {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    anyhow::ensure!(
        !std::env::var_os("MYX_BACKEND").is_some_and(|value| !value.is_empty()),
        "MYX_BACKEND is a removed option; unset it and use --help for supported commands"
    );
    myx::config::init_path(cli.config.as_deref())?;
    install_logger();
    let _ = rustls::crypto::ring::default_provider().install_default();
    if myx::client::handle_management(&cli)? {
        return Ok(());
    }
    myx::config::get().validate()?;
    let prepare = || -> Result<Startup> {
        let connection =
            Connection::ensure(&Config::path().context("resolve configuration path")?)?;
        let snapshot = connection.prepare(!cli.non_interactive && !cli.headless)?;
        Ok(Startup {
            connection,
            snapshot,
            uri: cli.uri.clone(),
            volume: cli.volume,
        })
    };
    if cli.headless {
        let startup = prepare()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        return runtime.block_on(run_headless(
            startup,
            cli.duration.context("headless duration required")?,
        ));
    }
    let (startup, mut terminal) = auth_then_terminal(prepare, init_terminal)?;
    let picker = Cover::make_picker(myx::config::get().protocol.as_deref());
    let result = (|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .context("start tokio runtime")?;
        runtime.block_on(boot(&mut terminal, startup, picker))
    })();
    let restored = restore_terminal(&mut terminal);
    result?;
    restored
}

// Setup must finish before the alternate screen can hide an authorization URL.
fn auth_then_terminal<A, T, Auth, Init>(auth: Auth, init_terminal: Init) -> Result<(A, T)>
where
    Auth: FnOnce() -> Result<A>,
    Init: FnOnce() -> Result<T>,
{
    let authenticated = auth()?;
    let terminal = init_terminal()?;
    Ok((authenticated, terminal))
}

async fn boot(terminal: &mut Term, startup: Startup, picker: Picker) -> Result<()> {
    terminal.draw(|frame| render_loading(frame, "attaching to daemon", 0))?;
    let shutdown = ShutdownSignals::new()?;
    let (ev_tx, ev_rx) = flume::unbounded::<EngineEvent>();
    let engine = Player::connect(startup.connection.clone(), ev_tx)?;
    let webapi = Arc::new(Mutex::new(WebApi::new(startup.connection)));
    let snapshot = startup.snapshot;
    let app = App {
        svc: Services {
            engine,
            picker,
            webapi,
        },
        playback: PlaybackState::from_snapshot(&snapshot, myx::domain::playback::now_ms()),
        theme: ThemeState {
            displayed: startup_theme(),
            target: startup_theme(),
            fade: None,
        },
        status: snapshot
            .message
            .clone()
            .unwrap_or_else(|| "loading library…".into()),
        browse: BrowseState {
            library: Library::default(),
            section: Section::Home,
            selected: 0,
            sort: SortMode::Added,
            details: Vec::new(),
        },
        transport: Transport::from_snapshot(&snapshot),
        search: SearchState {
            input_mode: false,
            input: Default::default(),
            searching: false,
            in_flight: false,
            search_results: Vec::new(),
        },
        view: ViewState {
            mode: RightView::NowPlaying,
            zen: false,
            lyrics: Vec::new(),
            lyrics_synced: false,
            actions: None,
            equalizer: None,
        },
        session: SessionState {
            authority: Some(CatalogAuthority::from_snapshot(&snapshot)),
            pending_meta: snapshot.track.as_ref().map(|track| track.uri.clone()),
            ..Default::default()
        },
        art_repaint: ArtRepaint::Idle,
    };
    // Only explicit CLI intent writes controls. Attach never restores client state.
    if let Some(volume) = startup.volume {
        app.svc.engine.set_volume(vol_u16(volume))?;
    }
    if let Some(uri) = startup.uri {
        app.svc
            .engine
            .play_context(uri, false)
            .context("start requested playback")?;
    }
    run_ui(terminal, app, ev_rx, shutdown, snapshot.track).await
}

async fn run_headless(startup: Startup, duration: Duration) -> Result<()> {
    let mut shutdown = ShutdownSignals::new()?;
    let (tx, rx) = flume::unbounded();
    let player = Player::connect(startup.connection, tx)?;
    if let Some(volume) = startup.volume {
        player.set_volume(vol_u16(volume))?;
    }
    if let Some(uri) = startup.uri {
        player.play_context(uri, false)?;
    }
    let deadline = tokio::time::sleep(duration);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            signal = shutdown.recv() => { signal?; break; },
            event = rx.recv_async() => match event.context("daemon subscription closed")? {
                EngineEvent::BackendStatus { message } => eprintln!("myx: {message}"),
                EngineEvent::Metadata(meta) => println!("myx: {} — {}", meta.artist, meta.title),
                _ => {},
            }
        }
    }
    // Dropping a client detaches its watcher, never the daemon or playback.
    Ok(())
}

struct UiChannels {
    meta: flume::Sender<Reply<TrackMeta>>,
    lib: flume::Sender<LibraryReply>,
    search: flume::Sender<Reply<SearchResults>>,
    lyrics: flume::Sender<Reply<LyricsResult>>,
    detail: flume::Sender<Reply<DetailResult>>,
    menu: flume::Sender<Reply<ActionMenu>>,
    astatus: flume::Sender<Reply<ActionOutcome>>,
}

fn request_library_section(app: &mut App, chans: &UiChannels, force: bool) {
    if app.search.searching || !app.browse.details.is_empty() {
        return;
    }
    let section = app.browse.section;
    if let Some(id) = app.browse.library.request(section, force) {
        app.status = format!("loading {}…", section.label());
        spawn_library_fetch(app.svc.webapi.clone(), section, id, chans.lib.clone());
    }
}

async fn run_ui(
    terminal: &mut Term,
    mut app: App,
    ev_rx: flume::Receiver<EngineEvent>,
    mut shutdown: ShutdownSignals,
    initial_track: Option<myx::domain::TrackMetadata>,
) -> Result<()> {
    let (in_tx, in_rx) = flume::unbounded::<io::Result<Event>>();
    let input_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct InputGuard(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for InputGuard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let _input_guard = InputGuard(input_done.clone());
    std::thread::spawn(move || {
        while !input_done.load(std::sync::atomic::Ordering::Relaxed) {
            let result = match event::poll(Duration::from_millis(200)) {
                Ok(false) => continue,
                Ok(true) => event::read(),
                Err(error) => Err(error),
            };
            let failed = result.is_err();
            if in_tx.send(result).is_err() || failed {
                break;
            }
        }
    });
    let (meta_tx, meta_rx) = flume::unbounded::<Reply<TrackMeta>>();
    let (lib_tx, lib_rx) = flume::unbounded::<LibraryReply>();
    let (search_tx, search_rx) = flume::unbounded::<Reply<SearchResults>>();
    let (lyrics_tx, lyrics_rx) = flume::unbounded::<Reply<LyricsResult>>();
    let (detail_tx, detail_rx) = flume::unbounded::<Reply<DetailResult>>();
    let (menu_tx, menu_rx) = flume::unbounded::<Reply<ActionMenu>>();
    let (astatus_tx, astatus_rx) = flume::unbounded::<Reply<ActionOutcome>>();
    let chans = UiChannels {
        meta: meta_tx,
        lib: lib_tx,
        search: search_tx,
        lyrics: lyrics_tx,
        detail: detail_tx,
        menu: menu_tx,
        astatus: astatus_tx,
    };
    request_library_section(&mut app, &chans, false);
    if let Some(track) = initial_track {
        let tx = chans.meta.clone();
        let webapi = app.svc.webapi.clone();
        let id = app.session.artwork_request.start();
        tokio::task::spawn_blocking(move || {
            let meta = fetch_sdk_artwork(
                &webapi,
                TrackMetadata {
                    uri: track.uri,
                    title: track.title.unwrap_or_default(),
                    artist: track
                        .artists
                        .map(|artists| artists.join(", "))
                        .unwrap_or_default(),
                    album: track.album.unwrap_or_default(),
                    duration_ms: track.duration_ms.unwrap_or(0).min(u64::from(u32::MAX)) as u32,
                    cover_url: track.artwork_url,
                },
            );
            let _ = tx.send(Reply {
                id,
                result: Ok(meta),
            });
        });
    }
    let in_tmux = std::env::var_os("TMUX").is_some();
    let mut frame = tokio::time::interval(Duration::from_millis(16));
    frame.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_draw = Instant::now() - IDLE_REDRAW;
    let mut last_sync = Instant::now();
    let mut dirty = true;
    let mut last_layout = (app.view.mode, app.view.zen);
    let mut overlay_open = app.view.actions.is_some();
    let mut restore_art = false;
    let mut spectrum_enabled = false;
    let mut out = FrameOut::default();
    loop {
        let touched = tokio::select! {
            biased;
            signal = shutdown.recv() => { signal?; break; },
            _ = frame.tick() => {
                // Project the original daemon timestamp/speed on each frame.
                // Supplemental UI work must not restart the canonical clock.
                app.playback.apply_snapshot(&app.transport.canonical, myx::domain::playback::now_ms());
                if let Some(result) = app.svc.engine.take_queue_page() {
                    match result {
                        Ok(page) if page.session_id == app.transport.canonical.session_id
                            && page.auth_revision == app.transport.canonical.auth_revision
                            && page.revision == app.transport.queue_revision
                            && page.offset == app.transport.queue_offset => {
                            app.transport.queue_total = page.total;
                            app.transport.queue = page.entries.iter().map(|entry| entry.title.clone().unwrap_or_else(|| "Loading metadata…".into())).collect();
                            app.transport.queue_uris = page.entries.into_iter().map(|entry| entry.uri).collect();
                            app.status = "native queue page loaded (catalog completeness unknown)".into();
                            dirty = true;
                        }
                        Ok(_) => {}
                        Err(error) => { app.status = format!("queue page: {error}"); dirty = true; }
                    }
                }
                request_library_section(&mut app, &chans, false);
                if app.view.mode == RightView::Lyrics { request_lyrics(&mut app, &chans.lyrics); }
                app.playback.flush_seek(&app.svc.engine, Instant::now());
                let spectrum_visible = app.view.mode == RightView::NowPlaying
                    && app.view.actions.is_none() && app.view.equalizer.is_none()
                    && out.spectrum.is_some();
                if spectrum_visible != spectrum_enabled {
                    app.svc.engine.set_spectrum_enabled(spectrum_visible);
                    spectrum_enabled = spectrum_visible;
                }
                while let Ok(LibraryReply { section, reply }) = lib_rx.try_recv() {
                    let delivered = reply.result.is_ok();
                    if !app.browse.library.receive(section, reply) { continue; }
                    dirty = true;
                    if delivered {
                        let items = app.browse.library.items_mut(section);
                        for (i, item) in items.iter_mut().enumerate() { item.order = i as u32; }
                        sort_list(app.browse.library.items_mut(section), app.browse.sort);
                    }
                    if section == app.browse.section && !app.search.searching && app.browse.details.is_empty() {
                        app.normalize_selection();
                        app.status = app.browse.library.error(section)
                            .map(|error| format!("{}: {error}; press r to reload", section.label()))
                            .unwrap_or_else(|| format!("loaded {}", section.label()));
                    }
                }
                let animating = app.theme.fade.is_some()
                    || (app.view.mode == RightView::Lyrics && app.view.lyrics_synced)
                    || (spectrum_enabled && app.svc.engine.bands.try_lock().map(|g| g.is_active).unwrap_or(false));
                if app.art_repaint != ArtRepaint::Idle { dirty = true; }
                if (app.view.mode, app.view.zen) != last_layout {
                    last_layout = (app.view.mode, app.view.zen);
                    app.art_repaint = ArtRepaint::Wipe;
                    dirty = true;
                }
                let overlay = app.view.actions.is_some();
                if overlay != overlay_open {
                    overlay_open = overlay;
                    if !overlay { restore_art = true; }
                    dirty = true;
                }
                if should_draw(dirty, animating, last_draw.elapsed()) {
                    app.theme.advance();
                    if !in_tmux { let _ = execute!(io::stdout(), BeginSynchronizedUpdate); }
                    let repaint = app.art_repaint;
                    let drawn = terminal.draw(|f| render(f, &app, &mut out, repaint)).map(|_| ());
                    let restore_result = if drawn.is_ok() && restore_art && !overlay_open && app.view.mode == RightView::NowPlaying {
                        out.art.zip(app.playback.now.as_ref().and_then(|now| now.cover.as_ref()).or(app.session.transition_cover.as_ref()))
                            .map(|(area, cover)| cover.render_direct(terminal.backend_mut(), area))
                    } else { None };
                    if !in_tmux { let _ = execute!(io::stdout(), EndSynchronizedUpdate); }
                    drawn?;
                    app.art_repaint = app.art_repaint.advance();
                    if restore_art && !overlay_open {
                        restore_art = false;
                        if let Some(Err(error)) = restore_result {
                            liblog(format!("cover restore failed: {error}"));
                            app.art_repaint = ArtRepaint::Wipe;
                        }
                    }
                    last_draw = Instant::now(); dirty = false;
                }
                if last_sync.elapsed() >= SYNC_EVERY {
                    last_sync = Instant::now();
                    if app.view.mode == RightView::Queue {
                        let _ = app.svc.engine.request_queue_page(Some(app.transport.queue_revision), app.transport.queue_offset);
                    }
                }
                false
            }
            ev = ev_rx.recv_async() => {
                let Ok(ev) = ev else { break };
                handle_engine_event(&mut app, ev, &chans.meta);
                true
            }
            ev = in_rx.recv_async() => {
                let event = match ev {
                    Ok(Ok(event)) => event,
                    Ok(Err(error)) => return Err(error).context("read terminal input"),
                    Err(_) => break,
                };
                match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if handle_key(&mut app, key.code, key.modifiers, &chans) { break; }
                    }
                    Event::Mouse(mouse) => {
                        if handle_mouse(&mut app, &out, mouse, &chans) { break; }
                    }
                    Event::Resize(..) => app.art_repaint = ArtRepaint::Wipe,
                    Event::FocusGained if in_tmux => restore_art = true,
                    _ => {},
                }
                true
            }
            m = meta_rx.recv_async() => {
                if let Ok(reply) = m {
                    if app.session.artwork_request.finish(reply.id) {
                        if let Ok(meta) = reply.result { apply_meta(&mut app, meta); }
                    }
                }
                true
            }
            s = search_rx.recv_async() => {
                if let Ok(reply) = s {
                    if app.session.search_request.finish(reply.id) && app.search.searching {
                        app.search.in_flight = false;
                        match reply.result {
                            Ok(results) => {
                                app.session.search_error = None;
                                app.session.search_notice = results.notice.clone();
                                app.search.search_results = results.rows;
                                app.browse.selected = app.first_selectable();
                                app.status = results.notice.unwrap_or_else(|| if app.search.search_results.is_empty() { "no results".into() } else { String::new() });
                            }
                            Err(error) => { app.status = format!("search: {error}; previous results retained"); app.session.search_error = Some(error); }
                        }
                    }
                }
                true
            }
            ly = lyrics_rx.recv_async() => {
                if let Ok(reply) = ly {
                    if app.session.lyrics_request.finish(reply.id) {
                        match reply.result {
                            Ok(result) if meta_is_current(app.session.pending_meta.as_deref(), &result.uri) => {
                                app.view.lyrics = result.lines; app.view.lyrics_synced = result.synced;
                                app.session.lyrics_error = None;
                            }
                            Err(error) => app.session.lyrics_error = Some(error),
                            _ => {},
                        }
                    }
                }
                true
            }
            d = detail_rx.recv_async() => {
                if let Ok(reply) = d {
                    if app.session.detail_request.finish(reply.id) {
                        match reply.result {
                            Ok(detail) => {
                                if let Some(open) = app.browse.details.last_mut().filter(|open| open.context_uri == detail.uri) {
                                    open.items = detail.items; open.title = detail.title;
                                } else {
                                    app.browse.details.push(Detail { context_uri: detail.uri, title: detail.title, items: detail.items, parent_selected: app.browse.selected });
                                }
                                app.browse.selected = app.first_selectable(); app.status = detail.notice.unwrap_or_default();
                            }
                            Err(error) => app.status = format!("detail: {error}; previous view retained"),
                        }
                    }
                }
                true
            }
            menu = menu_rx.recv_async() => {
                if let Ok(reply) = menu {
                    if app.session.menu_request.finish(reply.id) && app.view.actions.is_some() {
                        match reply.result {
                            Ok(mut menu) => {
                                let open = app.view.actions.as_ref().expect("open menu");
                                menu.selected = open.items.get(open.selected)
                                    .and_then(|selected| menu.items.iter().position(|item| item.kind == selected.kind)).unwrap_or(0);
                                app.session.menu_error = None;
                                if menu.items.is_empty() { app.status = "no playlists available".into(); }
                                app.view.actions = Some(menu);
                            }
                            Err(error) => {
                                app.status = format!("menu: {error}; membership unknown");
                                app.session.menu_error = Some(error);
                                if let Some(open) = app.view.actions.as_mut().filter(|open| open.items.is_empty()) { open.title = "Playlist list unavailable".into(); }
                            }
                        }
                    }
                }
                true
            }
            st = astatus_rx.recv_async() => {
                if let Ok(reply) = st {
                    if app.session.action_request.finish(reply.id) {
                        let refresh = reply.result.as_ref().map(|outcome| outcome.refresh_library).unwrap_or_else(|error| *error == ApiError::AmbiguousWrite);
                        if refresh {
                            app.browse.library.invalidate();
                            request_library_section(&mut app, &chans, false);
                            if let Some(item) = app.session.menu_item.clone().filter(|_| app.view.actions.is_some()) {
                                // A menu opened while the write was in flight may
                                // contain pre-write membership. Remove stale toggles
                                // until a new contains read settles that menu.
                                app.view.actions = build_action_menu(None, &item).ok();
                                app.session.menu_error = None;
                                let id = app.session.menu_request.start();
                                spawn_action_menu(app.svc.webapi.clone(), item, id, chans.menu.clone());
                            }
                            if let Some(detail) = app.browse.details.last() {
                                let (uri, name) = (detail.context_uri.clone(), detail.title.clone());
                                let id = app.session.detail_request.start();
                                spawn_detail_fetch(app.svc.webapi.clone(), uri, name, id, chans.detail.clone());
                            }
                        }
                        app.status = match reply.result {
                            Ok(outcome) => outcome.message.into(),
                            Err(error) => {
                                if error == ApiError::AmbiguousWrite { app.session.mutation_uncertain = true; }
                                format!("library action: {error}; refresh before trying again")
                            }
                        };
                    }
                }
                true
            }
        };
        dirty |= touched;
    }
    app.svc.engine.set_spectrum_enabled(false);
    Ok(())
}

#[cfg(test)]
#[path = "main_tests/mod.rs"]
mod main_tests;
