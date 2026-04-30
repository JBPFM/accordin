use std::cell::{Cell, UnsafeCell};
use std::hint::spin_loop;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::lock_stats::{flush_current_thread_stats, record_lock_acquired, record_thread_start};

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
}

struct CondState {
    seq: AtomicU32,
    target: AtomicU32,
}

unsafe impl<B: MutexHookBackend> Send for MutexState<B> {}
unsafe impl<B: MutexHookBackend> Sync for MutexState<B> {}

unsafe impl Send for CondState {}
unsafe impl Sync for CondState {}

#[inline(always)]
fn lock_with_stats<B: MutexHookBackend>(state: &B::LockState) {
    if B::try_lock(state) {
        // record_lock_acquired();
        return;
    }

    // let wait_start = record_wait_start();
    B::lock(state);
    // record_wait_end(wait_start);
    // record_lock_acquired();
}

#[inline(always)]
fn unlock_with_stats<B: MutexHookBackend>(state: &B::LockState) {
    // record_hold_end();
    B::unlock(state);
}

#[inline(always)]
unsafe fn state_atomic(mutex: *mut libc::pthread_mutex_t) -> &'static AtomicUsize {
    unsafe { &*(mutex as *const AtomicUsize) }
}

#[inline(always)]
unsafe fn cond_atomic(cond: *mut libc::pthread_cond_t) -> &'static AtomicUsize {
    unsafe { &*(cond as *const AtomicUsize) }
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
    });
    let ptr = Box::into_raw(state);
    let _ = attr;

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
    if cond.is_null() {
        return libc::EINVAL;
    }

    let state = Box::new(CondState {
        seq: AtomicU32::new(0),
        target: AtomicU32::new(0),
    });
    let ptr = Box::into_raw(state);
    let _ = attr;

    unsafe {
        std::ptr::write_bytes(
            cond as *mut u8,
            0,
            std::mem::size_of::<libc::pthread_cond_t>(),
        );
        *(cond as *mut usize) = ptr as usize;
    }
    0
}

pub unsafe fn pthread_cond_destroy_impl(cond: *mut libc::pthread_cond_t) -> libc::c_int {
    if cond.is_null() {
        return libc::EINVAL;
    }

    let atomic = unsafe { cond_atomic(cond) };
    let val = atomic.load(Ordering::Acquire);
    if val > SENTINEL {
        unsafe {
            drop(Box::from_raw(val as *mut CondState));
        }
        atomic.store(0, Ordering::Release);
    }
    0
}

pub unsafe fn pthread_cond_signal_impl(cond: *mut libc::pthread_cond_t) -> libc::c_int {
    let state = match unsafe { ensure_cond_state(cond) } {
        Ok(state) => state,
        Err(ret) => return ret,
    };

    unsafe {
        (*state).seq.fetch_add(1, Ordering::Release);
        futex_wake((*state).seq_ptr(), 1);
    }
    0
}

pub unsafe fn pthread_cond_broadcast_impl(cond: *mut libc::pthread_cond_t) -> libc::c_int {
    let state = match unsafe { ensure_cond_state(cond) } {
        Ok(state) => state,
        Err(ret) => return ret,
    };

    unsafe {
        let target = (*state).target.load(Ordering::Acquire);
        (*state).seq.store(target, Ordering::Release);
        futex_wake((*state).seq_ptr(), i32::MAX);
    }
    0
}

pub unsafe fn pthread_cond_wait_impl<B: MutexHookBackend>(
    cond: *mut libc::pthread_cond_t,
    user_mutex: *mut libc::pthread_mutex_t,
) -> libc::c_int {
    let mutex_state = match unsafe { ensure_state::<B>(user_mutex) } {
        Ok(state) => state,
        Err(ret) => return ret,
    };
    let cond_state = match unsafe { ensure_cond_state(cond) } {
        Ok(state) => state,
        Err(ret) => return ret,
    };

    unsafe {
        let target = (*cond_state)
            .target
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let mut seq = (*cond_state).seq.load(Ordering::Acquire);
        unlock_with_stats::<B>(&(*mutex_state).lock);

        while seq < target {
            futex_wait((*cond_state).seq_ptr(), seq, std::ptr::null());
            seq = (*cond_state).seq.load(Ordering::Acquire);
        }

        lock_with_stats::<B>(&(*mutex_state).lock);
        0
    }
}

pub unsafe fn pthread_cond_timedwait_impl<B: MutexHookBackend>(
    cond: *mut libc::pthread_cond_t,
    user_mutex: *mut libc::pthread_mutex_t,
    abstime: *const libc::timespec,
) -> libc::c_int {
    let mutex_state = match unsafe { ensure_state::<B>(user_mutex) } {
        Ok(state) => state,
        Err(ret) => return ret,
    };
    let cond_state = match unsafe { ensure_cond_state(cond) } {
        Ok(state) => state,
        Err(ret) => return ret,
    };

    unsafe {
        let target = (*cond_state)
            .target
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let mut seq = (*cond_state).seq.load(Ordering::Acquire);
        unlock_with_stats::<B>(&(*mutex_state).lock);

        let mut ret = 0;
        while seq < target {
            let timeout = match realtime_timeout_until(abstime) {
                Some(timeout) => timeout,
                None => {
                    ret = libc::ETIMEDOUT;
                    break;
                }
            };
            let rc = futex_wait((*cond_state).seq_ptr(), seq, &timeout);
            if rc == -1 && *libc::__errno_location() == libc::ETIMEDOUT {
                ret = libc::ETIMEDOUT;
                break;
            }
            seq = (*cond_state).seq.load(Ordering::Acquire);
        }

        lock_with_stats::<B>(&(*mutex_state).lock);
        ret
    }
}

impl CondState {
    #[inline(always)]
    fn seq_ptr(&self) -> *mut i32 {
        (&self.seq as *const AtomicU32).cast::<i32>() as *mut i32
    }
}

#[inline(always)]
fn futex_wait(addr: *mut i32, expected: u32, timeout: *const libc::timespec) -> libc::c_long {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            addr,
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            expected as i32,
            timeout,
        )
    }
}

#[inline(always)]
fn futex_wake(addr: *mut i32, count: i32) -> libc::c_long {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            addr,
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            count,
        )
    }
}

fn realtime_timeout_until(abstime: *const libc::timespec) -> Option<libc::timespec> {
    if abstime.is_null() {
        return None;
    }

    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) } != 0 {
        return None;
    }

    let deadline = unsafe { *abstime };
    let mut sec = deadline.tv_sec - now.tv_sec;
    let mut nsec = deadline.tv_nsec - now.tv_nsec;
    if nsec < 0 {
        sec -= 1;
        nsec += 1_000_000_000;
    }
    if sec < 0 {
        return None;
    }

    Some(libc::timespec {
        tv_sec: sec,
        tv_nsec: nsec,
    })
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
                        $crate::mutex_hook::ensure_thread_registered::<$registration>(
                            registered, guard,
                        )
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
