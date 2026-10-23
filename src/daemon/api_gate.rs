//! Bound waiting as well as HTTP concurrency so abandoned requests cannot wait
//! indefinitely and begin writes long after their client deadline.
use crate::webapi::ApiError;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub(super) struct ApiGate {
    state: Mutex<State>,
    changed: Condvar,
}
#[derive(Default)]
struct State {
    busy: bool,
    next: u64,
    waiting: VecDeque<u64>,
}
pub(super) struct Permit(Arc<ApiGate>);
impl ApiGate {
    pub(super) fn enter(self: &Arc<Self>, timeout: Duration) -> Result<Permit, ApiError> {
        let mut state = self.state.lock().map_err(|_| ApiError::Busy)?;
        if state.waiting.len() >= 16 {
            return Err(ApiError::Busy);
        }
        let ticket = state.next;
        state.next = state.next.wrapping_add(1);
        state.waiting.push_back(ticket);
        self.changed.notify_all();
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.busy || state.waiting.front() != Some(&ticket)
            })
            .map_err(|_| ApiError::Busy)?;
        if state.busy || state.waiting.front() != Some(&ticket) {
            if let Some(index) = state.waiting.iter().position(|pending| *pending == ticket) {
                state.waiting.remove(index);
            }
            self.changed.notify_all();
            return Err(ApiError::Busy);
        }
        state.waiting.pop_front();
        state.busy = true;
        Ok(Permit(self.clone()))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.busy = false;
            self.0.changed.notify_all();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn waiting_is_bounded_and_timeout_never_leaves_a_queued_operation() {
        let gate = Arc::new(ApiGate::default());
        let permit = gate.enter(Duration::from_millis(1)).unwrap();
        assert_eq!(
            gate.enter(Duration::from_millis(2)).err(),
            Some(ApiError::Busy)
        );
        drop(permit);
        assert!(gate.enter(Duration::from_millis(1)).is_ok());
    }
    #[test]
    fn release_wakes_waiters_without_polling() {
        let gate = Arc::new(ApiGate::default());
        let permit = gate.enter(Duration::from_millis(1)).unwrap();
        let other = gate.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(false).unwrap();
            let _permit = other.enter(Duration::from_secs(1)).unwrap();
            send.send(true).unwrap();
        });
        assert!(!receive.recv_timeout(Duration::from_secs(1)).unwrap());
        drop(permit);
        assert!(receive.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
    }
    #[test]
    fn accepted_operations_keep_fifo_order_for_read_after_write_confirmation() {
        let gate = Arc::new(ApiGate::default());
        let permit = gate.enter(Duration::from_secs(1)).unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let mut workers = Vec::new();
        for index in 1..=2 {
            let other = gate.clone();
            let send = send.clone();
            workers.push(std::thread::spawn(move || {
                let _permit = other.enter(Duration::from_secs(3)).unwrap();
                send.send(index).unwrap();
            }));
            let state = gate.state.lock().unwrap();
            let (state, _) = gate
                .changed
                .wait_timeout_while(state, Duration::from_secs(2), |state| {
                    state.waiting.len() < index
                })
                .unwrap();
            assert_eq!(state.waiting.len(), index);
        }
        drop(permit);
        assert_eq!(receive.recv_timeout(Duration::from_secs(3)).unwrap(), 1);
        assert_eq!(receive.recv_timeout(Duration::from_secs(3)).unwrap(), 2);
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
