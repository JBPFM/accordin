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
            use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

            use $crate::lock_stats::{
                record_hold_end_sample, record_lock_acquired, record_post_unlock,
                record_thread_start, record_wait_end, record_wait_start,
            };
            use $crate::mutex_hook::{MutexHookBackend, ThreadRegistration};

            type Backend = $backend;
            type Registration = $thread_registration;

            struct HookState {
                lock: <Backend as MutexHookBackend>::LockState,
                real_mutex: UnsafeCell<libc::pthread_mutex_t>,
            }

            unsafe impl Send for HookState {}
            unsafe impl Sync for HookState {}

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

            macro_rules! real {
                ($name:ident) => {{
                    #[allow(unused_unsafe)]
                    {
                        static REAL: AtomicPtr<std::ffi::c_void> =
                            AtomicPtr::new(std::ptr::null_mut());
                        let mut ptr = REAL.load(Ordering::Relaxed);
                        if ptr.is_null() {
                            ptr = unsafe {
                                libc::dlsym(
                                    libc::RTLD_NEXT,
                                    concat!(stringify!($name), "\0").as_ptr()
                                        as *const libc::c_char,
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

            #[inline(always)]
            unsafe fn state_atomic(mutex: *mut libc::pthread_mutex_t) -> &'static AtomicUsize {
                unsafe { &*(mutex as *const AtomicUsize) }
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
                        let real_destroy: unsafe extern "C" fn(
                            *mut libc::pthread_mutex_t,
                        ) -> libc::c_int = real!(pthread_mutex_destroy);
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
                attr: *const libc::pthread_condattr_t,
            ) -> libc::c_int {
                let f: unsafe extern "C" fn(
                    *mut libc::pthread_cond_t,
                    *const libc::pthread_condattr_t,
                ) -> libc::c_int = real!(pthread_cond_init);
                unsafe { f(cond, attr) }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_destroy(
                cond: *mut libc::pthread_cond_t,
            ) -> libc::c_int {
                let f: unsafe extern "C" fn(*mut libc::pthread_cond_t) -> libc::c_int =
                    real!(pthread_cond_destroy);
                unsafe { f(cond) }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_signal(
                cond: *mut libc::pthread_cond_t,
            ) -> libc::c_int {
                let f: unsafe extern "C" fn(*mut libc::pthread_cond_t) -> libc::c_int =
                    real!(pthread_cond_signal);
                unsafe { f(cond) }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_broadcast(
                cond: *mut libc::pthread_cond_t,
            ) -> libc::c_int {
                let f: unsafe extern "C" fn(*mut libc::pthread_cond_t) -> libc::c_int =
                    real!(pthread_cond_broadcast);
                unsafe { f(cond) }
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
                    let real_mu = (*state).real_mutex.get();

                    let real_lock: unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> libc::c_int =
                        real!(pthread_mutex_lock);
                    let real_unlock: unsafe extern "C" fn(
                        *mut libc::pthread_mutex_t,
                    ) -> libc::c_int = real!(pthread_mutex_unlock);
                    let real_wait: unsafe extern "C" fn(
                        *mut libc::pthread_cond_t,
                        *mut libc::pthread_mutex_t,
                    ) -> libc::c_int = real!(pthread_cond_wait);

                    real_lock(real_mu);
                    unlock_with_stats(&(*state).lock);
                    let ret = real_wait(cond, real_mu);
                    real_unlock(real_mu);
                    lock_with_stats(&(*state).lock);
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
                    let real_unlock: unsafe extern "C" fn(
                        *mut libc::pthread_mutex_t,
                    ) -> libc::c_int = real!(pthread_mutex_unlock);
                    let real_timedwait: unsafe extern "C" fn(
                        *mut libc::pthread_cond_t,
                        *mut libc::pthread_mutex_t,
                        *const libc::timespec,
                    ) -> libc::c_int = real!(pthread_cond_timedwait);

                    real_lock(real_mu);
                    unlock_with_stats(&(*state).lock);
                    let ret = real_timedwait(cond, real_mu, abstime);
                    real_unlock(real_mu);
                    lock_with_stats(&(*state).lock);
                    ret
                }
            }
        }
    };
}
