//! Read-only default-output discovery and an owned native Bonjour registration.
// Adapted from tools/soloist-elfuse under the project's MIT license.

use std::cell::Cell;
use std::ffi::CString;
use std::io;
use std::mem::size_of;
use std::ptr;
use std::time::Instant;

// DNS-SD TXT records are length-prefixed strings, without NUL terminators.
const TXT_RECORD: &[u8] = b"\x0bVERSION=1.0\x09CPath=/zc";

pub fn validate_registration(port: u16, identity: &str) -> io::Result<()> {
    if port == 0
        || !(1..=63).contains(&identity.len())
        || !identity
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Bonjour requires a nonzero port and 1..63 ASCII letters, digits or hyphens",
        ));
    }
    Ok(())
}

type DnsServiceRef = *mut libc::c_void;
type RegisterReply = unsafe extern "C" fn(
    DnsServiceRef,
    u32,
    i32,
    *const libc::c_char,
    *const libc::c_char,
    *const libc::c_char,
    *mut libc::c_void,
);

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const libc::c_void,
        data_size: *mut u32,
        data: *mut libc::c_void,
    ) -> i32;
}

#[link(name = "System")]
unsafe extern "C" {
    fn DNSServiceRegister(
        service: *mut DnsServiceRef,
        flags: u32,
        interface_index: u32,
        name: *const libc::c_char,
        service_type: *const libc::c_char,
        domain: *const libc::c_char,
        host: *const libc::c_char,
        port: u16,
        txt_len: u16,
        txt: *const libc::c_void,
        callback: Option<RegisterReply>,
        context: *mut libc::c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(service: DnsServiceRef) -> libc::c_int;
    fn DNSServiceProcessResult(service: DnsServiceRef) -> i32;
    fn DNSServiceRefDeallocate(service: DnsServiceRef);
}

fn api_error(operation: &str, code: i32) -> io::Error {
    io::Error::other(format!("{operation} failed ({code})"))
}

fn read_property(object: u32, selector: [u8; 4]) -> io::Result<u32> {
    let address = PropertyAddress {
        selector: u32::from_be_bytes(selector),
        scope: u32::from_be_bytes(*b"glob"),
        element: 0,
    };
    let mut size = size_of::<u32>() as u32;
    let mut value = 0_u32;
    // SAFETY: the address and both output buffers have the exact CoreAudio ABI
    // layouts and remain valid for this synchronous property query.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut size,
            (&mut value as *mut u32).cast(),
        )
    };
    if status != 0 {
        return Err(api_error("CoreAudio property query", status));
    }
    if size != size_of::<u32>() as u32 || value == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CoreAudio returned an invalid default output or buffer size",
        ));
    }
    Ok(value)
}

/// Return the current default output's object ID and current buffer frames.
/// This neither enumerates devices nor changes any audio property.
pub fn default_output() -> io::Result<(u32, u32)> {
    let device = read_property(1, *b"dOut")?;
    let frames = read_property(device, *b"fsiz")?;
    Ok((device, frames))
}

#[derive(Default)]
struct CallbackState {
    confirmed: Cell<bool>,
    error: Cell<i32>,
}

unsafe extern "C" fn registration_reply(
    _service: DnsServiceRef,
    _flags: u32,
    error: i32,
    _name: *const libc::c_char,
    _service_type: *const libc::c_char,
    _domain: *const libc::c_char,
    context: *mut libc::c_void,
) {
    // Never inspect callback strings, allocate, log, or panic across the ABI.
    if context.is_null() {
        return;
    }
    // SAFETY: register supplies a boxed state that outlives the service ref.
    // DNS-SD invokes this callback synchronously during ProcessResult.
    let state = unsafe { &*context.cast::<CallbackState>() };
    if error != 0 {
        if state.error.get() == 0 {
            state.error.set(error);
        }
    } else {
        state.confirmed.set(true);
    }
}

/// An advertisement withdrawn when dropped. Keep polling to observe errors.
pub struct Bonjour {
    service: DnsServiceRef,
    // The allocation remains at a fixed address even when Bonjour is moved.
    state: Box<CallbackState>,
}

// SAFETY: DNS-SD references may move between threads, but may not be processed
// concurrently. All access requires &mut self; the callback state stays boxed.
// Bonjour intentionally is not Sync.
unsafe impl Send for Bonjour {}

impl Bonjour {
    pub fn register(port: u16, identity: &str, deadline: Instant) -> io::Result<Self> {
        validate_registration(port, identity)?;
        let name = CString::new(identity)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid Bonjour identity"))?;
        let mut registration = Self {
            service: ptr::null_mut(),
            state: Box::default(),
        };
        let context = (&*registration.state as *const CallbackState)
            .cast_mut()
            .cast::<libc::c_void>();
        // SAFETY: all arguments have the DNS-SD ABI types. Register copies
        // the name/TXT strings; only the callback context must outlive it.
        // Null host selects the default hostname, and interface 0 means all.
        let status = unsafe {
            DNSServiceRegister(
                &mut registration.service,
                8, // kDNSServiceFlagsNoAutoRename
                0,
                name.as_ptr(),
                c"_spotify-connect._tcp".as_ptr(),
                c"local.".as_ptr(),
                ptr::null(),
                port.to_be(),
                TXT_RECORD.len() as u16,
                TXT_RECORD.as_ptr().cast(),
                Some(registration_reply),
                context,
            )
        };
        if status != 0 {
            return Err(api_error("Bonjour registration", status));
        }
        if registration.service.is_null() {
            return Err(io::Error::other("Bonjour returned no registration"));
        }
        loop {
            registration.callback_result()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Bonjour registration exceeded runtime readiness deadline",
                ));
            }
            if registration.state.confirmed.get() {
                return Ok(registration);
            }
            // Round up so a sub-millisecond remainder cannot busy-spin.
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128 - 1) as i32
                + i32::from(!remaining.subsec_nanos().is_multiple_of(1_000_000));
            match registration.process_ready(timeout_ms) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            }
        }
    }

    fn callback_result(&self) -> io::Result<()> {
        match self.state.error.get() {
            0 => Ok(()),
            code => Err(api_error("Bonjour callback", code)),
        }
    }

    fn process_ready(&mut self, timeout_ms: libc::c_int) -> io::Result<()> {
        self.callback_result()?;
        // SAFETY: only a successfully initialized, owned service is polled.
        let fd = unsafe { DNSServiceRefSockFD(self.service) };
        if fd < 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Bonjour registration socket closed",
            ));
        }
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one valid pollfd for the entire call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Bonjour registration socket failed",
            ));
        }
        if ready > 0 && descriptor.revents & libc::POLLIN != 0 {
            // SAFETY: the owned service's socket is readable; its callback
            // context is live. No other thread can process this reference.
            let status = unsafe { DNSServiceProcessResult(self.service) };
            if status != 0 {
                return Err(api_error("Bonjour result processing", status));
            }
        }
        self.callback_result()
    }

    /// Process available DNS-SD results without waiting for socket readiness.
    pub fn poll(&mut self) -> io::Result<()> {
        self.process_ready(0)
    }
}

impl Drop for Bonjour {
    fn drop(&mut self) {
        if !self.service.is_null() {
            // SAFETY: this reference is owned and deallocated exactly once,
            // before Rust drops the callback state, including on failure.
            unsafe { DNSServiceRefDeallocate(self.service) };
            self.service = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_rejects_invalid_public_identities_before_ffi() {
        for (port, identity) in [
            (0, "valid"),
            (1, ""),
            (1, "a.b"),
            (1, "space here"),
            (1, "café"),
            (1, "nul\0byte"),
            (1, &"a".repeat(64)),
        ] {
            assert_eq!(
                Bonjour::register(port, identity, Instant::now())
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(validate_registration(1, &"a".repeat(63)).is_ok());
    }

    #[test]
    fn txt_record_is_the_public_pairing_path() {
        let mut remaining = TXT_RECORD;
        let mut entries = Vec::new();
        while let Some((&length, rest)) = remaining.split_first() {
            let (entry, rest) = rest.split_at(usize::from(length));
            entries.push(entry);
            remaining = rest;
        }
        assert_eq!(
            entries,
            [b"VERSION=1.0".as_slice(), b"CPath=/zc".as_slice()]
        );
    }
}
