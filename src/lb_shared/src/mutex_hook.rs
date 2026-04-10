use std::cell::{Cell, UnsafeCell};
use std::hint::spin_loop;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::lock_stats::{
    flush_current_thread_stats, record_hold_end, record_lock_acquired, record_thread_start,
    record_wait_end, record_wait_start,
};

pub trait MutexHookBackend {
    type LockState: Send + Sync + 'static;

    fn create_state() -> Self::LockState;
    fn lock(state: &Self::LockState);
    fn try_lock(state: &Self::LockState) -> bool;
    fn unlock(state: &Self::LockState);
}

pub trait ThreadRegistration {
    fn register_current_thread() -> bool;
    fn unregister_current_thread();
}

pub struct ThreadRegistrationGuard<R: ThreadRegistration>(PhantomData<R>);

impl<R: ThreadRegistration> Drop for ThreadRegistrationGuard<R> {
    fn drop(&mut self) {
        flush_current_thread_stats();
        R::unregister_current_thread();
    }
}

#[inline(always)]
pub fn ensure_thread_registered<R: ThreadRegistration>(
    registered: &Cell<bool>,
    guard: &UnsafeCell<Option<ThreadRegistrationGuard<R>>>,
) {
    if !registered.get() && R::register_current_thread() {
        record_thread_start();
        unsafe {
            *guard.get() = Some(ThreadRegistrationGuard(PhantomData));
        }
        registered.set(true);
    }
}

#[inline(always)]
pub fn current_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

const SENTINEL: usize = 1;

struct MutexState<B: MutexHookBackend> {
    lock: B::LockState,
    real_mutex: UnsafeCell<libc::pthread_mutex_t>,
}

unsafe impl<B: MutexHookBackend> Send for MutexState<B> {}
unsafe impl<B: MutexHookBackend> Sync for MutexState<B> {}

macro_rules! real_fn {
    ($fn_name:ident : unsafe extern "C" fn($($arg:ty),* $(,)?) -> $ret:ty = $sym:ident) => {
        #[inline(always)]
        fn $fn_name() -> unsafe extern "C" fn($($arg),*) -> $ret {
            static REAL: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

            let mut ptr = REAL.load(Ordering::Relaxed);
            if ptr.is_null() {
                ptr = unsafe {
                    libc::dlsym(
                        libc::RTLD_NEXT,
                        concat!(stringify!($sym), "\0").as_ptr() as *const libc::c_char,
                    )
                };
                assert!(
                    !ptr.is_null(),
                    concat!("dlsym failed for ", stringify!($sym))
                );
                REAL.store(ptr, Ordering::Release);
            }

            unsafe {
                std::mem::transmute::<
                    *mut std::ffi::c_void,
                    unsafe extern "C" fn($($arg),*) -> $ret,
                >(ptr)
            }
        }
    };
}

real_fn!(real_pthread_mutex_init: unsafe extern "C" fn(
    *mut libc::pthread_mutex_t,
    *const libc::pthread_mutexattr_t,
) -> libc::c_int = pthread_mutex_init);
real_fn!(real_pthread_mutex_destroy: unsafe extern "C" fn(
    *mut libc::pthread_mutex_t,
) -> libc::c_int = pthread_mutex_destroy);
real_fn!(real_pthread_mutex_lock: unsafe extern "C" fn(
    *mut libc::pthread_mutex_t,
) -> libc::c_int = pthread_mutex_lock);
real_fn!(real_pthread_mutex_unlock: unsafe extern "C" fn(
    *mut libc::pthread_mutex_t,
) -> libc::c_int = pthread_mutex_unlock);
real_fn!(real_pthread_cond_init: unsafe extern "C" fn(
    *mut libc::pthread_cond_t,
    *const libc::pthread_condattr_t,
) -> libc::c_int = pthread_cond_init);
real_fn!(real_pthread_cond_destroy: unsafe extern "C" fn(
    *mut libc::pthread_cond_t,
) -> libc::c_int = pthread_cond_destroy);
real_fn!(real_pthread_cond_signal: unsafe extern "C" fn(
    *mut libc::pthread_cond_t,
) -> libc::c_int = pthread_cond_signal);
real_fn!(real_pthread_cond_broadcast: unsafe extern "C" fn(
    *mut libc::pthread_cond_t,
) -> libc::c_int = pthread_cond_broadcast);
real_fn!(real_pthread_cond_wait: unsafe extern "C" fn(
    *mut libc::pthread_cond_t,
    *mut libc::pthread_mutex_t,
) -> libc::c_int = pthread_cond_wait);
real_fn!(real_pthread_cond_timedwait: unsafe extern "C" fn(
    *mut libc::pthread_cond_t,
    *mut libc::pthread_mutex_t,
    *const libc::timespec,
) -> libc::c_int = pthread_cond_timedwait);

#[inline(always)]
fn lock_with_stats<B: MutexHookBackend>(state: &B::LockState) {
    if B::try_lock(state) {
        record_lock_acquired();
        return;
    }

    let wait_start = record_wait_start();
    B::lock(state);
    record_wait_end(wait_start);
    record_lock_acquired();
}

#[inline(always)]
fn unlock_with_stats<B: MutexHookBackend>(state: &B::LockState) {
    record_hold_end();
    B::unlock(state);
}

#[inline(always)]
unsafe fn state_atomic(mutex: *mut libc::pthread_mutex_t) -> &'static AtomicUsize {
    unsafe { &*(mutex as *const AtomicUsize) }
}

#[inline(always)]
unsafe fn ensure_state<B: MutexHookBackend>(
    mutex: *mut libc::pthread_mutex_t,
) -> Result<*mut MutexState<B>, libc::c_int> {
    if mutex.is_null() {
        return Err(libc::EINVAL);
    }

    let val = unsafe { state_atomic(mutex) }.load(Ordering::Acquire);
    if likely(val > SENTINEL) {
        return Ok(val as *mut MutexState<B>);
    }

    unsafe { ensure_state_slow::<B>(mutex, val) }
}

#[cold]
#[inline(never)]
unsafe fn ensure_state_slow<B: MutexHookBackend>(
    mutex: *mut libc::pthread_mutex_t,
    initial: usize,
) -> Result<*mut MutexState<B>, libc::c_int> {
    let atomic = unsafe { state_atomic(mutex) };
    let mut val = initial;

    loop {
        if val > SENTINEL {
            return Ok(val as *mut MutexState<B>);
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

        let state = Box::new(MutexState::<B> {
            lock: B::create_state(),
            real_mutex: unsafe { UnsafeCell::new(std::mem::zeroed()) },
        });
        let ptr = Box::into_raw(state);

        let ret = unsafe { real_pthread_mutex_init()((*ptr).real_mutex.get(), std::ptr::null()) };
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
const fn likely(value: bool) -> bool {
    if !value {
        cold_path();
    }
    value
}

#[cold]
#[inline(never)]
const fn cold_path() {}

pub unsafe fn pthread_mutex_init_impl<B: MutexHookBackend>(
    mutex: *mut libc::pthread_mutex_t,
    attr: *const libc::pthread_mutexattr_t,
) -> libc::c_int {
    if mutex.is_null() {
        return libc::EINVAL;
    }

    let state = Box::new(MutexState::<B> {
        lock: B::create_state(),
        real_mutex: UnsafeCell::new(unsafe { std::mem::zeroed() }),
    });
    let ptr = Box::into_raw(state);

    let ret = unsafe { real_pthread_mutex_init()((*ptr).real_mutex.get(), attr) };
    if ret != 0 {
        unsafe {
            drop(Box::from_raw(ptr));
        }
        return ret;
    }

    unsafe {
        std::ptr::write_bytes(
            mutex as *mut u8,
            0,
            std::mem::size_of::<libc::pthread_mutex_t>(),
        );
        *(mutex as *mut usize) = ptr as usize;
    }
    0
}

pub unsafe fn pthread_mutex_destroy_impl<B: MutexHookBackend>(
    mutex: *mut libc::pthread_mutex_t,
) -> libc::c_int {
    if mutex.is_null() {
        return libc::EINVAL;
    }

    let atomic = unsafe { state_atomic(mutex) };
    let val = atomic.load(Ordering::Acquire);
    if val > SENTINEL {
        let ptr = val as *mut MutexState<B>;
        let ret = unsafe { real_pthread_mutex_destroy()((*ptr).real_mutex.get()) };
        if ret != 0 {
            return ret;
        }
        unsafe {
            drop(Box::from_raw(ptr));
        }
        atomic.store(0, Ordering::Release);
    }
    0
}

pub unsafe fn pthread_mutex_lock_impl<B: MutexHookBackend>(
    mutex: *mut libc::pthread_mutex_t,
) -> libc::c_int {
    let state = match unsafe { ensure_state::<B>(mutex) } {
        Ok(state) => state,
        Err(ret) => return ret,
    };

    unsafe {
        lock_with_stats::<B>(&(*state).lock);
    }
    0
}

pub unsafe fn pthread_mutex_trylock_impl<B: MutexHookBackend>(
    mutex: *mut libc::pthread_mutex_t,
) -> libc::c_int {
    let state = match unsafe { ensure_state::<B>(mutex) } {
        Ok(state) => state,
        Err(ret) => return ret,
    };

    unsafe {
        if B::try_lock(&(*state).lock) {
            record_lock_acquired();
            0
        } else {
            libc::EBUSY
        }
    }
}

pub unsafe fn pthread_mutex_unlock_impl<B: MutexHookBackend>(
    mutex: *mut libc::pthread_mutex_t,
) -> libc::c_int {
    if mutex.is_null() {
        return libc::EINVAL;
    }

    let atomic = unsafe { state_atomic(mutex) };
    let val = atomic.load(Ordering::Acquire);
    if val > SENTINEL {
        unsafe {
            unlock_with_stats::<B>(&(*(val as *mut MutexState<B>)).lock);
        }
        return 0;
    }
    libc::EINVAL
}

pub unsafe fn pthread_cond_init_impl(
    cond: *mut libc::pthread_cond_t,
    attr: *const libc::pthread_condattr_t,
) -> libc::c_int {
    unsafe { real_pthread_cond_init()(cond, attr) }
}

pub unsafe fn pthread_cond_destroy_impl(cond: *mut libc::pthread_cond_t) -> libc::c_int {
    unsafe { real_pthread_cond_destroy()(cond) }
}

pub unsafe fn pthread_cond_signal_impl(cond: *mut libc::pthread_cond_t) -> libc::c_int {
    unsafe { real_pthread_cond_signal()(cond) }
}

pub unsafe fn pthread_cond_broadcast_impl(cond: *mut libc::pthread_cond_t) -> libc::c_int {
    unsafe { real_pthread_cond_broadcast()(cond) }
}

pub unsafe fn pthread_cond_wait_impl<B: MutexHookBackend>(
    cond: *mut libc::pthread_cond_t,
    user_mutex: *mut libc::pthread_mutex_t,
) -> libc::c_int {
    let state = match unsafe { ensure_state::<B>(user_mutex) } {
        Ok(state) => state,
        Err(ret) => return ret,
    };

    unsafe {
        let real_mutex = (*state).real_mutex.get();
        real_pthread_mutex_lock()(real_mutex);
        unlock_with_stats::<B>(&(*state).lock);
        let ret = real_pthread_cond_wait()(cond, real_mutex);
        real_pthread_mutex_unlock()(real_mutex);
        lock_with_stats::<B>(&(*state).lock);
        ret
    }
}

pub unsafe fn pthread_cond_timedwait_impl<B: MutexHookBackend>(
    cond: *mut libc::pthread_cond_t,
    user_mutex: *mut libc::pthread_mutex_t,
    abstime: *const libc::timespec,
) -> libc::c_int {
    let state = match unsafe { ensure_state::<B>(user_mutex) } {
        Ok(state) => state,
        Err(ret) => return ret,
    };

    unsafe {
        let real_mutex = (*state).real_mutex.get();
        real_pthread_mutex_lock()(real_mutex);
        unlock_with_stats::<B>(&(*state).lock);
        let ret = real_pthread_cond_timedwait()(cond, real_mutex, abstime);
        real_pthread_mutex_unlock()(real_mutex);
        lock_with_stats::<B>(&(*state).lock);
        ret
    }
}

#[cfg(test)]
mod tests {
    use super::{ThreadRegistration, ThreadRegistrationGuard, ensure_thread_registered};
    use std::cell::{Cell, UnsafeCell};

    thread_local! {
        static REGISTER_CALLS: Cell<usize> = const { Cell::new(0) };
        static UNREGISTER_CALLS: Cell<usize> = const { Cell::new(0) };
        static SHOULD_REGISTER: Cell<bool> = const { Cell::new(false) };
    }

    struct TestRegistration;

    impl ThreadRegistration for TestRegistration {
        fn register_current_thread() -> bool {
            REGISTER_CALLS.with(|calls| calls.set(calls.get() + 1));
            SHOULD_REGISTER.with(|should_register| should_register.get())
        }

        fn unregister_current_thread() {
            UNREGISTER_CALLS.with(|calls| calls.set(calls.get() + 1));
        }
    }

    fn reset_registration_state(should_register: bool) {
        REGISTER_CALLS.with(|calls| calls.set(0));
        UNREGISTER_CALLS.with(|calls| calls.set(0));
        SHOULD_REGISTER.with(|flag| flag.set(should_register));
    }

    fn register_calls() -> usize {
        REGISTER_CALLS.with(|calls| calls.get())
    }

    fn unregister_calls() -> usize {
        UNREGISTER_CALLS.with(|calls| calls.get())
    }

    #[test]
    fn ensure_thread_registered_skips_flag_and_guard_when_registration_fails() {
        reset_registration_state(false);
        let registered = Cell::new(false);
        let guard = UnsafeCell::new(None::<ThreadRegistrationGuard<TestRegistration>>);

        ensure_thread_registered::<TestRegistration>(&registered, &guard);

        assert!(!registered.get());
        assert!(unsafe { (*guard.get()).is_none() });
        assert_eq!(register_calls(), 1);
        assert_eq!(unregister_calls(), 0);
    }

    #[test]
    fn ensure_thread_registered_sets_flag_and_guard_once_on_success() {
        reset_registration_state(true);
        let registered = Cell::new(false);
        let guard = UnsafeCell::new(None::<ThreadRegistrationGuard<TestRegistration>>);

        ensure_thread_registered::<TestRegistration>(&registered, &guard);
        ensure_thread_registered::<TestRegistration>(&registered, &guard);

        assert!(registered.get());
        assert!(unsafe { (*guard.get()).is_some() });
        assert_eq!(register_calls(), 1);

        unsafe {
            *guard.get() = None;
        }
        assert_eq!(unregister_calls(), 1);
    }
}

#[macro_export]
macro_rules! export_mutex_hooks {
    ($backend:ty, $registration:ty) => {
        thread_local! {
            static REGISTERED: ::std::cell::Cell<bool> = const { ::std::cell::Cell::new(false) };
            static REGISTRATION_GUARD: ::std::cell::UnsafeCell<
                Option<$crate::mutex_hook::ThreadRegistrationGuard<$registration>>,
            > = const { ::std::cell::UnsafeCell::new(None) };
        }

        #[inline(always)]
        fn ensure_registered() {
            REGISTERED.with(|registered| {
                if !registered.get() {
                    REGISTRATION_GUARD.with(|guard| {
                        $crate::mutex_hook::ensure_thread_registered::<$registration>(registered, guard)
                    });
                }
            });
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_mutex_init(
            mutex: *mut libc::pthread_mutex_t,
            attr: *const libc::pthread_mutexattr_t,
        ) -> libc::c_int {
            unsafe { $crate::mutex_hook::pthread_mutex_init_impl::<$backend>(mutex, attr) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_mutex_destroy(
            mutex: *mut libc::pthread_mutex_t,
        ) -> libc::c_int {
            unsafe { $crate::mutex_hook::pthread_mutex_destroy_impl::<$backend>(mutex) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_mutex_lock(
            mutex: *mut libc::pthread_mutex_t,
        ) -> libc::c_int {
            ensure_registered();
            unsafe { $crate::mutex_hook::pthread_mutex_lock_impl::<$backend>(mutex) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_mutex_trylock(
            mutex: *mut libc::pthread_mutex_t,
        ) -> libc::c_int {
            ensure_registered();
            unsafe { $crate::mutex_hook::pthread_mutex_trylock_impl::<$backend>(mutex) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_mutex_unlock(
            mutex: *mut libc::pthread_mutex_t,
        ) -> libc::c_int {
            unsafe { $crate::mutex_hook::pthread_mutex_unlock_impl::<$backend>(mutex) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_cond_init(
            cond: *mut libc::pthread_cond_t,
            attr: *const libc::pthread_condattr_t,
        ) -> libc::c_int {
            unsafe { $crate::mutex_hook::pthread_cond_init_impl(cond, attr) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_cond_destroy(
            cond: *mut libc::pthread_cond_t,
        ) -> libc::c_int {
            unsafe { $crate::mutex_hook::pthread_cond_destroy_impl(cond) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_cond_signal(
            cond: *mut libc::pthread_cond_t,
        ) -> libc::c_int {
            unsafe { $crate::mutex_hook::pthread_cond_signal_impl(cond) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_cond_broadcast(
            cond: *mut libc::pthread_cond_t,
        ) -> libc::c_int {
            unsafe { $crate::mutex_hook::pthread_cond_broadcast_impl(cond) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_cond_wait(
            cond: *mut libc::pthread_cond_t,
            user_mutex: *mut libc::pthread_mutex_t,
        ) -> libc::c_int {
            unsafe { $crate::mutex_hook::pthread_cond_wait_impl::<$backend>(cond, user_mutex) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pthread_cond_timedwait(
            cond: *mut libc::pthread_cond_t,
            user_mutex: *mut libc::pthread_mutex_t,
            abstime: *const libc::timespec,
        ) -> libc::c_int {
            unsafe {
                $crate::mutex_hook::pthread_cond_timedwait_impl::<$backend>(
                    cond, user_mutex, abstime,
                )
            }
        }
    };
}
