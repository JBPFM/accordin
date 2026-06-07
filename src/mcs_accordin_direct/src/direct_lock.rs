// SPDX-License-Identifier: GPL-2.0-only

use std::cell::{Cell, UnsafeCell};

use accordin_shared::lock_backend::LockBackend;

#[allow(non_camel_case_types)]
#[repr(C)]
pub struct mcs_accordin_direct_mutex {
    lock: crate::mcs::McsLockRaw,
}

struct ThreadCtxGuard;

impl Drop for ThreadCtxGuard {
    fn drop(&mut self) {
        accordin_shared::lock_stats::flush_current_thread_stats();
        accordin_shared::mutex_hook::unregister_current_thread();
    }
}

thread_local! {
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
    static _GUARD: UnsafeCell<Option<ThreadCtxGuard>> = const { UnsafeCell::new(None) };
}

#[inline(always)]
fn ensure_registered() {
    accordin_shared::cpu_affinity::ensure_current_thread_affinity();

    REGISTERED.with(|registered| {
        if registered.get() {
            return;
        }

        accordin_shared::lock_stats::record_thread_start();
        let _ = accordin_shared::mutex_hook::register_current_thread();
        _GUARD.with(|guard| unsafe { *guard.get() = Some(ThreadCtxGuard) });
        registered.set(true);
    });
}

#[inline(always)]
fn lock_with_stats(lock: &crate::mcs::McsLockRaw) {
    if !accordin_shared::admission::token_consumed() && lock.try_lock() {
        accordin_shared::lock_stats::record_lock_acquired();
        return;
    }

    let wait_start = accordin_shared::lock_stats::record_wait_start();
    lock.lock();
    accordin_shared::lock_stats::record_wait_end(wait_start);
    accordin_shared::lock_stats::record_lock_acquired();
}

#[inline(always)]
fn unlock_with_stats(lock: &crate::mcs::McsLockRaw) {
    let hold_end = accordin_shared::lock_stats::record_hold_end_sample();
    lock.unlock();
    accordin_shared::lock_stats::record_post_unlock(hold_end);
}

unsafe fn mutex_ref<'a>(
    mutex: *mut mcs_accordin_direct_mutex,
) -> Result<&'a mcs_accordin_direct_mutex, libc::c_int> {
    if mutex.is_null() {
        Err(libc::EINVAL)
    } else {
        Ok(unsafe { &*mutex })
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mcs_accordin_direct_mutex_create() -> *mut mcs_accordin_direct_mutex {
    Box::into_raw(Box::new(mcs_accordin_direct_mutex {
        lock: crate::mcs::McsLockRaw::new(),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_accordin_direct_mutex_destroy(
    mutex: *mut mcs_accordin_direct_mutex,
) -> libc::c_int {
    if mutex.is_null() {
        return libc::EINVAL;
    }

    drop(unsafe { Box::from_raw(mutex) });
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_accordin_direct_mutex_lock(
    mutex: *mut mcs_accordin_direct_mutex,
) -> libc::c_int {
    let mutex = match unsafe { mutex_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    ensure_registered();
    lock_with_stats(&mutex.lock);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_accordin_direct_mutex_trylock(
    mutex: *mut mcs_accordin_direct_mutex,
) -> libc::c_int {
    let mutex = match unsafe { mutex_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    ensure_registered();
    if mutex.lock.try_lock() {
        accordin_shared::lock_stats::record_lock_acquired();
        0
    } else {
        libc::EBUSY
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_accordin_direct_mutex_unlock(
    mutex: *mut mcs_accordin_direct_mutex,
) -> libc::c_int {
    let mutex = match unsafe { mutex_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    unlock_with_stats(&mutex.lock);
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn mcs_tas_accordin_direct_mutex_create() -> *mut mcs_accordin_direct_mutex {
    mcs_accordin_direct_mutex_create()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_mutex_destroy(
    mutex: *mut mcs_accordin_direct_mutex,
) -> libc::c_int {
    unsafe { mcs_accordin_direct_mutex_destroy(mutex) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_mutex_lock(
    mutex: *mut mcs_accordin_direct_mutex,
) -> libc::c_int {
    unsafe { mcs_accordin_direct_mutex_lock(mutex) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_mutex_trylock(
    mutex: *mut mcs_accordin_direct_mutex,
) -> libc::c_int {
    unsafe { mcs_accordin_direct_mutex_trylock(mutex) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_mutex_unlock(
    mutex: *mut mcs_accordin_direct_mutex,
) -> libc::c_int {
    unsafe { mcs_accordin_direct_mutex_unlock(mutex) }
}

#[cfg(test)]
mod tests {
    use super::{
        mcs_accordin_direct_mutex_create, mcs_accordin_direct_mutex_destroy,
        mcs_accordin_direct_mutex_lock, mcs_accordin_direct_mutex_trylock,
        mcs_accordin_direct_mutex_unlock, mcs_tas_accordin_direct_mutex_create,
        mcs_tas_accordin_direct_mutex_destroy, mcs_tas_accordin_direct_mutex_lock,
        mcs_tas_accordin_direct_mutex_unlock,
    };

    #[test]
    fn direct_mutex_trylock_requires_unlock_before_reacquire() {
        let mutex = mcs_accordin_direct_mutex_create();
        assert!(!mutex.is_null());

        unsafe {
            assert_eq!(mcs_accordin_direct_mutex_trylock(mutex), 0);
            assert_eq!(mcs_accordin_direct_mutex_trylock(mutex), libc::EBUSY);
            assert_eq!(mcs_accordin_direct_mutex_unlock(mutex), 0);
            assert_eq!(mcs_accordin_direct_mutex_lock(mutex), 0);
            assert_eq!(mcs_accordin_direct_mutex_unlock(mutex), 0);
            assert_eq!(mcs_accordin_direct_mutex_destroy(mutex), 0);
        }
    }

    #[test]
    fn exports_leveldb_compat_symbols() {
        let mutex = mcs_tas_accordin_direct_mutex_create();
        assert!(!mutex.is_null());

        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_lock(mutex), 0);
            assert_eq!(mcs_tas_accordin_direct_mutex_unlock(mutex), 0);
            assert_eq!(mcs_tas_accordin_direct_mutex_destroy(mutex), 0);
        }
    }
}
