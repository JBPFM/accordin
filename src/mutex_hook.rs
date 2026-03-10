// SPDX-License-Identifier: GPL-2.0-only
//
// Interpose pthread mutex/cond and back them with an MCS-TAS lock.

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::mcs_tas::McsTasLockRaw;

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
        let state = Box::new(McsTasState {
            lock: McsTasLockRaw::new(),
            real_mutex: unsafe { UnsafeCell::new(std::mem::zeroed()) },
        });
        let ptr = Box::into_raw(state);

        let ret = unsafe {
            redhook::real!(pthread_mutex_init)((*ptr).real_mutex.get(), std::ptr::null())
        };
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

redhook::hook! {
    unsafe fn pthread_mutex_init(
        mutex: *mut libc::pthread_mutex_t,
        attr: *const libc::pthread_mutexattr_t
    ) -> libc::c_int => my_pthread_mutex_init {
        unsafe {
            let state = Box::new(McsTasState {
                lock: McsTasLockRaw::new(),
                real_mutex: UnsafeCell::new(std::mem::zeroed()),
            });
            let ptr = Box::into_raw(state);

            let ret = redhook::real!(pthread_mutex_init)((*ptr).real_mutex.get(), attr);
            if ret != 0 {
                drop(Box::from_raw(ptr));
                return ret;
            }

            std::ptr::write_bytes(mutex as *mut u8, 0, std::mem::size_of::<libc::pthread_mutex_t>());
            (*(mutex as *mut usize)) = ptr as usize;
            0
        }
    }
}

redhook::hook! {
    unsafe fn pthread_mutex_destroy(
        mutex: *mut libc::pthread_mutex_t
    ) -> libc::c_int => my_pthread_mutex_destroy {
        unsafe {
            if mutex.is_null() {
                return libc::EINVAL;
            }

            let atomic = state_atomic(mutex);
            let val = atomic.load(Ordering::Acquire);
            if val > SENTINEL {
                let ptr = val as *mut McsTasState;
                redhook::real!(pthread_mutex_destroy)((*ptr).real_mutex.get());
                drop(Box::from_raw(ptr));
                atomic.store(0, Ordering::Release);
            }
            0
        }
    }
}

redhook::hook! {
    unsafe fn pthread_mutex_lock(
        mutex: *mut libc::pthread_mutex_t
    ) -> libc::c_int => my_pthread_mutex_lock {
        unsafe {
            let state = match ensure_state(mutex) {
                Ok(state) => state,
                Err(ret) => return ret,
            };
            (*state).lock.lock();
            0
        }
    }
}

redhook::hook! {
    unsafe fn pthread_mutex_trylock(
        mutex: *mut libc::pthread_mutex_t
    ) -> libc::c_int => my_pthread_mutex_trylock {
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
}

redhook::hook! {
    unsafe fn pthread_mutex_unlock(
        mutex: *mut libc::pthread_mutex_t
    ) -> libc::c_int => my_pthread_mutex_unlock {
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
}

// ---------------------------------------------------------------------------
// Condvar hooks
// ---------------------------------------------------------------------------

redhook::hook! {
    unsafe fn pthread_cond_init(
        cond: *mut libc::pthread_cond_t,
        attr: *const libc::pthread_condattr_t
    ) -> libc::c_int => my_pthread_cond_init {
        unsafe { redhook::real!(pthread_cond_init)(cond, attr) }
    }
}

redhook::hook! {
    unsafe fn pthread_cond_destroy(
        cond: *mut libc::pthread_cond_t
    ) -> libc::c_int => my_pthread_cond_destroy {
        unsafe { redhook::real!(pthread_cond_destroy)(cond) }
    }
}

redhook::hook! {
    unsafe fn pthread_cond_signal(
        cond: *mut libc::pthread_cond_t
    ) -> libc::c_int => my_pthread_cond_signal {
        unsafe { redhook::real!(pthread_cond_signal)(cond) }
    }
}

redhook::hook! {
    unsafe fn pthread_cond_broadcast(
        cond: *mut libc::pthread_cond_t
    ) -> libc::c_int => my_pthread_cond_broadcast {
        unsafe { redhook::real!(pthread_cond_broadcast)(cond) }
    }
}

redhook::hook! {
    unsafe fn pthread_cond_wait(
        cond: *mut libc::pthread_cond_t,
        user_mutex: *mut libc::pthread_mutex_t
    ) -> libc::c_int => my_pthread_cond_wait {
        unsafe {
            let state = match ensure_state(user_mutex) {
                Ok(state) => state,
                Err(ret) => return ret,
            };
            let real_mu = (*state).real_mutex.get();

            // Lock the internal real_mutex before releasing the MCS lock so that
            // a racing signal cannot be lost in the window between the two.
            redhook::real!(pthread_mutex_lock)(real_mu);
            (*state).lock.unlock();
            let ret = redhook::real!(pthread_cond_wait)(cond, real_mu);
            // Returns with real_mu held.
            redhook::real!(pthread_mutex_unlock)(real_mu);
            (*state).lock.lock();
            ret
        }
    }
}

redhook::hook! {
    unsafe fn pthread_cond_timedwait(
        cond: *mut libc::pthread_cond_t,
        user_mutex: *mut libc::pthread_mutex_t,
        abstime: *const libc::timespec
    ) -> libc::c_int => my_pthread_cond_timedwait {
        unsafe {
            let state = match ensure_state(user_mutex) {
                Ok(state) => state,
                Err(ret) => return ret,
            };
            let real_mu = (*state).real_mutex.get();

            redhook::real!(pthread_mutex_lock)(real_mu);
            (*state).lock.unlock();
            let ret = redhook::real!(pthread_cond_timedwait)(cond, real_mu, abstime);
            redhook::real!(pthread_mutex_unlock)(real_mu);
            (*state).lock.lock();
            ret
        }
    }
}
