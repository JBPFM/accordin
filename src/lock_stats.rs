use std::cell::UnsafeCell;

use crate::arch::{wait_time_elapsed_ns_between, wait_time_start, wait_time_to_ns};

/// Per-thread lock scheduling context, read by BPF via bpf_probe_read_user.
#[repr(C)]
pub struct LockSchedThreadCtx {
    pub wait_ns_total: u64,
    pub wait_start_ns: u64,
    pub wait_end_ns: u64,
    pub unlock_count: u64,
}

impl LockSchedThreadCtx {
    const fn new() -> Self {
        Self {
            wait_ns_total: 0,
            wait_start_ns: 0,
            wait_end_ns: 0,
            unlock_count: 0,
        }
    }
}

thread_local! {
    static THREAD_CTX: UnsafeCell<LockSchedThreadCtx> = const { UnsafeCell::new(LockSchedThreadCtx::new()) };
}

/// Returns a pointer to the current thread's LockSchedThreadCtx.
pub fn thread_ctx() -> *mut LockSchedThreadCtx {
    THREAD_CTX.with(|ctx| ctx.get())
}

#[inline(always)]
pub fn record_wait_start() -> u64 {
    let wait_start = wait_time_start();
    unsafe {
        (*thread_ctx()).wait_start_ns = wait_time_to_ns(wait_start);
    }
    wait_start
}

#[inline(always)]
pub fn record_wait_end(wait_start: u64) {
    let wait_end = wait_time_start();
    unsafe {
        let ctx = thread_ctx();
        (*ctx).wait_ns_total += wait_time_elapsed_ns_between(wait_start, wait_end);
        (*ctx).wait_end_ns = wait_time_to_ns(wait_end);
    }
}

#[inline(always)]
pub fn record_unlock() {
    unsafe {
        (*thread_ctx()).unlock_count += 1;
    }
}
