use std::sync::atomic::{AtomicBool, Ordering};
static SIGNALLED: AtomicBool = AtomicBool::new(false);
extern "C" fn terminate(_: libc::c_int) {
    SIGNALLED.store(true, Ordering::Release);
}
pub fn install() {
    unsafe {
        libc::signal(libc::SIGTERM, terminate as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, terminate as *const () as libc::sighandler_t);
        // Detached owner lifetime is deliberately independent of its launching terminal.
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}
pub fn signalled() -> bool {
    SIGNALLED.load(Ordering::Acquire)
}
