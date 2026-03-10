mod arch;
mod epoch;
mod mcs_tas;

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::sync::atomic::{AtomicUsize, Ordering};

use epoch::with_epoch;
use mcs_tas::McsTasLockRaw;

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

/// Ensure that `mutex` has an associated `McsTasState`, allocating one if needed.
/// Handles the PTHREAD_MUTEX_INITIALIZER case (all-zero mutex) via a CAS sentinel.
unsafe fn ensure_state(mutex: *mut libc::pthread_mutex_t) -> *mut McsTasState {
    let atomic = unsafe { state_atomic(mutex) };
    loop {
        let val = atomic.load(Ordering::Acquire);

        if val > SENTINEL {
            // Already initialised.
            return val as *mut McsTasState;
        }

        if val == SENTINEL {
            // Another thread is initialising right now; spin.
            spin_loop();
            continue;
        }

        // val == 0 — try to claim the slot with the sentinel.
        if atomic
            .compare_exchange(0, SENTINEL, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Lost the race; retry from the top.
            continue;
        }

        // We won the CAS: allocate and initialise state.
        let state = Box::new(McsTasState {
            lock: McsTasLockRaw::new(),
            real_mutex: unsafe { UnsafeCell::new(std::mem::zeroed()) },
        });
        let ptr = Box::into_raw(state);

        // Initialise the internal real_mutex (used only by cond_wait).
        unsafe {
            redhook::real!(pthread_mutex_init)((*ptr).real_mutex.get(), std::ptr::null());
        }

        // Publish the pointer, replacing the sentinel.
        atomic.store(ptr as usize, Ordering::Release);
        return ptr;
    }
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

redhook::hook! {
    unsafe fn pthread_mutex_init(
        mutex: *mut libc::pthread_mutex_t,
        attr: *const libc::pthread_mutexattr_t
    ) -> libc::c_int => hooked_pthread_mutex_init {
        unsafe {
            // Allocate state immediately so we can honour the attr (we ignore it
            // for our lock, but initialise real_mutex with the same attr so that
            // cond_wait behaves correctly).
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

            // Zero out the whole mutex first (matches PTHREAD_MUTEX_INITIALIZER),
            // then write our pointer into the first word.
            std::ptr::write_bytes(mutex as *mut u8, 0, std::mem::size_of::<libc::pthread_mutex_t>());
            (*(mutex as *mut usize)) = ptr as usize;
            0
        }
    }
}

redhook::hook! {
    unsafe fn pthread_mutex_destroy(
        mutex: *mut libc::pthread_mutex_t
    ) -> libc::c_int => hooked_pthread_mutex_destroy {
        unsafe {
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
    ) -> libc::c_int => hooked_pthread_mutex_lock {
        unsafe {
            let state = ensure_state(mutex);

            // Attempt the fast path first so we can record spin_start_ns BEFORE
            // entering the slow path.  The extra try_lock() CAS has negligible
            // overhead compared to the spin cost.
            if (*state).lock.try_lock() {
                // Uncontended: no wait, just record lock_start.
                with_epoch(|ep| ep.on_lock_acquired(false));
            } else {
                // Contended: record the spin start timestamp, then block.
                with_epoch(|ep| ep.on_contention_start());
                (*state).lock.lock(); // blocks; returns true (contended)
                with_epoch(|ep| ep.on_lock_acquired(true));
            }
            0
        }
    }
}

redhook::hook! {
    unsafe fn pthread_mutex_trylock(
        mutex: *mut libc::pthread_mutex_t
    ) -> libc::c_int => hooked_pthread_mutex_trylock {
        unsafe {
            let state = ensure_state(mutex);
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
    ) -> libc::c_int => hooked_pthread_mutex_unlock {
        unsafe {
            let atomic = state_atomic(mutex);
            let val = atomic.load(Ordering::Acquire);
            if val > SENTINEL {
                // Record hold_ns before releasing so the unlock timestamp is
                // not inflated by another thread immediately re-acquiring.
                with_epoch(|ep| ep.on_lock_released());
                (*(val as *mut McsTasState)).lock.unlock();
            }
            0
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
    ) -> libc::c_int => hooked_pthread_cond_init {
        unsafe { redhook::real!(pthread_cond_init)(cond, attr) }
    }
}

redhook::hook! {
    unsafe fn pthread_cond_destroy(
        cond: *mut libc::pthread_cond_t
    ) -> libc::c_int => hooked_pthread_cond_destroy {
        unsafe { redhook::real!(pthread_cond_destroy)(cond) }
    }
}

redhook::hook! {
    unsafe fn pthread_cond_signal(
        cond: *mut libc::pthread_cond_t
    ) -> libc::c_int => hooked_pthread_cond_signal {
        unsafe { redhook::real!(pthread_cond_signal)(cond) }
    }
}

redhook::hook! {
    unsafe fn pthread_cond_broadcast(
        cond: *mut libc::pthread_cond_t
    ) -> libc::c_int => hooked_pthread_cond_broadcast {
        unsafe { redhook::real!(pthread_cond_broadcast)(cond) }
    }
}

redhook::hook! {
    unsafe fn pthread_cond_wait(
        cond: *mut libc::pthread_cond_t,
        user_mutex: *mut libc::pthread_mutex_t
    ) -> libc::c_int => hooked_pthread_cond_wait {
        unsafe {
            let state = ensure_state(user_mutex);
            let real_mu = (*state).real_mutex.get();

            // Lock the internal real_mutex before releasing the MCS lock so that
            // a racing signal cannot be lost in the window between the two.
            redhook::real!(pthread_mutex_lock)(real_mu);
            with_epoch(|ep| ep.on_lock_released()); // MCS lock released
            (*state).lock.unlock();

            with_epoch(|ep| ep.on_park_start()); // about to sleep in cond_wait
            let ret = redhook::real!(pthread_cond_wait)(cond, real_mu);
            with_epoch(|ep| ep.on_park_end());   // woken from cond_wait

            // Returns with real_mu held.
            redhook::real!(pthread_mutex_unlock)(real_mu);
            // Re-acquire MCS lock; treat as contended (we were parked).
            with_epoch(|ep| ep.on_contention_start());
            (*state).lock.lock();
            with_epoch(|ep| ep.on_lock_acquired(true));
            ret
        }
    }
}

redhook::hook! {
    unsafe fn pthread_cond_timedwait(
        cond: *mut libc::pthread_cond_t,
        user_mutex: *mut libc::pthread_mutex_t,
        abstime: *const libc::timespec
    ) -> libc::c_int => hooked_pthread_cond_timedwait {
        unsafe {
            let state = ensure_state(user_mutex);
            let real_mu = (*state).real_mutex.get();

            redhook::real!(pthread_mutex_lock)(real_mu);
            with_epoch(|ep| ep.on_lock_released());
            (*state).lock.unlock();

            with_epoch(|ep| ep.on_park_start());
            let ret = redhook::real!(pthread_cond_timedwait)(cond, real_mu, abstime);
            with_epoch(|ep| ep.on_park_end());

            redhook::real!(pthread_mutex_unlock)(real_mu);
            with_epoch(|ep| ep.on_contention_start());
            (*state).lock.lock();
            with_epoch(|ep| ep.on_lock_acquired(true));
            ret
        }
    }
}
