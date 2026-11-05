//! Nonblocking libpulse transport, confined to one owned worker thread.
//! ABI declarations follow the public PulseAudio 17.0 headers (context.h,
//! stream.h, mainloop.h, sample.h, def.h). No PulseAudio implementation is copied.
use crate::pcm::{
    AudioControl, AudioProcessor, TimingMetric, BLOCK_SAMPLES, CHANNELS, SAMPLE_RATE,
};
use anyhow::{ensure, Context, Result};
use libloading::Library;
use std::{
    collections::VecDeque,
    ffi::{c_char, c_int, c_void, CString},
    path::Path,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Handle = *mut c_void;
type Success = Option<unsafe extern "C" fn(Handle, c_int, Handle)>;
type Notify = Option<unsafe extern "C" fn(Handle, Handle)>;
#[derive(Default)]
struct StreamReply {
    success: std::cell::Cell<Option<bool>>,
}
impl StreamReply {
    fn finish(&self) -> Result<()> {
        ensure!(
            self.success.get() == Some(true),
            "PCM stream operation rejected or unacknowledged"
        );
        Ok(())
    }
}
unsafe extern "C" fn stream_reply(_stream: Handle, success: c_int, userdata: Handle) {
    if !userdata.is_null() {
        // SAFETY: operation retains the reply and cancels the callback before
        // returning on any failure; dispatch runs only on this owning thread.
        unsafe { &*userdata.cast::<StreamReply>() }
            .success
            .set(Some(success != 0));
    }
}

enum StreamOperation {
    Cork(bool),
    Flush,
}
const FRAME_BYTES: usize = CHANNELS * 4;
const BLOCK_BYTES: usize = BLOCK_SAMPLES * 4;
// Keep processed-but-unwritten PCM within the combined 50ms transport budget.
const MAX_PENDING: usize = BLOCK_BYTES;
// Capacity for transport bursts, not the requested capture latency/fragment size.
const MAX_CAPTURE: usize = SAMPLE_RATE as usize / 10 * FRAME_BYTES;
// 1,920 frames cover the scheduling-gap regression without a 100ms output queue.
const MAX_OUTPUT: usize = BLOCK_BYTES * 15;
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
    pa_mainloop_wakeup: unsafe extern "C" fn(Handle),
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
    pa_stream_set_underflow_callback: unsafe extern "C" fn(Handle, Notify, Handle),
    pa_operation_unref: unsafe extern "C" fn(Handle),
    pa_operation_cancel: unsafe extern "C" fn(Handle),
    pa_operation_get_state: unsafe extern "C" fn(Handle) -> c_int,
}

struct Connection {
    api: Api,
    mainloop: Handle,
    context: Handle,
    capture: Handle,
    playback: Handle,
    control: Option<AudioControl>,
    capture_offset: std::cell::Cell<usize>,
}

fn capture_span(length: usize, offset: usize, pending: usize) -> Result<std::ops::Range<usize>> {
    ensure!(
        length <= MAX_CAPTURE
            && offset <= length
            && pending <= MAX_PENDING
            && length.is_multiple_of(FRAME_BYTES)
            && offset.is_multiple_of(FRAME_BYTES)
            && pending.is_multiple_of(FRAME_BYTES),
        "invalid bounded PCM capture span"
    );
    Ok(offset..offset + (length - offset).min(MAX_PENDING - pending))
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
        ensure!(
            !stop.load(Ordering::Acquire) && Instant::now() < deadline,
            "PCM startup cancelled or timed out"
        );
        let mut connection = Self {
            api: Api::load(library)?,
            mainloop: ptr::null_mut(),
            context: ptr::null_mut(),
            capture: ptr::null_mut(),
            playback: ptr::null_mut(),
            control: None,
            capture_offset: std::cell::Cell::new(0),
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
            maxlength: MAX_CAPTURE as u32,
            tlength: BLOCK_BYTES as u32 * 2,
            // Do not free-run the output clock before captured PCM arrives.
            // Otherwise post-pause/underrun writes can land behind the read
            // index and be discarded forever. One block bounds startup delay.
            prebuf: BLOCK_BYTES as u32,
            minreq: BLOCK_BYTES as u32,
            fragsize: BLOCK_BYTES as u32,
        };
        let output_attr = BufferAttr {
            // Cover callback scheduling jitter without growing any queue past
            // its explicit 43.6ms output-queue bound at the negotiated rate.
            maxlength: MAX_OUTPUT as u32,
            tlength: MAX_OUTPUT as u32,
            prebuf: MAX_OUTPUT as u32,
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
                    &output_attr,
                    flags & !ADJUST_LATENCY,
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
                    !buffer.is_null()
                        && (FRAME_BYTES..=if stream == connection.capture {
                            MAX_CAPTURE
                        } else {
                            MAX_OUTPUT
                        })
                            .contains(&((*buffer).maxlength as usize))
                        && if stream == connection.capture {
                            (FRAME_BYTES..=MAX_PENDING).contains(&((*buffer).fragsize as usize))
                        } else {
                            (FRAME_BYTES..=MAX_OUTPUT).contains(&((*buffer).tlength as usize))
                        },
                    "PCM negotiated an unbounded buffer"
                );
            }
        }
        Ok(connection)
    }
    fn iterate(&self) -> Result<()> {
        self.dispatch(false)
    }
    fn dispatch(&self, stop_for_capture: bool) -> Result<()> {
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
                // Consume queued capture data before dispatching another batch:
                // a bounded record queue can overflow even while I/O is busy.
                let capture_ready = stop_for_capture
                    && !self.capture.is_null()
                    && (self.api.pa_stream_get_state)(self.capture) == STREAM_READY
                    && (self.api.pa_stream_readable_size)(self.capture) > 0;
                if dispatched == 0 || capture_ready {
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
    fn wait(&self, milliseconds: c_int) -> Result<()> {
        // pa_mainloop_prepare uses microseconds, unlike poll(2). Passing the
        // millisecond value directly makes even an idle worker busy-loop.
        let timeout_micros = milliseconds.saturating_mul(1_000);
        // SAFETY: only this worker polls the mainloop. pa_mainloop_wakeup is
        // explicitly thread-safe and interrupts the poll on control changes.
        unsafe {
            ensure!(
                (self.api.pa_mainloop_prepare)(self.mainloop, timeout_micros) >= 0,
                "PCM prepare failed"
            );
            ensure!(
                (self.api.pa_mainloop_poll)(self.mainloop) >= 0,
                "PCM poll failed"
            );
            ensure!(
                (self.api.pa_mainloop_dispatch)(self.mainloop) >= 0,
                "PCM dispatch failed"
            );
        }
        Ok(())
    }
    fn register_control(&mut self, control: AudioControl) {
        control.register_wake(Some(crate::pcm::Wake {
            handle: self.mainloop as usize,
            wake: self.api.pa_mainloop_wakeup,
        }));
        // SAFETY: the Arc allocation is stable across moves of this connection;
        // self.control retains it until Drop unregisters this same-thread callback.
        unsafe {
            (self.api.pa_stream_set_underflow_callback)(
                self.playback,
                Some(crate::pcm::playback_underflow),
                control.underflow_userdata(),
            );
        }
        self.control = Some(control);
    }
    fn operation(&self, stream: Handle, request: StreamOperation) -> Result<()> {
        let reply = StreamReply::default();
        let userdata = (&reply as *const StreamReply).cast_mut().cast();
        // SAFETY: reply stays at this stack address until completion or explicit
        // cancellation; none of these operations dispatches on another thread.
        let operation = unsafe {
            match request {
                StreamOperation::Cork(corked) => (self.api.pa_stream_cork)(
                    stream,
                    i32::from(corked),
                    Some(stream_reply),
                    userdata,
                ),
                StreamOperation::Flush => {
                    (self.api.pa_stream_flush)(stream, Some(stream_reply), userdata)
                }
            }
        };
        ensure!(!operation.is_null(), "PCM stream operation failed");
        // A flush changes libpulse's write-index/request accounting. Do not
        // resume writes until its acknowledgement has been dispatched.
        // SAFETY: keep our operation reference through completion/timeout and
        // cancel on error before unref, so no callback can outlive its reply.
        let result = (|| -> Result<()> {
            let deadline = Instant::now() + Duration::from_millis(500);
            while unsafe { (self.api.pa_operation_get_state)(operation) } == 0 {
                ensure!(Instant::now() < deadline, "PCM stream operation timed out");
                self.iterate()?;
                if unsafe { (self.api.pa_operation_get_state)(operation) } == 0 {
                    self.wait(10)?;
                }
            }
            ensure!(
                unsafe { (self.api.pa_operation_get_state)(operation) } == 1,
                "PCM stream operation cancelled"
            );
            reply.finish()
        })();
        unsafe {
            if result.is_err() {
                (self.api.pa_operation_cancel)(operation);
            }
            (self.api.pa_operation_unref)(operation);
        }
        result
    }
    fn reset(&self, active: bool) -> Result<()> {
        // SAFETY: both streams are ready; no callbacks borrow Rust data.
        unsafe {
            for stream in [self.capture, self.playback] {
                // Cork both streams while inactive: the SDK's private null sink
                // remains available, but no monitor packets drive paused work.
                self.operation(stream, StreamOperation::Cork(true))?;
            }
            for stream in [self.capture, self.playback] {
                self.operation(stream, StreamOperation::Flush)?;
            }
            // A record flush only affects the server. Already-delivered client
            // fragments, including a retained partial peek, must be dropped too.
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                ensure!(
                    Instant::now() < deadline,
                    "PCM capture reset did not quiesce"
                );
                let mut data = ptr::null();
                let mut length = 0;
                ensure!(
                    (self.api.pa_stream_peek)(self.capture, &mut data, &mut length) == 0,
                    "PCM capture reset read failed"
                );
                if length == 0 {
                    break;
                }
                ensure!(
                    length.is_multiple_of(FRAME_BYTES),
                    "unaligned PCM reset fragment"
                );
                let remaining = length
                    .checked_sub(self.capture_offset.get())
                    .context("PCM capture cursor changed during reset")?;
                ensure!(
                    (self.api.pa_stream_drop)(self.capture) == 0,
                    "PCM capture reset release failed"
                );
                if let Some(control) = &self.control {
                    control.dropped(remaining / FRAME_BYTES);
                }
                self.capture_offset.set(0);
            }
            self.capture_offset.set(0);
            if active {
                for stream in [self.playback, self.capture] {
                    self.operation(stream, StreamOperation::Cork(false))?;
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
    ) -> Result<(usize, usize)> {
        let mut received = 0;
        let mut written = 0;
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
                let span = capture_span(length, self.capture_offset.get(), pending.len())?;
                if !control.active() {
                    let remaining = length - self.capture_offset.get();
                    control.dropped(remaining / FRAME_BYTES);
                    received += remaining / FRAME_BYTES;
                    ensure!(
                        (self.api.pa_stream_drop)(self.capture) == 0,
                        "inactive PCM release failed"
                    );
                    self.capture_offset.set(0);
                    continue;
                }
                if span.is_empty() {
                    break;
                }
                if data.is_null() {
                    control.capture_hole(span.len() / FRAME_BYTES);
                }
                let bytes = if data.is_null() {
                    None
                } else {
                    Some(std::slice::from_raw_parts(data.cast::<u8>(), length))
                };
                for offset in (span.start..span.end).step_by(BLOCK_BYTES) {
                    let count = (span.end - offset).min(BLOCK_BYTES) / 4;
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
                    let (frames, micros, peak_micros) = processor.fft_metrics();
                    control.fft(frames, micros, peak_micros);
                    for sample in &samples[..count] {
                        ensure!(
                            sample.is_finite(),
                            "PCM processor produced non-finite samples"
                        );
                        pending.extend(sample.to_le_bytes());
                    }
                    control.record(count / CHANNELS, pending.len() / FRAME_BYTES);
                }
                received += span.len() / FRAME_BYTES;
                self.capture_offset.set(span.end);
                if span.end != length {
                    break;
                }
                ensure!(
                    (self.api.pa_stream_drop)(self.capture) == 0,
                    "PCM fragment release failed"
                );
                self.capture_offset.set(0);
            }
            let writable = self.writable_bytes()?;
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
                written = count / FRAME_BYTES;
            }
            let mut latency = 0;
            let mut valid = true;
            for stream in [self.capture, self.playback] {
                let mut micros = 0;
                let mut negative = 0;
                if (self.api.pa_stream_get_latency)(stream, &mut micros, &mut negative) == 0
                    && negative == 0
                {
                    latency += micros;
                } else {
                    valid = false;
                }
            }
            control.latency(valid.then_some(
                latency
                    + pending.len() as u64 * 1_000_000 / FRAME_BYTES as u64 / SAMPLE_RATE as u64,
            ));
        }
        Ok((received, written))
    }
    fn writable_bytes(&self) -> Result<usize> {
        // SAFETY: this owned connection accesses its ready stream only on its worker.
        let bytes = unsafe { (self.api.pa_stream_writable_size)(self.playback) };
        ensure!(bytes != usize::MAX, "PCM output failed");
        Ok(bytes)
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: unregister while the retained control allocation and stream are
        // still live, before disconnect/unref can dispatch any teardown work.
        if !self.playback.is_null() {
            unsafe {
                (self.api.pa_stream_set_underflow_callback)(self.playback, None, ptr::null_mut());
            }
        }
        if let Some(control) = &self.control {
            control.register_wake(None);
        }
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

#[derive(Default)]
struct ProgressClock {
    capture: Option<Instant>,
    write: Option<Instant>,
}
impl ProgressClock {
    fn observe(&mut self, now: Instant, received: bool, written: bool, control: &AudioControl) {
        if received {
            if let Some(then) = self.capture.replace(now) {
                control.note_timing(
                    TimingMetric::CaptureGap,
                    micros(now.saturating_duration_since(then)),
                );
            }
        }
        if written {
            if let Some(then) = self.write.replace(now) {
                control.note_timing(
                    TimingMetric::WriteGap,
                    micros(now.saturating_duration_since(then)),
                );
            }
        }
    }
    fn capture_gap(&self, now: Instant) -> u64 {
        self.capture
            .map_or(0, |then| micros(now.saturating_duration_since(then)))
    }
}
fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

pub(crate) struct Bridge {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
    control: AudioControl,
    target: Arc<Mutex<(u64, Option<String>)>>,
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
        let target = Arc::new(Mutex::new((0u64, Some(sink.clone()))));
        let worker_target = target.clone();
        let source = source.to_owned();
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("myx-pcm".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let mut connection = Connection::open(
                        &library, &server, &cookie, &sink, &source, &stopped, deadline,
                    )?;
                    connection.register_control(state.clone());
                    let mut pending = VecDeque::with_capacity(MAX_PENDING);
                    connection.reset(true)?;
                    // A new null sink may initially have a long idle quantum.
                    // Prime/drain its monitor before publishing runtime ready,
                    // keeping that setup delay outside active playback.
                    let mut primed = 0;
                    while primed < crate::pcm::BLOCK_FRAMES {
                        ensure!(
                            !stopped.load(Ordering::Acquire) && Instant::now() < deadline,
                            "PCM monitor priming cancelled or timed out"
                        );
                        connection.dispatch(true)?;
                        primed += connection
                            .transfer(&mut pending, processor.as_mut(), &state)?
                            .0;
                        thread::park_timeout(Duration::from_millis(2));
                    }
                    processor.reset();
                    connection.reset(false)?;
                    let mut generation = u64::MAX;
                    state.health(1);
                    let _ = tx.send(true);
                    let mut connection = Some(connection);
                    let mut current_revision = 0;
                    let mut retries = 0u32;
                    let mut progress = ProgressClock::default();
                    let mut observed_underflows = state.underflow_count();
                    let mut previous_iteration = None;
                    while !stopped.load(Ordering::Acquire) {
                        if state.active() && state.generation() == generation {
                            let now = Instant::now();
                            if let Some(previous) = previous_iteration.replace(now) {
                                state.note_timing(
                                    TimingMetric::IterationGap,
                                    micros(now.duration_since(previous)),
                                );
                            }
                        } else {
                            previous_iteration = None;
                        }
                        let (revision, target) = worker_target
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone();
                        if revision != current_revision {
                            progress = ProgressClock::default();
                            previous_iteration = None;
                            connection.take();
                            state.dropped(pending.len() / FRAME_BYTES);
                            pending.clear();
                            processor.reset();
                            state.health(2);
                            current_revision = revision;
                            retries = 0;
                        }
                        if connection.is_none() {
                            if let Some(sink) = target.as_deref().filter(|_| retries < 5) {
                                state.recovering();
                                match Connection::open(
                                    &library,
                                    &server,
                                    &cookie,
                                    sink,
                                    &source,
                                    &stopped,
                                    Instant::now() + Duration::from_millis(500),
                                ) {
                                    Ok(mut reopened) => {
                                        retries += 1;
                                        reopened.register_control(state.clone());
                                        connection = Some(reopened);
                                        generation = u64::MAX;
                                    }
                                    Err(_) => {
                                        retries += 1;
                                        thread::park_timeout(Duration::from_millis(
                                            250u64 << (retries - 1),
                                        ));
                                        continue;
                                    }
                                }
                            } else {
                                thread::park_timeout(Duration::from_secs(1));
                                continue;
                            }
                        }
                        let live = connection.as_ref().expect("opened connection");
                        let result = (|| -> Result<()> {
                            let dispatch_started = Instant::now();
                            live.dispatch(true)?;
                            if state.active() && state.generation() == generation {
                                state.note_timing(
                                    TimingMetric::Dispatch,
                                    micros(dispatch_started.elapsed()),
                                );
                            }
                            if state.generation() != generation {
                                generation = state.generation();
                                state.dropped(pending.len() / FRAME_BYTES);
                                pending.clear();
                                processor.reset();
                                state.did_reset();
                                live.reset(state.active())?;
                                progress = ProgressClock::default();
                                observed_underflows = state.underflow_count();
                                previous_iteration = None;
                            }
                            let underflows = state.underflow_count();
                            if state.active() && underflows != observed_underflows {
                                state.note_underflow(
                                    underflows,
                                    progress.capture_gap(Instant::now()),
                                    pending.len() / FRAME_BYTES,
                                    live.writable_bytes()? / FRAME_BYTES,
                                );
                            }
                            observed_underflows = underflows;
                            let transfer_started = Instant::now();
                            let (received, written) =
                                live.transfer(&mut pending, processor.as_mut(), &state)?;
                            let now = Instant::now();
                            if state.active() && state.generation() == generation {
                                state.note_timing(
                                    TimingMetric::Transfer,
                                    micros(now.duration_since(transfer_started)),
                                );
                                progress.observe(now, received > 0, written > 0, &state);
                            } else {
                                progress = ProgressClock::default();
                            }
                            if worker_target
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .0
                                == current_revision
                            {
                                state.health(1);
                            }
                            // A bounded transfer may leave more PCM in libpulse's
                            // userspace queue. No new socket event is required for
                            // that data: drain progress before blocking again.
                            if received == 0 && written == 0 {
                                // Park at most approximately one 128-frame quantum
                                // while active; control/idle waits remain event-driven.
                                let wait_started = Instant::now();
                                let was_active = state.active();
                                let wait_generation = state.generation();
                                live.wait(if was_active { 3 } else { 1000 })?;
                                if was_active
                                    && state.active()
                                    && state.generation() == wait_generation
                                {
                                    state.note_timing(
                                        TimingMetric::Wait,
                                        micros(wait_started.elapsed()),
                                    );
                                }
                                state.woke();
                            }
                            Ok(())
                        })();
                        if result.is_err() {
                            connection.take();
                            state.dropped(pending.len() / FRAME_BYTES);
                            pending.clear();
                            processor.reset();
                            state.health(2);
                            thread::park_timeout(Duration::from_millis(
                                250u64 << retries.saturating_sub(1).min(4),
                            ));
                        }
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
            target,
        };
        if !matches!(
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
            Ok(true)
        ) {
            bridge.stop()?;
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
    pub(crate) fn alive(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }
    pub(crate) fn output_recovery(&self) {
        self.control.recovering();
    }
    pub(crate) fn retarget(&self, sink: Option<String>) {
        self.control.health(2);
        let mut target = self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        target.0 = target.0.wrapping_add(1);
        target.1 = sink;
        drop(target);
        self.control.reset();
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
    pub(crate) fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.control.wake();
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
pub(crate) mod tests {
    use super::*;
    use crate::{private, process::OwnedChild};
    use std::process::{Command, Stdio};
    #[test]
    fn stream_reply_requires_a_positive_native_acknowledgement() {
        let reply = StreamReply::default();
        assert!(reply.finish().is_err());
        // SAFETY: callback runs synchronously with this retained stack reply.
        unsafe {
            stream_reply(
                ptr::null_mut(),
                0,
                (&reply as *const StreamReply).cast_mut().cast(),
            );
        }
        assert_eq!(reply.success.get(), Some(false));
        assert!(reply.finish().is_err());
        unsafe {
            stream_reply(
                ptr::null_mut(),
                1,
                (&reply as *const StreamReply).cast_mut().cast(),
            );
        }
        assert_eq!(reply.success.get(), Some(true));
        reply.finish().unwrap();
    }
    #[test]
    fn progress_timing_keeps_maxima_and_excludes_an_inactive_epoch() {
        use crate::pcm::TimingMetric;
        let control = AudioControl::default();
        let start = Instant::now();
        let mut clock = ProgressClock::default();
        clock.observe(start, true, true, &control);
        clock.observe(start + Duration::from_millis(2), false, true, &control);
        clock.observe(start + Duration::from_millis(5), true, true, &control);
        assert_eq!(clock.capture_gap(start + Duration::from_millis(8)), 3000);
        clock = ProgressClock::default();
        clock.observe(start + Duration::from_secs(5), true, true, &control);
        control.note_timing(TimingMetric::Dispatch, 120);
        control.note_timing(TimingMetric::Dispatch, 10);
        control.note_timing(TimingMetric::Transfer, 80);
        control.note_timing(TimingMetric::Wait, 40);
        control.note_timing(TimingMetric::IterationGap, 5200);
        control.note_underflow(2, 3000, 64, 128);
        let s = control.stats();
        assert_eq!(s.timing.max_capture_gap_micros, 5000);
        assert_eq!(s.timing.max_write_gap_micros, 3000);
        assert_eq!(s.timing.max_dispatch_micros, 120);
        assert_eq!(s.timing.max_transfer_micros, 80);
        assert_eq!(s.timing.max_wait_micros, 40);
        assert_eq!(s.timing.max_iteration_gap_micros, 5200);
        assert_eq!(s.timing.last_underflow_capture_gap_micros, 3000);
        assert_eq!(s.timing.last_underflow_pending_frames, 64);
        assert_eq!(s.timing.last_underflow_write_credit_frames, 128);
        assert_eq!(s.timing.observed_underflows, 2);
        assert_eq!(s.processed_frames, 0);
        assert_eq!(s.resets, 0);
        assert!(!control.active());
    }
    #[test]
    fn capture_burst_cannot_process_ahead_of_the_transport_latency_budget() {
        let span = capture_span(MAX_CAPTURE, 0, 0).unwrap();
        let buffered_frames = (span.len() + MAX_OUTPUT + BLOCK_BYTES) / FRAME_BYTES;
        let latency = Duration::from_secs_f64(buffered_frames as f64 / f64::from(SAMPLE_RATE));
        assert!(
            latency <= Duration::from_millis(50),
            "processed pending, output reserve and monitor fragment total {latency:?}"
        );
    }
    #[test]
    fn large_capture_fragments_are_consumed_in_bounded_frame_aligned_spans() {
        assert_eq!(capture_span(8192, 0, 0).unwrap(), 0..1024);
        assert_eq!(capture_span(8192, 1024, 1024).unwrap(), 1024..1024);
        assert_eq!(capture_span(8192, 1024, 0).unwrap(), 1024..2048);
        assert!(capture_span(8193, 0, 0).is_err());
        assert!(capture_span(8192, 8200, 0).is_err());
        assert!(capture_span(MAX_CAPTURE + FRAME_BYTES, 0, 0).is_err());
    }

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
        run_route(
            Box::new(HalfGain(Arc::new(AtomicBool::new(false)))),
            0.125,
            false,
        );
    }
    #[test]
    #[ignore = "owner fixture required; private cached source with one injected scheduling delay"]
    fn native_transport_absorbs_a_bounded_scheduling_gap() {
        struct DelayedHalfGain {
            inner: HalfGain,
            frames: usize,
            delayed: bool,
        }
        impl AudioProcessor for DelayedHalfGain {
            fn process(&mut self, samples: &mut [f32]) {
                self.frames += samples.len() / CHANNELS;
                if !self.delayed && self.frames >= SAMPLE_RATE as usize {
                    self.delayed = true;
                    // Test-only fault injection: no new writes during a scheduling
                    // gap comparable to the measured 39ms capture-delivery gap.
                    // Sleep can overshoot by an unbounded scheduler delay. A
                    // short runnable stall models this specific delay instead.
                    let started = Instant::now();
                    let until = started + Duration::from_millis(38);
                    while Instant::now() < until {
                        std::hint::spin_loop();
                    }
                    let elapsed = started.elapsed();
                    assert!(elapsed <= Duration::from_millis(40),
                        "fault injection overshot its 40ms bound ({elapsed:?}); invalid fixture run");
                }
                self.inner.process(samples);
            }
            fn reset(&mut self) {
                self.inner.reset();
            }
        }
        run_route(
            Box::new(DelayedHalfGain {
                inner: HalfGain(Arc::new(AtomicBool::new(false))),
                frames: 0,
                delayed: false,
            }),
            0.125,
            false,
        );
    }
    struct NativeServer {
        server: OwnedChild,
        directory: tempfile::TempDir,
        tools: std::path::PathBuf,
        cookie: std::path::PathBuf,
        address: String,
        port: u16,
        deadline: Instant,
    }
    impl NativeServer {
        fn pactl(&self, arguments: &[&str]) -> Vec<u8> {
            let mut command = Command::new(self.tools.join("bin/pactl"));
            command
                .args(arguments)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("PULSE_SERVER", &self.address)
                .env("PULSE_COOKIE", &self.cookie);
            crate::process::output(
                &mut command,
                Instant::now() + Duration::from_secs(1),
                131072,
            )
            .unwrap()
        }

        fn cache_step_signal(&self) {
            let lead_frames = SAMPLE_RATE as usize / 2;
            // Stay valid through bounded recovery, not just the measurement,
            // even if setup and control replies consume their allowed time.
            let signal_frames = SAMPLE_RATE as usize * 29;
            let total_frames = SAMPLE_RATE as usize * 30;
            let data_bytes = (total_frames * CHANNELS * 2) as u32;
            let mut wave = Vec::with_capacity(44 + data_bytes as usize);
            wave.extend(b"RIFF");
            wave.extend((36 + data_bytes).to_le_bytes());
            wave.extend(b"WAVEfmt ");
            wave.extend(16u32.to_le_bytes());
            wave.extend(1u16.to_le_bytes());
            wave.extend((CHANNELS as u16).to_le_bytes());
            wave.extend(SAMPLE_RATE.to_le_bytes());
            wave.extend((SAMPLE_RATE * CHANNELS as u32 * 2).to_le_bytes());
            wave.extend((CHANNELS as u16 * 2).to_le_bytes());
            wave.extend(16u16.to_le_bytes());
            wave.extend(b"data");
            wave.extend(data_bytes.to_le_bytes());
            wave.resize(44 + lead_frames * CHANNELS * 2, 0);
            for _ in 0..signal_frames {
                // PCM16 8192 converts exactly to float32 0.25; right stays zero.
                wave.extend(8192i16.to_le_bytes());
                wave.extend(0i16.to_le_bytes());
            }
            wave.resize(44 + data_bytes as usize, 0);
            let path = self.directory.path().join("step.wav");
            private::write_private(&path, &wave).unwrap();
            self.pactl(&["upload-sample", path.to_str().unwrap(), "myx_step"]);
            let cached: serde_json::Value =
                serde_json::from_slice(&self.pactl(&["--format=json", "list", "samples"])).unwrap();
            let sample = cached
                .as_array()
                .unwrap()
                .iter()
                .find(|sample| sample["name"] == "myx_step")
                .expect("uploaded source missing from the private cache");
            assert_eq!(sample["sample_specification"], "float32le 2ch 44100Hz");
            assert_eq!(sample["duration"].as_f64(), Some(30.0));
        }

        fn start() -> Self {
            let tools = crate::native_audio::owner_tools();
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
            while std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
                || std::os::unix::net::UnixStream::connect(temp.path().join("native")).is_err()
            {
                assert!(Instant::now() < deadline && server.child.try_wait().unwrap().is_none());
                thread::sleep(Duration::from_millis(5));
            }
            let address = format!("unix:{}", temp.path().join("native").display());
            Self {
                server,
                directory: temp,
                tools,
                cookie,
                address,
                port,
                deadline,
            }
        }
    }
    #[test]
    #[ignore = "owner fixture required; real private PulseAudio, synthetic silence only"]
    fn native_reset_discards_retained_and_queued_client_capture() {
        let mut fixture = NativeServer::start();
        let stop = AtomicBool::new(false);
        let mut connection = Connection::open(
            &fixture.tools.join("lib/libpulse.0.dylib"),
            &fixture.address,
            &fixture.cookie,
            "myx_input",
            "myx_input.monitor",
            &stop,
            fixture.deadline,
        )
        .unwrap();
        let control = AudioControl::default();
        connection.register_control(control.clone());
        connection.reset(true).unwrap();
        control.set_active(true);
        loop {
            connection.iterate().unwrap();
            let available = unsafe { (connection.api.pa_stream_readable_size)(connection.capture) };
            if available >= FRAME_BYTES * 2 && available != usize::MAX {
                break;
            }
            assert!(Instant::now() < fixture.deadline);
            thread::sleep(Duration::from_millis(2));
        }
        let mut pending = VecDeque::from(vec![0u8; MAX_PENDING - FRAME_BYTES]);
        let mut processor = HalfGain(Arc::new(AtomicBool::new(false)));
        connection
            .transfer(&mut pending, &mut processor, &control)
            .unwrap();
        assert_eq!(
            connection.capture_offset.get(),
            FRAME_BYTES,
            "fixture did not retain a partial peek"
        );
        let mut retained = ptr::null();
        let mut retained_length = 0;
        unsafe {
            assert_eq!(
                (connection.api.pa_stream_peek)(
                    connection.capture,
                    &mut retained,
                    &mut retained_length
                ),
                0
            );
        }
        loop {
            connection.iterate().unwrap();
            let queued = unsafe { (connection.api.pa_stream_readable_size)(connection.capture) };
            if queued > retained_length && queued != usize::MAX {
                break;
            }
            assert!(
                Instant::now() < fixture.deadline,
                "fixture did not queue another capture fragment"
            );
            thread::sleep(Duration::from_millis(2));
        }
        connection.reset(false).unwrap();
        let mut data = ptr::null();
        let mut length = 0;
        unsafe {
            assert_eq!(
                (connection.api.pa_stream_peek)(connection.capture, &mut data, &mut length),
                0
            );
            assert_eq!(
                length, 0,
                "server flush left pre-reset PCM in the client's retained peek"
            );
            assert_eq!(
                (connection.api.pa_stream_readable_size)(connection.capture),
                0,
                "queued pre-reset PCM survived"
            );
        }
        drop(connection);
        fixture.server.stop().unwrap();
    }
    #[test]
    #[ignore = "owner fixture required; server-generated sine through private null sinks, inaudible"]
    fn native_server_generated_sine_has_no_steady_output_gaps() {
        let mut fixture = NativeServer::start();
        let mut load = Command::new(fixture.tools.join("bin/pactl"));
        load.args([
            "load-module",
            "module-sine",
            "sink=myx_input",
            "frequency=441",
            "rate=44100",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PULSE_SERVER", &fixture.address)
        .env("PULSE_COOKIE", &fixture.cookie);
        crate::process::output(&mut load, fixture.deadline, 4096).unwrap();
        let control = AudioControl::default();
        let input_seen = Arc::new(AtomicBool::new(false));
        let mut bridge = Bridge::start(
            Endpoint {
                tools: &fixture.tools,
                server: &fixture.address,
                cookie: &fixture.cookie,
                sink: "test_output",
                source: "myx_input.monitor",
            },
            control.clone(),
            Box::new(HalfGain(input_seen.clone())),
            fixture.deadline,
        )
        .unwrap();
        let stop = AtomicBool::new(false);
        let observer = Connection::open(
            &fixture.tools.join("lib/libpulse.0.dylib"),
            &fixture.address,
            &fixture.cookie,
            "myx_input",
            "test_output.monitor",
            &stop,
            fixture.deadline,
        )
        .unwrap();
        observer.reset(true).unwrap();
        control.set_active(true);
        let deadline = Instant::now() + Duration::from_secs(6);
        let coefficient = 2.0 * (std::f32::consts::TAU / 100.0).cos();
        let mut previous = [0.0f32; 2];
        let mut seen_samples = 0usize;
        let mut measured_samples = 0usize;
        let mut bad_samples = 0usize;
        let mut steady = None;
        let mut steady_underflows = 0;
        let mut zero_run = 0usize;
        while Instant::now() < deadline {
            observer.iterate().unwrap();
            unsafe {
                for _ in 0..64 {
                    let mut data = ptr::null();
                    let mut length = 0;
                    assert_eq!(
                        (observer.api.pa_stream_peek)(observer.capture, &mut data, &mut length),
                        0
                    );
                    if length == 0 {
                        break;
                    }
                    assert!(length.is_multiple_of(FRAME_BYTES));
                    if data.is_null() {
                        if steady.is_some() {
                            bad_samples += length / FRAME_BYTES;
                        }
                    } else {
                        for frame in std::slice::from_raw_parts(data.cast::<u8>(), length)
                            .chunks_exact(FRAME_BYTES)
                        {
                            let sample = f32::from_le_bytes(frame[..4].try_into().unwrap());
                            if sample.abs() > 0.01 {
                                seen_samples += 1;
                            }
                            if steady.is_none() && seen_samples >= 4096 {
                                steady = Some(Instant::now());
                                steady_underflows = control.underflow_count();
                            }
                            if steady.is_some() {
                                measured_samples += 1;
                                if (sample - coefficient * previous[1] + previous[0]).abs() > 0.001
                                {
                                    bad_samples += 1;
                                }
                                zero_run = if sample.abs() < 1e-6 { zero_run + 1 } else { 0 };
                                if zero_run > 8 {
                                    bad_samples += 1;
                                }
                            }
                            previous = [previous[1], sample];
                        }
                    }
                    assert_eq!((observer.api.pa_stream_drop)(observer.capture), 0);
                }
            }
            if steady.is_some_and(|since| since.elapsed() >= Duration::from_secs(3)) {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        println!("Server-generated sine continuity: samples={measured_samples}, bad={bad_samples}, stats={:?}", control.stats());
        assert!(input_seen.load(Ordering::Acquire));
        assert!(steady.is_some_and(|since| since.elapsed() >= Duration::from_secs(3)));
        assert!(measured_samples > SAMPLE_RATE as usize * 2);
        assert_eq!(
            bad_samples, 0,
            "server-generated input had output discontinuities"
        );
        assert_eq!(
            control.underflow_count(),
            steady_underflows,
            "server-generated output underruns occurred during the measurement window"
        );
        control.set_active(false);
        drop(observer);
        bridge.stop().unwrap();
        fixture.server.stop().unwrap();
    }
    pub(crate) fn run_route(
        processor: Box<dyn AudioProcessor>,
        expected_left: f32,
        recover_output: bool,
    ) {
        struct TimedInput {
            inner: Box<dyn AudioProcessor>,
            first: Arc<std::sync::OnceLock<Instant>>,
        }
        impl AudioProcessor for TimedInput {
            fn process(&mut self, samples: &mut [f32]) {
                if self.first.get().is_none() && samples.iter().any(|sample| sample.abs() > 0.01) {
                    let _ = self.first.set(Instant::now());
                }
                self.inner.process(samples);
            }
            fn reset(&mut self) {
                self.inner.reset();
            }
            fn fft_metrics(&self) -> (u64, u64, u64) {
                self.inner.fft_metrics()
            }
        }
        let first_input = Arc::new(std::sync::OnceLock::new());
        let processor = Box::new(TimedInput {
            inner: processor,
            first: first_input.clone(),
        });
        let mut fixture = NativeServer::start();
        let tools = fixture.tools.clone();
        let cookie = fixture.cookie.clone();
        let address = fixture.address.clone();
        let deadline = fixture.deadline;
        let port = fixture.port;
        // Upload before starting the worker; no application-thread producer
        // shares the observation loop during the measured signal transport.
        fixture.cache_step_signal();
        let control = AudioControl::default();
        let mut bridge = Bridge::start(
            Endpoint {
                tools: &tools,
                server: &address,
                cookie: &cookie,
                sink: "test_output",
                source: "myx_input.monitor",
            },
            control.clone(),
            processor,
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
        peer.reset(false).unwrap();
        peer.operation(peer.capture, StreamOperation::Cork(false))
            .unwrap();
        control.set_active(true);
        // Allow cork/flush operations to reach both contexts before timing.
        for _ in 0..20 {
            peer.iterate().unwrap();
            thread::sleep(Duration::from_millis(2));
        }
        let source_started = Instant::now();
        fixture.pactl(&["play-sample", "myx_step", "myx_input"]);
        let mut first_processed = None;
        let mut signal_frames = 0usize;
        let mut gain_verified = false;
        let mut steady_since = None;
        let mut discontinuous_frames = 0usize;
        let mut steady_frames = 0usize;
        let mut steady_output_underflows = 0;
        let mut iterations = 0;
        let mut captured_frames = 0;
        let deadline = Instant::now() + Duration::from_secs(8);
        while (steady_since.is_none_or(|since: Instant| since.elapsed() < Duration::from_secs(3))
            || steady_frames < SAMPLE_RATE as usize * 3)
            && Instant::now() < deadline
        {
            iterations += 1;
            if !bridge.running() {
                bridge.stop().unwrap();
                panic!("PCM worker exited before processing");
            }
            peer.iterate().unwrap();
            // SAFETY: ready private capture; every peeked buffer is consumed
            // before drop. The source lives entirely in the server sample cache.
            unsafe {
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
                        if data.is_null() && gain_verified {
                            discontinuous_frames += length / FRAME_BYTES;
                            steady_frames += length / FRAME_BYTES;
                        }
                        if !data.is_null() {
                            let samples = std::slice::from_raw_parts(data.cast::<u8>(), length);
                            for frame in samples.chunks_exact(FRAME_BYTES) {
                                let left = f32::from_le_bytes(frame[..4].try_into().unwrap());
                                let right = f32::from_le_bytes(frame[4..].try_into().unwrap());
                                assert!(right.abs() < 1e-6, "stereo channels mixed");
                                if gain_verified && (left - expected_left).abs() >= 1e-4 {
                                    discontinuous_frames += 1;
                                }
                                if gain_verified {
                                    steady_frames += 1;
                                }
                                if left.abs() > 0.01 {
                                    first_processed.get_or_insert_with(Instant::now);
                                    signal_frames += 1;
                                    // Allow actual EQ's step response to settle,
                                    // while timing the first output transition.
                                    if signal_frames >= 4096 {
                                        assert!((left - expected_left).abs() < 1e-4, "unexpected processed native gain: {left}, expected {expected_left}");
                                        gain_verified = true;
                                        if steady_since.is_none() {
                                            steady_since = Some(Instant::now());
                                            steady_output_underflows =
                                                control.stats().playback_underflows;
                                        }
                                    }
                                }
                            }
                        }
                        assert_eq!((peer.api.pa_stream_drop)(peer.capture), 0);
                    }
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
        let delay = first_processed
            .unwrap_or_else(|| panic!("processed PCM never reached output: {:?}; captured={captured_frames}; iterations={iterations}", control.stats()))
            .duration_since(*first_input.get().expect("signal never reached DSP input"));
        let source_lead_in = first_input.get().unwrap().duration_since(source_started);
        println!(
            "private DSP-input/output latency: {delay:?}; cached source lead-in={source_lead_in:?}; steady_frames={steady_frames}; output={:?}",
            control.stats()
        );
        assert!(
            gain_verified,
            "native output never reached expected steady-state DSP gain"
        );
        assert!(
            steady_since.is_some_and(|since| since.elapsed() >= Duration::from_secs(3)),
            "native output did not sustain the continuity measurement window"
        );
        assert!(
            steady_frames >= SAMPLE_RATE as usize * 3,
            "less than three seconds of steady PCM was observed"
        );
        assert_eq!(
            discontinuous_frames,
            0,
            "steady synthetic PCM contains gaps/clicks; new output underruns={}: {:?}",
            control.stats().playback_underflows - steady_output_underflows,
            control.stats()
        );
        assert_eq!(
            control.underflow_count(),
            steady_output_underflows,
            "cached-source output underruns occurred during the measurement window"
        );
        assert!(
            delay <= Duration::from_millis(50),
            "PCM bridge exceeds 50ms latency budget: {delay:?}"
        );
        assert!(control.stats().peak_pending_frames <= MAX_PENDING as u64 / FRAME_BYTES as u64);
        control.set_active(false);
        thread::sleep(Duration::from_millis(30));
        let paused_frames = control.stats().processed_frames;
        let paused_wakes = control.stats().worker_wakes;
        thread::sleep(Duration::from_millis(20));
        assert_eq!(
            control.stats().processed_frames,
            paused_frames,
            "paused PCM still processed"
        );
        assert!(
            control.stats().worker_wakes.saturating_sub(paused_wakes) < 10,
            "paused worker busy-looped"
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
        drop(peer);
        if recover_output {
            bridge.retarget(None);
            assert!(!control.is_ready());
            let sinks: serde_json::Value =
                serde_json::from_slice(&fixture.pactl(&["--format=json", "list", "sinks"]))
                    .unwrap();
            let output = sinks
                .as_array()
                .unwrap()
                .iter()
                .find(|sink| sink["name"] == "test_output")
                .unwrap();
            let module = output["owner_module"].as_u64().unwrap();
            fixture.pactl(&["unload-module", &module.to_string()]);
            fixture.pactl(&[
                "load-module",
                "module-null-sink",
                "sink_name=test_recovered",
                "format=float32le",
                "rate=44100",
                "channels=2",
                "norewinds=true",
            ]);
            bridge.retarget(Some("test_recovered".into()));
            control.set_active(true);
            let deadline = Instant::now() + Duration::from_secs(3);
            while !control.is_ready() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                control.is_ready(),
                "output did not recover on existing worker"
            );
            assert!(control.stats().recovery_attempts > 0);
            let recovered = Connection::open(
                &tools.join("lib/libpulse.0.dylib"),
                &address,
                &cookie,
                "myx_input",
                "test_recovered.monitor",
                &stop,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            recovered.reset(false).unwrap();
            recovered
                .operation(recovered.capture, StreamOperation::Cork(false))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut recovered_signal_frames = 0;
            while recovered_signal_frames < crate::pcm::BLOCK_FRAMES * 4
                && Instant::now() < deadline
            {
                recovered.iterate().unwrap();
                // SAFETY: inspect only this fixture's synthetic recovered monitor,
                // consuming each borrowed fragment before the native drop.
                unsafe {
                    for _ in 0..64 {
                        let mut data = ptr::null();
                        let mut length = 0;
                        assert_eq!(
                            (recovered.api.pa_stream_peek)(
                                recovered.capture,
                                &mut data,
                                &mut length
                            ),
                            0
                        );
                        if length == 0 {
                            break;
                        }
                        assert!(length.is_multiple_of(FRAME_BYTES));
                        if !data.is_null() {
                            for frame in std::slice::from_raw_parts(data.cast::<u8>(), length)
                                .chunks_exact(FRAME_BYTES)
                            {
                                let left = f32::from_le_bytes(frame[..4].try_into().unwrap());
                                let right = f32::from_le_bytes(frame[4..].try_into().unwrap());
                                assert!(right.abs() < 1e-6, "recovered stereo channels mixed");
                                if left.abs() > 0.01 {
                                    assert!(
                                        (left - expected_left).abs() < 1e-4,
                                        "recovered DSP gain mismatch"
                                    );
                                    recovered_signal_frames += 1;
                                }
                            }
                        }
                        assert_eq!((recovered.api.pa_stream_drop)(recovered.capture), 0);
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(
                recovered_signal_frames >= crate::pcm::BLOCK_FRAMES * 4,
                "recovered output never delivered the expected processed signal"
            );
            drop(recovered);
            control.set_active(false);
        }
        bridge.stop().unwrap();
        assert!(!control.is_ready());
        fixture.server.stop().unwrap();
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    }

    pub(crate) fn cancelled_startup() {
        let tools = crate::native_audio::owner_tools();
        let stop = AtomicBool::new(true);
        let result = Connection::open(
            &tools.join("lib/libpulse.0.dylib"),
            "unix:/absent",
            Path::new("/absent"),
            "absent",
            "absent",
            &stop,
            Instant::now() + Duration::from_secs(1),
        );
        assert!(result.err().unwrap().to_string().contains("cancelled"));
    }
}
