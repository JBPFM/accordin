// SPDX-License-Identifier: GPL-2.0-only

pub trait MutexHookBackend {
    type LockState;

    fn create_state() -> Self::LockState;
    fn lock(state: &Self::LockState);
    fn try_lock(state: &Self::LockState) -> bool;
    fn unlock(state: &Self::LockState);
}

pub trait ThreadRegistration {
    fn register_current_thread() -> bool;
    fn unregister_current_thread();
}

#[inline(always)]
pub fn current_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[macro_export]
macro_rules! export_mutex_hooks {
    ($backend:ty, $thread_registration:ty) => {
        mod exported_mutex_hooks {
            use std::cell::{Cell, UnsafeCell};
            use std::hint::spin_loop;
            use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

            use $crate::lock_stats::{
                record_hold_end_sample, record_lock_acquired, record_post_unlock,
                record_thread_start, record_wait_end, record_wait_start,
            };
            use $crate::mutex_hook::{MutexHookBackend, ThreadRegistration};

            type Backend = $backend;
            type Registration = $thread_registration;

            struct HookState {
                lock: <Backend as MutexHookBackend>::LockState,
            }

            unsafe impl Send for HookState {}
            unsafe impl Sync for HookState {}

            struct CondState {
                seq: AtomicU32,
                target: AtomicU32,
            }

            unsafe impl Send for CondState {}
            unsafe impl Sync for CondState {}

            struct ThreadCtxGuard;

            impl Drop for ThreadCtxGuard {
                fn drop(&mut self) {
                    $crate::lock_stats::flush_current_thread_stats();
                    Registration::unregister_current_thread();
                }
            }

            thread_local! {
                static REGISTERED: Cell<bool> = const { Cell::new(false) };
                static _GUARD: UnsafeCell<Option<ThreadCtxGuard>> = const { UnsafeCell::new(None) };
            }

            #[inline(always)]
            fn ensure_registered() {
                $crate::cpu_affinity::ensure_current_thread_affinity();

                REGISTERED.with(|registered| {
                    if !registered.get() {
                        record_thread_start();
                        let _ = Registration::register_current_thread();
                        _GUARD.with(|guard| unsafe { *guard.get() = Some(ThreadCtxGuard) });
                        registered.set(true);
                    }
                });
            }

            #[inline(always)]
            fn lock_with_stats(lock: &<Backend as MutexHookBackend>::LockState) {
                if Backend::try_lock(lock) {
                    record_lock_acquired();
                    return;
                }
                let wait_start = record_wait_start();
                Backend::lock(lock);
                record_wait_end(wait_start);
                record_lock_acquired();
            }

            #[inline(always)]
            fn unlock_with_stats(lock: &<Backend as MutexHookBackend>::LockState) {
                let hold_end = record_hold_end_sample();
                Backend::unlock(lock);
                record_post_unlock(hold_end);
            }

            const SENTINEL: usize = 1;
            const FUTEX_WAIT_PRIVATE: libc::c_int = 128;
            const FUTEX_WAKE_PRIVATE: libc::c_int = 129;
            const FUTEX_WAIT_BITSET_PRIVATE_REALTIME: libc::c_int = 9 | 128 | 256;
            const FUTEX_BITSET_MATCH_ANY: libc::c_uint = libc::c_uint::MAX;

            #[inline(always)]
            unsafe fn state_atomic(mutex: *mut libc::pthread_mutex_t) -> &'static AtomicUsize {
                unsafe { &*(mutex as *const AtomicUsize) }
            }

            #[inline(always)]
            unsafe fn cond_atomic(cond: *mut libc::pthread_cond_t) -> &'static AtomicUsize {
                unsafe { &*(cond as *const AtomicUsize) }
            }

            #[inline(always)]
            unsafe fn ensure_state(
                mutex: *mut libc::pthread_mutex_t,
            ) -> Result<*mut HookState, libc::c_int> {
                if mutex.is_null() {
                    return Err(libc::EINVAL);
                }

                let val = unsafe { state_atomic(mutex) }.load(Ordering::Acquire);
                if likely(val > SENTINEL) {
                    return Ok(val as *mut HookState);
                }
                unsafe { ensure_state_slow(mutex, val) }
            }

            #[cold]
            #[inline(never)]
            unsafe fn ensure_state_slow(
                mutex: *mut libc::pthread_mutex_t,
                initial: usize,
            ) -> Result<*mut HookState, libc::c_int> {
                let atomic = unsafe { state_atomic(mutex) };
                let mut val = initial;
                loop {
                    if val > SENTINEL {
                        return Ok(val as *mut HookState);
                    }

                    if val == SENTINEL {
                        spin_loop();
                        val = atomic.load(Ordering::Acquire);
                        continue;
                    }

                    if atomic
                        .compare_exchange(0, SENTINEL, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        val = atomic.load(Ordering::Acquire);
                        continue;
                    }

                    let state = Box::new(HookState {
                        lock: Backend::create_state(),
                    });
                    let ptr = Box::into_raw(state);

                    atomic.store(ptr as usize, Ordering::Release);
                    return Ok(ptr);
                }
            }

            #[inline(always)]
            unsafe fn ensure_cond_state(
                cond: *mut libc::pthread_cond_t,
            ) -> Result<*mut CondState, libc::c_int> {
                if cond.is_null() {
                    return Err(libc::EINVAL);
                }

                let val = unsafe { cond_atomic(cond) }.load(Ordering::Acquire);
                if likely(val > SENTINEL) {
                    return Ok(val as *mut CondState);
                }
                unsafe { ensure_cond_state_slow(cond, val) }
            }

            #[cold]
            #[inline(never)]
            unsafe fn ensure_cond_state_slow(
                cond: *mut libc::pthread_cond_t,
                initial: usize,
            ) -> Result<*mut CondState, libc::c_int> {
                let atomic = unsafe { cond_atomic(cond) };
                let mut val = initial;
                loop {
                    if val > SENTINEL {
                        return Ok(val as *mut CondState);
                    }

                    if val == SENTINEL {
                        spin_loop();
                        val = atomic.load(Ordering::Acquire);
                        continue;
                    }

                    if atomic
                        .compare_exchange(0, SENTINEL, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        val = atomic.load(Ordering::Acquire);
                        continue;
                    }

                    let state = Box::new(CondState {
                        seq: AtomicU32::new(0),
                        target: AtomicU32::new(0),
                    });
                    let ptr = Box::into_raw(state);
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

            #[inline(always)]
            unsafe fn futex_wait(addr: *const AtomicU32, expected: u32) -> libc::c_long {
                unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        addr as *const u32,
                        FUTEX_WAIT_PRIVATE,
                        expected,
                        std::ptr::null::<libc::timespec>(),
                    )
                }
            }

            #[inline(always)]
            unsafe fn futex_wait_until_realtime(
                addr: *const AtomicU32,
                expected: u32,
                abstime: *const libc::timespec,
            ) -> libc::c_long {
                unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        addr as *const u32,
                        FUTEX_WAIT_BITSET_PRIVATE_REALTIME,
                        expected,
                        abstime,
                        std::ptr::null::<libc::c_void>(),
                        FUTEX_BITSET_MATCH_ANY,
                    )
                }
            }

            #[inline(always)]
            unsafe fn futex_wake(addr: *const AtomicU32, count: libc::c_int) -> libc::c_long {
                unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        addr as *const u32,
                        FUTEX_WAKE_PRIVATE,
                        count,
                    )
                }
            }

            #[inline(always)]
            fn wait_should_continue(target: u32, seq: u32) -> bool {
                target.wrapping_sub(seq) < (u32::MAX / 2) && target != seq
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_mutex_init(
                mutex: *mut libc::pthread_mutex_t,
                attr: *const libc::pthread_mutexattr_t,
            ) -> libc::c_int {
                unsafe {
                    if mutex.is_null() {
                        return libc::EINVAL;
                    }

                    let state = Box::new(HookState {
                        lock: Backend::create_state(),
                    });
                    let ptr = Box::into_raw(state);

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
            pub unsafe extern "C" fn pthread_mutex_destroy(
                mutex: *mut libc::pthread_mutex_t,
            ) -> libc::c_int {
                unsafe {
                    if mutex.is_null() {
                        return libc::EINVAL;
                    }

                    let atomic = state_atomic(mutex);
                    let val = atomic.load(Ordering::Acquire);
                    if val > SENTINEL {
                        let ptr = val as *mut HookState;
                        drop(Box::from_raw(ptr));
                        atomic.store(0, Ordering::Release);
                    }
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_mutex_lock(
                mutex: *mut libc::pthread_mutex_t,
            ) -> libc::c_int {
                ensure_registered();
                unsafe {
                    let state = match ensure_state(mutex) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    lock_with_stats(&(*state).lock);
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_mutex_trylock(
                mutex: *mut libc::pthread_mutex_t,
            ) -> libc::c_int {
                ensure_registered();
                unsafe {
                    let state = match ensure_state(mutex) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    if Backend::try_lock(&(*state).lock) {
                        record_lock_acquired();
                        0
                    } else {
                        libc::EBUSY
                    }
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_mutex_unlock(
                mutex: *mut libc::pthread_mutex_t,
            ) -> libc::c_int {
                unsafe {
                    if mutex.is_null() {
                        return libc::EINVAL;
                    }

                    let atomic = state_atomic(mutex);
                    let val = atomic.load(Ordering::Acquire);
                    if val > SENTINEL {
                        unlock_with_stats(&(*(val as *mut HookState)).lock);
                        return 0;
                    }
                    libc::EINVAL
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_init(
                cond: *mut libc::pthread_cond_t,
                _attr: *const libc::pthread_condattr_t,
            ) -> libc::c_int {
                unsafe {
                    if cond.is_null() {
                        return libc::EINVAL;
                    }

                    let state = Box::new(CondState {
                        seq: AtomicU32::new(0),
                        target: AtomicU32::new(0),
                    });
                    let ptr = Box::into_raw(state);
                    std::ptr::write_bytes(
                        cond as *mut u8,
                        0,
                        std::mem::size_of::<libc::pthread_cond_t>(),
                    );
                    (*(cond as *mut usize)) = ptr as usize;
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_destroy(
                cond: *mut libc::pthread_cond_t,
            ) -> libc::c_int {
                unsafe {
                    if cond.is_null() {
                        return libc::EINVAL;
                    }

                    let atomic = cond_atomic(cond);
                    let val = atomic.load(Ordering::Acquire);
                    if val > SENTINEL {
                        drop(Box::from_raw(val as *mut CondState));
                        atomic.store(0, Ordering::Release);
                    }
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_signal(
                cond: *mut libc::pthread_cond_t,
            ) -> libc::c_int {
                unsafe {
                    let cond_state = match ensure_cond_state(cond) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    (*cond_state).seq.fetch_add(1, Ordering::Release);
                    futex_wake(&(*cond_state).seq, 1);
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_broadcast(
                cond: *mut libc::pthread_cond_t,
            ) -> libc::c_int {
                unsafe {
                    let cond_state = match ensure_cond_state(cond) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    let target = (*cond_state).target.load(Ordering::Acquire);
                    (*cond_state).seq.store(target, Ordering::Release);
                    futex_wake(&(*cond_state).seq, libc::c_int::MAX);
                    0
                }
            }

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
                    let cond_state = match ensure_cond_state(cond) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    let target = (*cond_state).target.fetch_add(1, Ordering::Relaxed) + 1;
                    let mut seq = (*cond_state).seq.load(Ordering::Acquire);
                    unlock_with_stats(&(*state).lock);
                    while wait_should_continue(target, seq) {
                        futex_wait(&(*cond_state).seq, seq);
                        seq = (*cond_state).seq.load(Ordering::Acquire);
                    }
                    lock_with_stats(&(*state).lock);
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_timedwait(
                cond: *mut libc::pthread_cond_t,
                user_mutex: *mut libc::pthread_mutex_t,
                abstime: *const libc::timespec,
            ) -> libc::c_int {
                unsafe {
                    if abstime.is_null() {
                        return libc::EINVAL;
                    }

                    let state = match ensure_state(user_mutex) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    let cond_state = match ensure_cond_state(cond) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    let target = (*cond_state).target.fetch_add(1, Ordering::Relaxed) + 1;
                    let mut seq = (*cond_state).seq.load(Ordering::Acquire);
                    let mut ret = 0;
                    unlock_with_stats(&(*state).lock);
                    while wait_should_continue(target, seq) {
                        if futex_wait_until_realtime(&(*cond_state).seq, seq, abstime) != 0 {
                            let errno = *libc::__errno_location();
                            if errno == libc::ETIMEDOUT {
                                ret = libc::ETIMEDOUT;
                                break;
                            }
                        }
                        seq = (*cond_state).seq.load(Ordering::Acquire);
                    }
                    lock_with_stats(&(*state).lock);
                    ret
                }
            }
        }
    };
}
