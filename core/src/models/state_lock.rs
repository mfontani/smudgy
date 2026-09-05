//! In-process, thread-reentrant locks for persisted model domains.
//!
//! Every model that reads or writes a server's files does so under one named lock per server
//! (`"package-state:<server>"`, `"automation-state:<server>"`, and so on). The lock is reentrant
//! on its owning thread, so a public model function can freely call another public model
//! function for the same server without a `_locked` variant. Other threads block until the
//! outermost guard on the owning thread drops.
//!
//! The locks serialize the UI thread, background tasks, and each session's script-engine thread
//! within this process. Two Smudgy processes sharing one data directory are not a supported
//! deployment; every file write is still an atomic replacement, so the worst case there is the
//! last writer winning for one file.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::ThreadId;

#[derive(Default)]
struct LockState {
    owner: Option<ThreadId>,
    depth: usize,
}

#[derive(Default)]
struct NamedLock {
    state: Mutex<LockState>,
    available: Condvar,
}

fn registry() -> &'static Mutex<HashMap<String, Weak<NamedLock>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Weak<NamedLock>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A held lock. It is intentionally not `Send`: recursion ownership is thread-based.
pub(crate) struct StateLockGuard {
    lock: Arc<NamedLock>,
    _not_send: PhantomData<Rc<()>>,
}

/// Acquires the named lock, blocking other threads.
///
/// Acquiring the same name again on the current thread only increments its recursion depth; the
/// outermost guard releases the lock when dropped.
pub(crate) fn acquire(name: &str) -> StateLockGuard {
    let lock = {
        let mut locks = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, weak| weak.strong_count() > 0);
        if let Some(lock) = locks.get(name).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(NamedLock::default());
            locks.insert(name.to_string(), Arc::downgrade(&lock));
            lock
        }
    };

    let current = std::thread::current().id();
    let mut state = lock
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        match state.owner {
            Some(owner) if owner == current => {
                state.depth += 1;
                break;
            }
            Some(_) => {
                state = lock
                    .available
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            None => {
                state.owner = Some(current);
                state.depth = 1;
                break;
            }
        }
    }
    drop(state);
    StateLockGuard {
        lock,
        _not_send: PhantomData,
    }
}

impl Drop for StateLockGuard {
    fn drop(&mut self) {
        let mut state = self
            .lock
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert_eq!(state.owner, Some(std::thread::current().id()));
        debug_assert!(state.depth > 0);
        state.depth -= 1;
        if state.depth == 0 {
            state.owner = None;
            self.lock.available.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn same_thread_acquisition_is_reentrant() {
        let first = acquire("state-lock-test-reentrant");
        let second = acquire("state-lock-test-reentrant");
        drop(second);
        drop(first);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _guard = acquire("state-lock-test-reentrant");
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn another_thread_waits_until_the_outermost_guard_drops() {
        let first = acquire("state-lock-test-contention");
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _guard = acquire("state-lock-test-contention");
            acquired_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn different_names_do_not_contend() {
        let _first = acquire("state-lock-test-a");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _guard = acquire("state-lock-test-b");
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}
