// SPDX-License-Identifier: GPL-2.0-only
//
// Interpose pthread mutex/cond and back them with an MCS-TAS lock.

use std::cell::{Cell, UnsafeCell};
use std::hint::spin_loop;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::mcs_tas::{McsTasLockRaw, thread_ctx};
use libbpf_rs::{MapCore, MapFlags, MapHandle};

/// BPF map handle for thread_ctx_addr_map, set by lib.rs after BPF load.
static THREAD_CTX_MAP: OnceLock<MapHandle> = OnceLock::new();

pub fn set_thread_ctx_map(map: MapHandle) {
    let _ = THREAD_CTX_MAP.set(map);
}

trait ThreadCtxMapOps {
    fn update_entry(&self, key: &[u8], value: &[u8], flags: MapFlags) -> bool;
    fn delete_entry(&self, key: &[u8]) -> bool;
}

impl ThreadCtxMapOps for MapHandle {
    fn update_entry(&self, key: &[u8], value: &[u8], flags: MapFlags) -> bool {
        self.update(key, value, flags).is_ok()
    }

    fn delete_entry(&self, key: &[u8]) -> bool {
        self.delete(key).is_ok()
    }
}

#[inline(always)]
fn current_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

fn register_thread_ctx_with_map<M>(map: &M, tid: u32, ctx_ptr: u64) -> bool
where
    M: ThreadCtxMapOps + ?Sized,
{
    map.update_entry(&tid.to_ne_bytes(), &ctx_ptr.to_ne_bytes(), MapFlags::ANY)
}

fn unregister_thread_ctx_with_map<M>(map: &M, tid: u32) -> bool
where
    M: ThreadCtxMapOps + ?Sized,
{
    map.delete_entry(&tid.to_ne_bytes())
}

/// Register the current thread's LockSchedThreadCtx pointer into the BPF map.
/// Called once per thread on first pthread_mutex_lock.
fn register_thread_ctx() {
    let Some(map) = THREAD_CTX_MAP.get() else {
        return;
    };

    let tid = current_tid();
    let ctx_ptr = thread_ctx() as u64;
    let _ = register_thread_ctx_with_map(map, tid, ctx_ptr);
}

/// Delete the current thread's entry from the BPF map.
fn unregister_thread_ctx() {
    let Some(map) = THREAD_CTX_MAP.get() else {
        return;
    };
    let _ = unregister_thread_ctx_with_map(map, current_tid());
}

/// Thread-local guard that unregisters from the BPF map on thread exit.
struct ThreadCtxGuard;

impl Drop for ThreadCtxGuard {
    fn drop(&mut self) {
        unregister_thread_ctx();
    }
}

thread_local! {
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
    static _GUARD: UnsafeCell<Option<ThreadCtxGuard>> = const { UnsafeCell::new(None) };
}

/// Ensure the current thread is registered in the BPF map (idempotent).
#[inline(always)]
fn ensure_registered() {
    REGISTERED.with(|r| {
        if !r.get() {
            register_thread_ctx();
            _GUARD.with(|g| unsafe { *g.get() = Some(ThreadCtxGuard) });
            r.set(true);
        }
    });
}

/// Resolve the real (next) symbol via `dlsym(RTLD_NEXT, name)`.
///
/// Each call site gets its own `static AtomicPtr` cache so the lookup
/// happens at most once per symbol.
macro_rules! real {
    ($name:ident) => {{
        #[allow(unused_unsafe)]
        {
            static REAL: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
            let mut ptr = REAL.load(Ordering::Relaxed);
            if ptr.is_null() {
                ptr = unsafe {
                    libc::dlsym(
                        libc::RTLD_NEXT,
                        concat!(stringify!($name), "\0").as_ptr() as *const libc::c_char,
                    )
                };
                assert!(
                    !ptr.is_null(),
                    concat!("dlsym failed for ", stringify!($name))
                );
                REAL.store(ptr, Ordering::Release);
            }
            unsafe { std::mem::transmute::<*mut std::ffi::c_void, _>(ptr) }
        }
    }};
}

/// Sentinel value stored in mutex[0..8] while McsTasState is being initialized.
const SENTINEL: usize = 1;

/// Per-mutex state stored on the heap; pointer lives in the first 8 bytes of pthread_mutex_t.
struct McsTasState {
    lock: McsTasLockRaw,
    real_mutex: UnsafeCell<libc::pthread_mutex_t>,
}

// SAFETY: The lock itself provides the necessary synchronisation.
unsafe impl Send for McsTasState {}
unsafe impl Sync for McsTasState {}

/// Read the atomic word in the first 8 bytes of `mutex`.
#[inline(always)]
unsafe fn state_atomic(mutex: *mut libc::pthread_mutex_t) -> &'static AtomicUsize {
    unsafe { &*(mutex as *const AtomicUsize) }
}

/// Fast-path: return the state pointer if already initialised.
/// Falls through to the cold slow path only on first use.
#[inline(always)]
unsafe fn ensure_state(mutex: *mut libc::pthread_mutex_t) -> Result<*mut McsTasState, libc::c_int> {
    if mutex.is_null() {
        return Err(libc::EINVAL);
    }

    let val = unsafe { state_atomic(mutex) }.load(Ordering::Acquire);
    if likely(val > SENTINEL) {
        return Ok(val as *mut McsTasState);
    }
    unsafe { ensure_state_slow(mutex, val) }
}

/// Cold path: allocate and publish a new `McsTasState`.
#[cold]
#[inline(never)]
unsafe fn ensure_state_slow(
    mutex: *mut libc::pthread_mutex_t,
    initial: usize,
) -> Result<*mut McsTasState, libc::c_int> {
    let atomic = unsafe { state_atomic(mutex) };
    let mut val = initial;
    loop {
        if val > SENTINEL {
            return Ok(val as *mut McsTasState);
        }

        if val == SENTINEL {
            // Another thread is initialising right now; spin.
            spin_loop();
            val = atomic.load(Ordering::Acquire);
            continue;
        }

        // val == 0 — try to claim the slot with the sentinel.
        if atomic
            .compare_exchange(0, SENTINEL, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            val = atomic.load(Ordering::Acquire);
            continue;
        }

        // We won the CAS: allocate and initialise state.
        let lock = match McsTasLockRaw::new() {
            Ok(lock) => lock,
            Err(ret) => {
                atomic.store(0, Ordering::Release);
                return Err(ret);
            }
        };
        let state = Box::new(McsTasState {
            lock,
            real_mutex: unsafe { UnsafeCell::new(std::mem::zeroed()) },
        });
        let ptr = Box::into_raw(state);

        let real_init: unsafe extern "C" fn(
            *mut libc::pthread_mutex_t,
            *const libc::pthread_mutexattr_t,
        ) -> libc::c_int = real!(pthread_mutex_init);
        let ret = unsafe { real_init((*ptr).real_mutex.get(), std::ptr::null()) };
        if ret != 0 {
            unsafe {
                drop(Box::from_raw(ptr));
            }
            atomic.store(0, Ordering::Release);
            return Err(ret);
        }

        atomic.store(ptr as usize, Ordering::Release);
        return Ok(ptr);
    }
}

#[inline(always)]
const fn likely(b: bool) -> bool {
    if !b {
        cold_path();
    }
    b
}

#[cold]
#[inline(never)]
const fn cold_path() {}

// ---------------------------------------------------------------------------
// Mutex hooks
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_init(
    mutex: *mut libc::pthread_mutex_t,
    attr: *const libc::pthread_mutexattr_t,
) -> libc::c_int {
    unsafe {
        let lock = match McsTasLockRaw::new() {
            Ok(lock) => lock,
            Err(ret) => return ret,
        };
        let state = Box::new(McsTasState {
            lock,
            real_mutex: UnsafeCell::new(std::mem::zeroed()),
        });
        let ptr = Box::into_raw(state);

        let real_init: unsafe extern "C" fn(
            *mut libc::pthread_mutex_t,
            *const libc::pthread_mutexattr_t,
        ) -> libc::c_int = real!(pthread_mutex_init);
        let ret = real_init((*ptr).real_mutex.get(), attr);
        if ret != 0 {
            drop(Box::from_raw(ptr));
            return ret;
        }

        std::ptr::write_bytes(
            mutex as *mut u8,
            0,
            std::mem::size_of::<libc::pthread_mutex_t>(),
        );
        (*(mutex as *mut usize)) = ptr as usize;
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_destroy(mutex: *mut libc::pthread_mutex_t) -> libc::c_int {
    unsafe {
        if mutex.is_null() {
            return libc::EINVAL;
        }

        let atomic = state_atomic(mutex);
        let val = atomic.load(Ordering::Acquire);
        if val > SENTINEL {
            let ptr = val as *mut McsTasState;
            let real_destroy: unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> libc::c_int =
                real!(pthread_mutex_destroy);
            let ret = real_destroy((*ptr).real_mutex.get());
            if ret != 0 {
                return ret;
            }
            drop(Box::from_raw(ptr));
            atomic.store(0, Ordering::Release);
        }
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_lock(mutex: *mut libc::pthread_mutex_t) -> libc::c_int {
    ensure_registered();
    unsafe {
        let state = match ensure_state(mutex) {
            Ok(state) => state,
            Err(ret) => return ret,
        };
        (*state).lock.lock();
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_trylock(mutex: *mut libc::pthread_mutex_t) -> libc::c_int {
    ensure_registered();
    unsafe {
        let state = match ensure_state(mutex) {
            Ok(state) => state,
            Err(ret) => return ret,
        };
        if (*state).lock.try_lock() {
            0
        } else {
            libc::EBUSY
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_unlock(mutex: *mut libc::pthread_mutex_t) -> libc::c_int {
    unsafe {
        if mutex.is_null() {
            return libc::EINVAL;
        }

        let atomic = state_atomic(mutex);
        let val = atomic.load(Ordering::Acquire);
        if val > SENTINEL {
            (*(val as *mut McsTasState)).lock.unlock();
            return 0;
        }
        libc::EINVAL
    }
}

// ---------------------------------------------------------------------------
// Condvar hooks
// ---------------------------------------------------------------------------

/// Generate a passthrough hook that forwards directly to the real symbol.
macro_rules! passthrough_hook {
    ($sym:ident ( $($arg:ident : $ty:ty),* $(,)? ) -> $ret:ty) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $sym($($arg: $ty),*) -> $ret {
            let f: unsafe extern "C" fn($($ty),*) -> $ret = real!($sym);
            unsafe { f($($arg),*) }
        }
    };
}

passthrough_hook!(pthread_cond_init(
    cond: *mut libc::pthread_cond_t,
    attr: *const libc::pthread_condattr_t,
) -> libc::c_int);

passthrough_hook!(pthread_cond_destroy(
    cond: *mut libc::pthread_cond_t,
) -> libc::c_int);

passthrough_hook!(pthread_cond_signal(
    cond: *mut libc::pthread_cond_t,
) -> libc::c_int);

passthrough_hook!(pthread_cond_broadcast(
    cond: *mut libc::pthread_cond_t,
) -> libc::c_int);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_wait(
    cond: *mut libc::pthread_cond_t,
    user_mutex: *mut libc::pthread_mutex_t,
) -> libc::c_int {
    unsafe {
        let state = match ensure_state(user_mutex) {
            Ok(state) => state,
            Err(ret) => return ret,
        };
        let real_mu = (*state).real_mutex.get();

        let real_lock: unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> libc::c_int =
            real!(pthread_mutex_lock);
        let real_unlock: unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> libc::c_int =
            real!(pthread_mutex_unlock);
        let real_wait: unsafe extern "C" fn(
            *mut libc::pthread_cond_t,
            *mut libc::pthread_mutex_t,
        ) -> libc::c_int = real!(pthread_cond_wait);

        // Lock the internal real_mutex before releasing the MCS lock so that
        // a racing signal cannot be lost in the window between the two.
        real_lock(real_mu);
        (*state).lock.unlock();
        let ret = real_wait(cond, real_mu);
        // Returns with real_mu held.
        real_unlock(real_mu);
        (*state).lock.lock();
        ret
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_timedwait(
    cond: *mut libc::pthread_cond_t,
    user_mutex: *mut libc::pthread_mutex_t,
    abstime: *const libc::timespec,
) -> libc::c_int {
    unsafe {
        let state = match ensure_state(user_mutex) {
            Ok(state) => state,
            Err(ret) => return ret,
        };
        let real_mu = (*state).real_mutex.get();

        let real_lock: unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> libc::c_int =
            real!(pthread_mutex_lock);
        let real_unlock: unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> libc::c_int =
            real!(pthread_mutex_unlock);
        let real_timedwait: unsafe extern "C" fn(
            *mut libc::pthread_cond_t,
            *mut libc::pthread_mutex_t,
            *const libc::timespec,
        ) -> libc::c_int = real!(pthread_cond_timedwait);

        real_lock(real_mu);
        (*state).lock.unlock();
        let ret = real_timedwait(cond, real_mu, abstime);
        real_unlock(real_mu);
        (*state).lock.lock();
        ret
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use libbpf_rs::MapFlags;

    use super::ThreadCtxMapOps;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum MapCall {
        Update {
            key: Vec<u8>,
            value: Vec<u8>,
            flags: MapFlags,
        },
        Delete {
            key: Vec<u8>,
        },
    }

    #[derive(Default)]
    struct RecordingMap {
        calls: RefCell<Vec<MapCall>>,
    }

    impl ThreadCtxMapOps for RecordingMap {
        fn update_entry(&self, key: &[u8], value: &[u8], flags: MapFlags) -> bool {
            self.calls.borrow_mut().push(MapCall::Update {
                key: key.to_vec(),
                value: value.to_vec(),
                flags,
            });
            true
        }

        fn delete_entry(&self, key: &[u8]) -> bool {
            self.calls
                .borrow_mut()
                .push(MapCall::Delete { key: key.to_vec() });
            true
        }
    }

    impl RecordingMap {
        fn calls(&self) -> Vec<MapCall> {
            self.calls.borrow().clone()
        }
    }

    #[test]
    fn register_thread_ctx_uses_map_helper_update() {
        let map = RecordingMap::default();

        assert!(super::register_thread_ctx_with_map(
            &map,
            7,
            0x1122_3344_5566_7788,
        ));

        assert_eq!(
            map.calls(),
            vec![MapCall::Update {
                key: 7u32.to_ne_bytes().to_vec(),
                value: 0x1122_3344_5566_7788u64.to_ne_bytes().to_vec(),
                flags: MapFlags::ANY,
            }]
        );
    }

    #[test]
    fn unregister_thread_ctx_uses_map_helper_delete() {
        let map = RecordingMap::default();

        assert!(super::unregister_thread_ctx_with_map(&map, 11));

        assert_eq!(
            map.calls(),
            vec![MapCall::Delete {
                key: 11u32.to_ne_bytes().to_vec(),
            }]
        );
    }
}
