// SPDX-License-Identifier: GPL-2.0-only

//! Thread registration and acquisition brackets shared by the direct APIs.

use crate::admission;
use crate::lock_backend::LockBackend;
use libbpf_rs::{MapCore, MapFlags, MapHandle};
use std::sync::OnceLock;

/// Only the outermost acquisition asks for a CPU admission slot.
#[inline(always)]
pub fn lock<L: LockBackend>(lock: &L) {
    let outer = admission::begin();
    if lock.try_lock() {
        admission::enter(outer);
        return;
    }
    if outer {
        admission::wait();
    }
    lock.lock();
    admission::enter(outer);
}

/// A failed trylock must not open an episode or change the admission word.
#[inline(always)]
pub fn try_lock<L: LockBackend>(lock: &L) -> bool {
    if !lock.try_lock() {
        return false;
    }
    admission::enter(admission::begin());
    true
}

#[inline(always)]
pub fn unlock<L: LockBackend>(lock: &L) {
    lock.unlock();
    admission::finish();
}

static THREAD_CTX_MAP: OnceLock<MapHandle> = OnceLock::new();
pub fn set_thread_ctx_map(map: MapHandle) {
    let _ = THREAD_CTX_MAP.set(map);
}

struct ThreadRegistration(Option<u32>);

impl ThreadRegistration {
    fn new() -> Self {
        let Some(map) = THREAD_CTX_MAP.get() else {
            return Self(None);
        };
        let tid = unsafe { libc::syscall(libc::SYS_gettid) as u32 };
        let address = admission::user_word_addr() as u64;
        map.update(&tid.to_ne_bytes(), &address.to_ne_bytes(), MapFlags::ANY)
            .expect("failed to register the thread for admission");
        Self(Some(tid))
    }
}

impl Drop for ThreadRegistration {
    fn drop(&mut self) {
        if let (Some(tid), Some(map)) = (self.0, THREAD_CTX_MAP.get()) {
            let _ = map.delete(&tid.to_ne_bytes());
        }
    }
}

thread_local! {
    static REGISTRATION: ThreadRegistration = ThreadRegistration::new();
}

#[inline(always)]
pub fn ensure_registered() {
    REGISTRATION.with(|_| {});
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct TestLock(std::cell::Cell<bool>);

    impl LockBackend for TestLock {
        fn lock(&self) {
            assert!(!self.0.replace(true));
        }
        fn try_lock(&self) -> bool {
            !self.0.replace(true)
        }
        fn unlock(&self) {
            assert!(self.0.replace(false));
        }
    }

    #[test]
    fn failed_trylock_preserves_the_outer_episode() {
        admission::reset_for_test();
        let raw = TestLock::default();
        assert!(try_lock(&raw));
        assert_eq!(admission::state_for_test(), admission::HELD);
        assert!(!try_lock(&raw));
        assert_eq!(admission::state_for_test(), admission::HELD);
        unlock(&raw);
        assert_eq!(admission::state_for_test(), 0);
        lock(&raw);
        assert_eq!(admission::state_for_test(), admission::HELD);
        unlock(&raw);
    }
}
