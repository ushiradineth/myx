//! Nonblocking libpulse transport, confined to one owned worker thread.
//! ABI declarations follow the public PulseAudio 17.0 headers (context.h,
//! stream.h, mainloop.h, sample.h, def.h). No PulseAudio implementation is copied.
use crate::pcm::{AudioControl, AudioProcessor, BLOCK_SAMPLES, CHANNELS, SAMPLE_RATE};
use anyhow::{ensure, Context, Result};
use libloading::Library;
use std::{
    collections::VecDeque,
    ffi::{c_char, c_int, c_void, CString},
    path::Path,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Handle = *mut c_void;
type Success = Option<unsafe extern "C" fn(Handle, c_int, Handle)>;
const FRAME_BYTES: usize = CHANNELS * 4;
const BLOCK_BYTES: usize = BLOCK_SAMPLES * 4;
const MAX_PENDING: usize = BLOCK_BYTES * 4;
const FLOAT32LE: c_int = 5;
const CONTEXT_READY: c_int = 4;
const STREAM_READY: c_int = 2;
const START_CORKED: c_int = 1;
const AUTO_TIMING: c_int = 8;
const DONT_MOVE: c_int = 0x200;
const ADJUST_LATENCY: c_int = 0x2000;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}
#[derive(Debug)]
#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

macro_rules! api {
    ($($name:ident : $ty:ty),+ $(,)?) => {
        struct Api { $($name: $ty,)+ _library: Library }
        impl Api {
            fn load(path: &Path) -> Result<Self> {
                // SAFETY: the caller supplies the provisioned native libpulse.
                // Symbols use the pinned public C ABI and remain loaded until
                // every context/stream/mainloop has been destroyed.
                unsafe {
                    // libpulse/common registers C thread-local destructors. Its
                    // code must outlive worker-thread exit, not just Context.
                    // NODELETE pins code without leaking a dlopen reference on
                    // every runtime restart; handles are still closed normally.
                    let library: Library = libloading::os::unix::Library::open(Some(path), libc::RTLD_NOW | libc::RTLD_LOCAL | libc::RTLD_NODELETE)
                        .context("native libpulse unavailable")?.into();
                    $(let $name = *library.get::<$ty>(concat!(stringify!($name), "\0").as_bytes())
                        .context("native libpulse API unavailable")?;)+
                    Ok(Self { $($name,)+ _library: library })
                }
            }
        }
    }
}
api! {
    pa_mainloop_new: unsafe extern "C" fn() -> Handle,
    pa_mainloop_free: unsafe extern "C" fn(Handle),
    pa_mainloop_get_api: unsafe extern "C" fn(Handle) -> Handle,
    pa_mainloop_prepare: unsafe extern "C" fn(Handle, c_int) -> c_int,
    pa_mainloop_poll: unsafe extern "C" fn(Handle) -> c_int,
    pa_mainloop_dispatch: unsafe extern "C" fn(Handle) -> c_int,
    pa_context_new: unsafe extern "C" fn(Handle, *const c_char) -> Handle,
    pa_context_unref: unsafe extern "C" fn(Handle),
    pa_context_disconnect: unsafe extern "C" fn(Handle),
    pa_context_connect: unsafe extern "C" fn(Handle, *const c_char, c_int, Handle) -> c_int,
    pa_context_load_cookie_from_file: unsafe extern "C" fn(Handle, *const c_char) -> c_int,
    pa_context_get_state: unsafe extern "C" fn(Handle) -> c_int,
    pa_stream_new: unsafe extern "C" fn(Handle, *const c_char, *const SampleSpec, Handle) -> Handle,
    pa_stream_unref: unsafe extern "C" fn(Handle),
    pa_stream_disconnect: unsafe extern "C" fn(Handle) -> c_int,
    pa_stream_get_state: unsafe extern "C" fn(Handle) -> c_int,
    pa_stream_get_sample_spec: unsafe extern "C" fn(Handle) -> *const SampleSpec,
    pa_stream_get_buffer_attr: unsafe extern "C" fn(Handle) -> *const BufferAttr,
    pa_stream_connect_playback: unsafe extern "C" fn(Handle, *const c_char, *const BufferAttr, c_int, Handle, Handle) -> c_int,
    pa_stream_connect_record: unsafe extern "C" fn(Handle, *const c_char, *const BufferAttr, c_int) -> c_int,
    pa_stream_readable_size: unsafe extern "C" fn(Handle) -> usize,
    pa_stream_writable_size: unsafe extern "C" fn(Handle) -> usize,
    pa_stream_peek: unsafe extern "C" fn(Handle, *mut *const c_void, *mut usize) -> c_int,
    pa_stream_drop: unsafe extern "C" fn(Handle) -> c_int,
    pa_stream_write: unsafe extern "C" fn(Handle, *const c_void, usize, Option<unsafe extern "C" fn(Handle)>, i64, c_int) -> c_int,
    pa_stream_cork: unsafe extern "C" fn(Handle, c_int, Success, Handle) -> Handle,
    pa_stream_flush: unsafe extern "C" fn(Handle, Success, Handle) -> Handle,
    pa_stream_get_latency: unsafe extern "C" fn(Handle, *mut u64, *mut c_int) -> c_int,
    pa_operation_unref: unsafe extern "C" fn(Handle),
    pa_operation_get_state: unsafe extern "C" fn(Handle) -> c_int,
}

struct Connection {
    api: Api,
    mainloop: Handle,
    context: Handle,
    capture: Handle,
    playback: Handle,
}
impl Connection {
    fn open(
        library: &Path,
        server: &str,
        cookie: &Path,
        sink: &str,
        source: &str,
        stop: &AtomicBool,
        deadline: Instant,
    ) -> Result<Self> {
        let mut connection = Self {
            api: Api::load(library)?,
            mainloop: ptr::null_mut(),
            context: ptr::null_mut(),
            capture: ptr::null_mut(),
            playback: ptr::null_mut(),
        };
        let server = CString::new(server)?;
        let cookie = CString::new(cookie.to_str().context("non-UTF8 cookie path")?)?;
        let sink = CString::new(sink)?;
        let source = CString::new(source)?;
        let spec = SampleSpec {
            format: FLOAT32LE,
            rate: SAMPLE_RATE,
            channels: CHANNELS as u8,
        };
        let attr = BufferAttr {
            maxlength: MAX_PENDING as u32,
            tlength: BLOCK_BYTES as u32 * 2,
            // Do not free-run the output clock before captured PCM arrives.
            // Otherwise post-pause/underrun writes can land behind the read
            // index and be discarded forever. One block bounds startup delay.
            prebuf: BLOCK_BYTES as u32,
            minreq: BLOCK_BYTES as u32,
            fragsize: BLOCK_BYTES as u32,
        };
        // SAFETY: all handles are null-checked, owned only on this thread, and
        // released by Connection::drop; C strings and attributes outlive calls.
        unsafe {
            connection.mainloop = (connection.api.pa_mainloop_new)();
            ensure!(
                !connection.mainloop.is_null(),
                "PCM mainloop creation failed"
            );
            connection.context = (connection.api.pa_context_new)(
                (connection.api.pa_mainloop_get_api)(connection.mainloop),
                c"Myx PCM bridge".as_ptr(),
            );
            ensure!(!connection.context.is_null(), "PCM context creation failed");
            ensure!(
                (connection.api.pa_context_load_cookie_from_file)(
                    connection.context,
                    cookie.as_ptr()
                ) == 0,
                "PCM private authentication unavailable"
            );
            // NOAUTOSPAWN; never connect to a user's global/default audio server.
            ensure!(
                (connection.api.pa_context_connect)(
                    connection.context,
                    server.as_ptr(),
                    1,
                    ptr::null_mut()
                ) == 0,
                "PCM private connection failed"
            );
            while (connection.api.pa_context_get_state)(connection.context) != CONTEXT_READY {
                connection.iterate()?;
                ensure!(
                    !stop.load(Ordering::Acquire) && Instant::now() < deadline,
                    "PCM startup cancelled or timed out"
                );
                thread::sleep(Duration::from_millis(2));
            }
            connection.capture = (connection.api.pa_stream_new)(
                connection.context,
                c"Myx input monitor".as_ptr(),
                &spec,
                ptr::null_mut(),
            );
            connection.playback = (connection.api.pa_stream_new)(
                connection.context,
                c"Myx processed output".as_ptr(),
                &spec,
                ptr::null_mut(),
            );
            ensure!(
                !connection.capture.is_null() && !connection.playback.is_null(),
                "PCM stream creation failed"
            );
            let flags = START_CORKED | AUTO_TIMING | ADJUST_LATENCY | DONT_MOVE;
            ensure!(
                (connection.api.pa_stream_connect_record)(
                    connection.capture,
                    source.as_ptr(),
                    &attr,
                    flags
                ) == 0,
                "PCM monitor connection failed"
            );
            ensure!(
                (connection.api.pa_stream_connect_playback)(
                    connection.playback,
                    sink.as_ptr(),
                    &attr,
                    flags,
                    ptr::null_mut(),
                    ptr::null_mut()
                ) == 0,
                "PCM output connection failed"
            );
            while (connection.api.pa_stream_get_state)(connection.capture) != STREAM_READY
                || (connection.api.pa_stream_get_state)(connection.playback) != STREAM_READY
            {
                connection.iterate()?;
                ensure!(
                    !stop.load(Ordering::Acquire) && Instant::now() < deadline,
                    "PCM stream startup cancelled or timed out"
                );
                thread::sleep(Duration::from_millis(2));
            }
            for stream in [connection.capture, connection.playback] {
                let actual = (connection.api.pa_stream_get_sample_spec)(stream);
                ensure!(
                    !actual.is_null()
                        && (*actual).format == FLOAT32LE
                        && (*actual).rate == SAMPLE_RATE
                        && (*actual).channels == CHANNELS as u8,
                    "PCM negotiated an unsupported format"
                );
                let buffer = (connection.api.pa_stream_get_buffer_attr)(stream);
                ensure!(
                    !buffer.is_null() && (*buffer).maxlength as usize <= MAX_PENDING,
                    "PCM negotiated an unbounded buffer"
                );
            }
        }
        Ok(connection)
    }
    fn iterate(&self) -> Result<()> {
        // SAFETY: handles belong to this single-threaded connection.
        unsafe {
            // libpulse defers packet reads/writes across dispatch iterations.
            // Drain ready work before parking; one dispatch per 2ms throttles
            // short monitor fragments below real-time throughput.
            for _ in 0..64 {
                ensure!(
                    (self.api.pa_mainloop_prepare)(self.mainloop, 0) >= 0,
                    "PCM mainloop preparation failed"
                );
                ensure!(
                    (self.api.pa_mainloop_poll)(self.mainloop) >= 0,
                    "PCM mainloop poll failed"
                );
                let dispatched = (self.api.pa_mainloop_dispatch)(self.mainloop);
                ensure!(dispatched >= 0, "PCM mainloop dispatch failed");
                if dispatched == 0 {
                    break;
                }
            }
            ensure!(
                (self.api.pa_context_get_state)(self.context) < 5,
                "PCM server disconnected"
            );
            for stream in [self.capture, self.playback] {
                if !stream.is_null() {
                    ensure!(
                        (self.api.pa_stream_get_state)(stream) < 3,
                        "PCM stream failed"
                    );
                }
            }
        }
        Ok(())
    }
    fn operation(&self, operation: Handle) -> Result<()> {
        ensure!(!operation.is_null(), "PCM stream operation failed");
        // A flush changes libpulse's write-index/request accounting. Do not
        // resume writes until its acknowledgement has been dispatched.
        // SAFETY: keep our operation reference through completion/timeout and
        // unref on every result path; no callback borrows Rust stack data.
        let result = (|| -> Result<()> {
            let deadline = Instant::now() + Duration::from_millis(500);
            while unsafe { (self.api.pa_operation_get_state)(operation) } == 0 {
                ensure!(Instant::now() < deadline, "PCM stream operation timed out");
                self.iterate()?;
            }
            ensure!(
                unsafe { (self.api.pa_operation_get_state)(operation) } == 1,
                "PCM stream operation cancelled"
            );
            Ok(())
        })();
        unsafe {
            (self.api.pa_operation_unref)(operation);
        }
        result
    }
    fn reset(&self, active: bool) -> Result<()> {
        // SAFETY: both streams are ready; no callbacks borrow Rust data.
        unsafe {
            for stream in [self.capture, self.playback] {
                // Keep the private monitor clock running while paused. Only
                // the physical playback stream is corked; inactive PCM is
                // discarded without EQ/FFT or output, so no stale audio queues.
                self.operation((self.api.pa_stream_cork)(
                    stream,
                    i32::from(stream == self.playback),
                    None,
                    ptr::null_mut(),
                ))?;
                self.operation((self.api.pa_stream_flush)(stream, None, ptr::null_mut()))?;
                if active && stream == self.playback {
                    self.operation((self.api.pa_stream_cork)(stream, 0, None, ptr::null_mut()))?;
                }
            }
        }
        Ok(())
    }
    fn transfer(
        &self,
        pending: &mut VecDeque<u8>,
        processor: &mut dyn AudioProcessor,
        control: &AudioControl,
    ) -> Result<usize> {
        let mut received = 0;
        let mut samples = [0.0f32; BLOCK_SAMPLES];
        let mut output = [0u8; MAX_PENDING];
        // SAFETY: peeked buffers remain valid until drop. Lengths are bounded,
        // frame-aligned, and copied before dropping; write requests an internal
        // copy (no borrowed Rust buffer is retained by libpulse).
        unsafe {
            if (self.api.pa_stream_readable_size)(self.capture) == usize::MAX {
                anyhow::bail!("PCM input failed");
            }
            loop {
                let mut data = ptr::null();
                let mut length = 0usize;
                ensure!(
                    (self.api.pa_stream_peek)(self.capture, &mut data, &mut length) == 0,
                    "PCM read failed"
                );
                if length == 0 {
                    break;
                }
                ensure!(length.is_multiple_of(FRAME_BYTES), "unaligned PCM fragment");
                if !control.active() {
                    received += length / FRAME_BYTES;
                    ensure!(
                        (self.api.pa_stream_drop)(self.capture) == 0,
                        "inactive PCM release failed"
                    );
                    continue;
                }
                ensure!(
                    length <= MAX_PENDING,
                    "PCM input exceeds bounded frame capacity"
                );
                if pending.len() + length > MAX_PENDING {
                    break;
                }
                let bytes = if data.is_null() {
                    None
                } else {
                    Some(std::slice::from_raw_parts(data.cast::<u8>(), length))
                };
                for offset in (0..length).step_by(BLOCK_BYTES) {
                    let count = (length - offset).min(BLOCK_BYTES) / 4;
                    for (i, sample) in samples[..count].iter_mut().enumerate() {
                        *sample = bytes.map_or(0.0, |bytes| {
                            f32::from_le_bytes(
                                bytes[offset + i * 4..offset + i * 4 + 4]
                                    .try_into()
                                    .expect("four PCM bytes"),
                            )
                        });
                        ensure!(sample.is_finite(), "PCM input contains non-finite samples");
                    }
                    processor.process(&mut samples[..count]);
                    for sample in &samples[..count] {
                        ensure!(
                            sample.is_finite(),
                            "PCM processor produced non-finite samples"
                        );
                        pending.extend(sample.to_le_bytes());
                    }
                    control.record(count / CHANNELS, pending.len() / FRAME_BYTES, None);
                }
                received += length / FRAME_BYTES;
                ensure!(
                    (self.api.pa_stream_drop)(self.capture) == 0,
                    "PCM fragment release failed"
                );
            }
            let writable = (self.api.pa_stream_writable_size)(self.playback);
            ensure!(writable != usize::MAX, "PCM output failed");
            let count = writable.min(pending.len()).min(MAX_PENDING) / FRAME_BYTES * FRAME_BYTES;
            if count > 0 {
                for byte in &mut output[..count] {
                    *byte = pending.pop_front().expect("bounded pending PCM");
                }
                ensure!(
                    (self.api.pa_stream_write)(
                        self.playback,
                        output.as_ptr().cast(),
                        count,
                        None,
                        0,
                        0
                    ) == 0,
                    "PCM output write failed"
                );
            }
            let mut latency = 0;
            for stream in [self.capture, self.playback] {
                let mut micros = 0;
                let mut negative = 0;
                if (self.api.pa_stream_get_latency)(stream, &mut micros, &mut negative) == 0
                    && negative == 0
                {
                    latency += micros;
                }
            }
            control.record(
                0,
                pending.len() / FRAME_BYTES,
                Some(
                    latency
                        + pending.len() as u64 * 1_000_000
                            / FRAME_BYTES as u64
                            / SAMPLE_RATE as u64,
                ),
            );
        }
        Ok(received)
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: all handles were created on this thread. Partial startup is
        // supported; streams are released before their context and mainloop.
        unsafe {
            for stream in [self.capture, self.playback] {
                if !stream.is_null() {
                    (self.api.pa_stream_disconnect)(stream);
                    (self.api.pa_stream_unref)(stream);
                }
            }
            if !self.context.is_null() {
                (self.api.pa_context_disconnect)(self.context);
                (self.api.pa_context_unref)(self.context);
            }
            if !self.mainloop.is_null() {
                (self.api.pa_mainloop_free)(self.mainloop);
            }
        }
    }
}

pub(crate) struct Bridge {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
    control: AudioControl,
}
pub(crate) struct Endpoint<'a> {
    pub tools: &'a Path,
    pub server: &'a str,
    pub cookie: &'a Path,
    pub sink: &'a str,
    pub source: &'a str,
}
impl Bridge {
    pub(crate) fn start(
        endpoint: Endpoint<'_>,
        control: AudioControl,
        mut processor: Box<dyn AudioProcessor>,
        deadline: Instant,
    ) -> Result<Self> {
        let Endpoint {
            tools,
            server,
            cookie,
            sink,
            source,
        } = endpoint;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let state = control.clone();
        let library = tools.join("lib/libpulse.0.dylib");
        let server = server.to_owned();
        let cookie = cookie.to_owned();
        let sink = sink.to_owned();
        let source = source.to_owned();
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("myx-pcm".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let connection = Connection::open(
                        &library, &server, &cookie, &sink, &source, &stopped, deadline,
                    )?;
                    let mut pending = VecDeque::with_capacity(MAX_PENDING);
                    connection.reset(false)?;
                    // A new null sink may initially have a long idle quantum.
                    // Prime/drain its monitor before publishing runtime ready,
                    // keeping that setup delay outside active playback.
                    let mut primed = 0;
                    while primed < crate::pcm::BLOCK_FRAMES {
                        ensure!(
                            !stopped.load(Ordering::Acquire) && Instant::now() < deadline,
                            "PCM monitor priming cancelled or timed out"
                        );
                        connection.iterate()?;
                        primed += connection.transfer(&mut pending, processor.as_mut(), &state)?;
                        thread::park_timeout(Duration::from_millis(2));
                    }
                    processor.reset();
                    let mut generation = u64::MAX;
                    state.health(1);
                    let _ = tx.send(true);
                    while !stopped.load(Ordering::Acquire) {
                        connection.iterate()?;
                        if state.generation() != generation {
                            generation = state.generation();
                            pending.clear();
                            processor.reset();
                            connection.reset(state.active())?;
                        }
                        connection.transfer(&mut pending, processor.as_mut(), &state)?;
                        thread::park_timeout(Duration::from_millis(2));
                    }
                    Ok(())
                })();
                processor.reset();
                state.health(if result.is_ok() { 3 } else { 2 });
                let _ = tx.try_send(false);
                result
            })?;
        let mut bridge = Self {
            stop,
            worker: Some(worker),
            control,
        };
        if !matches!(
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
            Ok(true)
        ) {
            let _ = bridge.stop();
            anyhow::bail!("native PCM bridge startup failed");
        }
        Ok(bridge)
    }
    pub(crate) fn running(&self) -> bool {
        self.control.is_ready()
            && self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
    }
    pub(crate) fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            ensure!(
                worker.is_finished(),
                "PCM worker did not stop within deadline"
            );
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("PCM worker panicked"))??;
        }
        Ok(())
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{private, process::OwnedChild};
    use std::process::{Command, Stdio};

    struct HalfGain(Arc<AtomicBool>);
    impl AudioProcessor for HalfGain {
        fn process(&mut self, samples: &mut [f32]) {
            if samples.iter().any(|sample| sample.abs() > 0.01) {
                self.0.store(true, Ordering::Release);
            }
            for sample in samples {
                *sample *= 0.5;
            }
        }
        fn reset(&mut self) {}
    }

    #[test]
    #[ignore = "requires MYX_AUDIO_TOOLS pointing to provisioned native PulseAudio; private null sinks only"]
    fn native_pcm_routes_processed_stereo_and_stops_with_bounded_latency() {
        let tools = std::path::PathBuf::from(
            std::env::var_os("MYX_AUDIO_TOOLS").expect("MYX_AUDIO_TOOLS required"),
        );
        let temp = tempfile::tempdir().unwrap();
        let cookie = temp.path().join("cookie");
        let config = temp.path().join("server.pa");
        private::write_private(&cookie, &[42; 256]).unwrap();
        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        private::write_private(&config, format!(".fail\nload-module module-null-sink sink_name=myx_input format=float32le rate=44100 channels=2 norewinds=true\nload-module module-null-sink sink_name=test_output format=float32le rate=44100 channels=2 norewinds=true\nload-module module-native-protocol-tcp listen=127.0.0.1 port={port} auth-anonymous=no auth-cookie={}\nload-module module-native-protocol-unix socket={} auth-anonymous=no auth-cookie={}\nset-default-sink myx_input\n", serde_json::to_string(cookie.to_str().unwrap()).unwrap(), serde_json::to_string(temp.path().join("native").to_str().unwrap()).unwrap(), serde_json::to_string(cookie.to_str().unwrap()).unwrap()).as_bytes()).unwrap();
        drop(reservation);
        let mut command = Command::new(tools.join("bin/pulseaudio"));
        command
            .args([
                "--daemonize=no",
                "--use-pid-file=no",
                "--system=no",
                "--exit-idle-time=-1",
                "--disable-shm=yes",
                "--log-target=stderr",
                "--log-level=error",
                "-n",
                "-F",
            ])
            .arg(&config)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", temp.path())
            .env("PULSE_RUNTIME_PATH", temp.path())
            .env("PULSE_STATE_PATH", temp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut server = OwnedChild::spawn(&mut command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline && server.child.try_wait().unwrap().is_none());
            thread::sleep(Duration::from_millis(5));
        }
        let address = format!("unix:{}", temp.path().join("native").display());
        let control = AudioControl::default();
        let saw_input = Arc::new(AtomicBool::new(false));
        let mut bridge = Bridge::start(
            Endpoint {
                tools: &tools,
                server: &address,
                cookie: &cookie,
                sink: "test_output",
                source: "myx_input.monitor",
            },
            control.clone(),
            Box::new(HalfGain(saw_input.clone())),
            deadline,
        )
        .unwrap();
        let stop = AtomicBool::new(false);
        let peer = Connection::open(
            &tools.join("lib/libpulse.0.dylib"),
            &address,
            &cookie,
            "myx_input",
            "test_output.monitor",
            &stop,
            deadline,
        )
        .unwrap();
        peer.reset(true).unwrap();
        control.set_active(true);
        // Allow cork/flush operations to reach both contexts before timing.
        for _ in 0..20 {
            peer.iterate().unwrap();
            thread::sleep(Duration::from_millis(2));
        }
        let mut bytes = [0u8; BLOCK_BYTES];
        for frame in bytes.chunks_exact_mut(FRAME_BYTES) {
            frame[..4].copy_from_slice(&0.25f32.to_le_bytes());
        }
        let mut first_write = None;
        let mut first_processed = None;
        let mut written_frames = 0;
        let mut iterations = 0;
        let mut captured_frames = 0;
        let deadline = Instant::now() + Duration::from_secs(3);
        while first_processed.is_none() && Instant::now() < deadline {
            iterations += 1;
            if !bridge.running() {
                bridge.stop().unwrap();
                panic!("PCM worker exited before processing");
            }
            peer.iterate().unwrap();
            // SAFETY: ready private test streams; every peeked buffer is copied
            // before drop, and write requests an immediate libpulse-owned copy.
            unsafe {
                let writable = (peer.api.pa_stream_writable_size)(peer.playback);
                if writable >= BLOCK_BYTES {
                    written_frames += BLOCK_BYTES / FRAME_BYTES;
                    // Prime the synthetic output's own null sink with silence.
                    // Measure a signal transition only once the route is live;
                    // cold daemon setup is a separate bounded startup phase.
                    let silence = [0u8; BLOCK_BYTES];
                    let input = if captured_frames >= crate::pcm::BLOCK_FRAMES * 4 {
                        first_write.get_or_insert_with(Instant::now);
                        &bytes
                    } else {
                        &silence
                    };
                    assert_eq!(
                        (peer.api.pa_stream_write)(
                            peer.playback,
                            input.as_ptr().cast(),
                            input.len(),
                            None,
                            0,
                            0
                        ),
                        0
                    );
                }
                for _ in 0..64 {
                    let mut data = ptr::null();
                    let mut length = 0usize;
                    assert_eq!(
                        (peer.api.pa_stream_peek)(peer.capture, &mut data, &mut length),
                        0
                    );
                    if length == 0 {
                        break;
                    }
                    if length > 0 {
                        captured_frames += length / FRAME_BYTES;
                        if !data.is_null() {
                            let samples = std::slice::from_raw_parts(data.cast::<u8>(), length);
                            for frame in samples.chunks_exact(FRAME_BYTES) {
                                let left = f32::from_le_bytes(frame[..4].try_into().unwrap());
                                let right = f32::from_le_bytes(frame[4..].try_into().unwrap());
                                assert!(right.abs() < 1e-6, "stereo channels mixed");
                                if left.abs() > 0.01 {
                                    assert!(
                                        (left - 0.125).abs() < 1e-6,
                                        "unprocessed audio reached output"
                                    );
                                    first_processed = Some(Instant::now());
                                    break;
                                }
                            }
                        }
                        assert_eq!((peer.api.pa_stream_drop)(peer.capture), 0);
                    }
                    if first_processed.is_some() {
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
        let delay = first_processed
            .unwrap_or_else(|| panic!("processed PCM never reached output: {:?}; written={written_frames}; captured={captured_frames}; iterations={iterations}; input_signal={}", control.stats(), saw_input.load(Ordering::Acquire)))
            .duration_since(first_write.unwrap());
        println!(
            "private PCM roundtrip latency: {delay:?}; {:?}",
            control.stats()
        );
        assert!(
            delay <= Duration::from_millis(50),
            "PCM bridge exceeds 50ms latency budget: {delay:?}"
        );
        control.set_active(false);
        thread::sleep(Duration::from_millis(30));
        let paused_frames = control.stats().processed_frames;
        thread::sleep(Duration::from_millis(20));
        assert_eq!(
            control.stats().processed_frames,
            paused_frames,
            "paused PCM still processed"
        );
        control.set_active(true);
        thread::sleep(Duration::from_millis(40));
        assert!(
            control.stats().processed_frames > paused_frames,
            "PCM did not resume"
        );
        let before_seek = control.stats().processed_frames;
        control.reset();
        thread::sleep(Duration::from_millis(40));
        assert!(
            control.stats().processed_frames > before_seek,
            "PCM did not recover after flushing a seek"
        );
        control.set_active(false);
        bridge.stop().unwrap();
        assert!(!control.is_ready());
        drop(peer);
        server.stop().unwrap();
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    }
}
