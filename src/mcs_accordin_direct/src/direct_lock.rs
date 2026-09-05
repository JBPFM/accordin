// SPDX-License-Identifier: GPL-2.0-only

use accordin_shared::direct_runtime;

#[allow(non_camel_case_types)]
#[repr(C)]
pub struct mcs_accordin_direct_mutex {
    lock: crate::mcs::McsLockRaw,
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

    direct_runtime::ensure_registered();
    direct_runtime::lock(&mutex.lock);
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

    direct_runtime::ensure_registered();
    if direct_runtime::try_lock(&mutex.lock) {
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

    direct_runtime::unlock(&mutex.lock);
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
