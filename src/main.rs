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
        println!("myx [OPTIONS] [SPOTIFY_URI_OR_URL]\n  daemon run|start|stop|restart|status\n  service install|uninstall|status\n  --config PATH          Select configuration\n  --non-interactive      Never open browser authorization\n  --headless --duration SECONDS  Observe playback without a terminal\n  --volume 0..100        Set playback volume\n  --json                 Machine-readable management status\n  --help, --version");
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
    myx::config::get().validate()?;
    install_logger();
    let _ = rustls::crypto::ring::default_provider().install_default();
    if myx::client::handle_management(&cli)? {
        return Ok(());
    }
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
    let mut app = App {
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
            pending_meta: snapshot.track.as_ref().map(|track| track.uri.clone()),
            last_ctrl_c: None,
            last_click: None,
        },
        art_repaint: ArtRepaint::Idle,
    };
    // Only explicit CLI intent writes controls. Attach never restores client state.
    if let Some(volume) = startup.volume {
        app.svc.engine.set_volume(vol_u16(volume))?;
        app.transport.volume = volume;
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
    meta: flume::Sender<TrackMeta>,
    lib: flume::Sender<(Section, Vec<LibItem>)>,
    search: flume::Sender<Vec<LibItem>>,
    lyrics: flume::Sender<(Vec<(u32, String)>, bool)>,
    detail: flume::Sender<(String, String, Vec<LibItem>)>,
    menu: flume::Sender<ActionMenu>,
    astatus: flume::Sender<String>,
    libdone: flume::Sender<bool>,
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
    let (meta_tx, meta_rx) = flume::unbounded::<TrackMeta>();
    let (lib_tx, lib_rx) = flume::unbounded::<(Section, Vec<LibItem>)>();
    let (search_tx, search_rx) = flume::unbounded::<Vec<LibItem>>();
    let (lyrics_tx, lyrics_rx) = flume::unbounded::<(Vec<(u32, String)>, bool)>();
    let (detail_tx, detail_rx) = flume::unbounded::<(String, String, Vec<LibItem>)>();
    let (menu_tx, menu_rx) = flume::unbounded::<ActionMenu>();
    let (astatus_tx, astatus_rx) = flume::unbounded::<String>();
    let (libdone_tx, libdone_rx) = flume::unbounded::<bool>();
    let chans = UiChannels {
        meta: meta_tx,
        lib: lib_tx,
        search: search_tx,
        lyrics: lyrics_tx,
        detail: detail_tx,
        menu: menu_tx,
        astatus: astatus_tx,
        libdone: libdone_tx,
    };
    spawn_library_fetch(
        app.svc.webapi.clone(),
        chans.lib.clone(),
        chans.libdone.clone(),
    );
    if let Some(track) = initial_track {
        let tx = chans.meta.clone();
        let webapi = app.svc.webapi.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(fetch_sdk_artwork(
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
            ));
        });
    }
    let in_tmux = std::env::var_os("TMUX").is_some();
    let mut lib_attempts = 0;
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
                app.playback.flush_seek(&app.svc.engine, Instant::now());
                let spectrum_visible = app.view.mode == RightView::NowPlaying
                    && app.view.actions.is_none() && app.view.equalizer.is_none()
                    && out.spectrum.is_some();
                if spectrum_visible != spectrum_enabled {
                    app.svc.engine.set_spectrum_enabled(spectrum_visible);
                    spectrum_enabled = spectrum_visible;
                }
                while let Ok((section, mut items)) = lib_rx.try_recv() {
                    dirty = true;
                    for (i, item) in items.iter_mut().enumerate() { item.order = i as u32; }
                    app.browse.library.set(section, items);
                    sort_list(app.browse.library.items_mut(section), app.browse.sort);
                    if section == app.browse.section { app.normalize_selection(); }
                    app.status = format!("loaded {}", section.label());
                }
                while let Ok(got_any) = libdone_rx.try_recv() {
                    dirty = true;
                    if got_any { lib_attempts = 0; app.status.clear(); }
                    else if lib_attempts < 2 {
                        lib_attempts += 1;
                        app.status = "retrying library…".into();
                        spawn_library_fetch(app.svc.webapi.clone(), chans.lib.clone(), chans.libdone.clone());
                    } else {
                        app.status = "library failed — press r to reload".into();
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
                        out.art.zip(app.playback.now.as_ref().and_then(|now| now.cover.as_ref()))
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
                    if app.transport.playback_started { let _ = app.svc.engine.request_queue(); }
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
            m = meta_rx.recv_async() => { if let Ok(meta) = m { apply_meta(&mut app, meta, &chans.lyrics); } true }
            s = search_rx.recv_async() => {
                if let Ok(results) = s {
                    app.search.in_flight = false; app.search.search_results = results;
                    app.browse.selected = app.first_selectable();
                    app.status = if app.search.search_results.is_empty() { "no results".into() } else { String::new() };
                }
                true
            }
            ly = lyrics_rx.recv_async() => { if let Ok((lines, synced)) = ly { app.view.lyrics = lines; app.view.lyrics_synced = synced; } true }
            d = detail_rx.recv_async() => {
                if let Ok((context_uri, title, items)) = d {
                    app.browse.details.push(Detail { context_uri, title, items, parent_selected: app.browse.selected });
                    app.browse.selected = app.first_selectable(); app.status.clear();
                }
                true
            }
            menu = menu_rx.recv_async() => {
                if let Ok(mut menu) = menu {
                    if let Some(open) = app.view.actions.as_ref().filter(|_| !menu.items.is_empty()) {
                        menu.selected = open.selected.min(menu.items.len() - 1); app.view.actions = Some(menu);
                    }
                }
                true
            }
            st = astatus_rx.recv_async() => { if let Ok(msg) = st { app.status = msg; } true }
        };
        dirty |= touched;
    }
    app.svc.engine.set_spectrum_enabled(false);
    Ok(())
}

#[cfg(test)]
#[path = "main_tests/mod.rs"]
mod main_tests;
