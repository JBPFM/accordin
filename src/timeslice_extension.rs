use std::mem::offset_of;
use std::ptr;
use std::sync::atomic::{Ordering, compiler_fence};

use libc::c_int;

#[inline(always)]
pub(crate) fn on_mcs_spin_start() -> bool {
    imp::request_extension()
}

#[inline(always)]
pub(crate) fn clear_extension_request() {
    imp::clear_extension();
}

#[inline(always)]
pub(crate) fn on_mcs_spin_preempted() {
    imp::clear_extension();
    if !imp::yield_extended_slice() {
        std::thread::yield_now();
    }
}

#[inline(always)]
pub(crate) fn grant_was_cleared_by_kernel() -> bool {
    imp::grant_was_cleared_by_kernel()
}

#[inline(always)]
pub(crate) fn on_critical_section_exit() {
    imp::clear_extension();
    let _ = imp::yield_extended_slice();
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod imp {
    use std::cell::UnsafeCell;

    use super::*;

    const PR_RSEQ_SLICE_EXTENSION: c_int = 79;
    const PR_RSEQ_SLICE_EXTENSION_GET: libc::c_ulong = 1;
    const PR_RSEQ_SLICE_EXTENSION_SET: libc::c_ulong = 2;
    const PR_RSEQ_SLICE_EXT_ENABLE: libc::c_ulong = 0x01;

    const RSEQ_CS_FLAG_SLICE_EXT_AVAILABLE: u32 = 1 << 4;

    #[cfg(target_arch = "x86_64")]
    const RSEQ_SLICE_YIELD_SYSCALL: libc::c_long = 471;
    #[cfg(not(target_arch = "x86_64"))]
    const RSEQ_SLICE_YIELD_SYSCALL: libc::c_long = -1;

    #[repr(C)]
    struct RseqSliceCtrl {
        all: u32,
    }

    impl RseqSliceCtrl {
        #[inline(always)]
        fn request_ptr(&self) -> *mut u8 {
            ptr::addr_of!(self.all).cast::<u8>() as *mut u8
        }

        #[inline(always)]
        fn request(&self) -> u8 {
            (self.all & 0xff) as u8
        }

        #[inline(always)]
        fn granted(&self) -> u8 {
            ((self.all >> 8) & 0xff) as u8
        }
    }

    #[repr(C)]
    struct RseqWithSliceCtrl {
        cpu_id_start: u32,
        cpu_id: u32,
        rseq_cs: u64,
        flags: u32,
        node_id: u32,
        mm_cid: u32,
        slice_ctrl: RseqSliceCtrl,
    }

    const _: () = {
        assert!(offset_of!(RseqWithSliceCtrl, slice_ctrl) == 28);
    };

    const RSEQ_SLICE_CTRL_END: usize =
        offset_of!(RseqWithSliceCtrl, slice_ctrl) + std::mem::size_of::<RseqSliceCtrl>();

    unsafe extern "C" {
        static __rseq_size: libc::c_uint;
        static __rseq_offset: libc::ptrdiff_t;
    }

    #[derive(Clone, Copy)]
    struct TimesliceThreadState {
        initialized: bool,
        enabled: bool,
        rseq: *mut RseqWithSliceCtrl,
    }

    impl TimesliceThreadState {
        const fn new() -> Self {
            Self {
                initialized: false,
                enabled: false,
                rseq: ptr::null_mut(),
            }
        }
    }

    thread_local! {
        static THREAD_STATE: UnsafeCell<TimesliceThreadState> =
            const { UnsafeCell::new(TimesliceThreadState::new()) };
    }

    #[inline(always)]
    fn thread_pointer() -> *mut u8 {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let mut tp: *mut u8;
            core::arch::asm!("mov {}, fs:0", out(reg) tp, options(nostack, preserves_flags));
            tp
        }

        #[cfg(target_arch = "x86")]
        unsafe {
            let mut tp: *mut u8;
            core::arch::asm!("mov {}, gs:0", out(reg) tp, options(nostack, preserves_flags));
            tp
        }

        #[cfg(target_arch = "aarch64")]
        unsafe {
            let mut tp: *mut u8;
            core::arch::asm!("mrs {}, tpidr_el0", out(reg) tp, options(nostack, preserves_flags));
            tp
        }

        #[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")))]
        {
            ptr::null_mut()
        }
    }

    #[inline(always)]
    fn current_thread_rseq_with_slice_ctrl() -> *mut RseqWithSliceCtrl {
        let size = unsafe { __rseq_size as usize };
        if size < RSEQ_SLICE_CTRL_END {
            return ptr::null_mut();
        }

        let tp = thread_pointer();
        if tp.is_null() {
            return ptr::null_mut();
        }

        let offset = unsafe { __rseq_offset };
        unsafe { tp.offset(offset).cast::<RseqWithSliceCtrl>() }
    }

    #[inline(always)]
    fn is_unsupported_errno(err: i32) -> bool {
        err == libc::EOPNOTSUPP || err == libc::ENOSYS || err == libc::EINVAL || err == 524
    }

    fn init_thread_state() -> TimesliceThreadState {
        let mut state = TimesliceThreadState::new();
        state.initialized = true;

        if RSEQ_SLICE_YIELD_SYSCALL == -1 {
            return state;
        }

        if unsafe { __rseq_size } == 0 {
            return state;
        }

        state.rseq = current_thread_rseq_with_slice_ctrl();
        if state.rseq.is_null() {
            return state;
        }

        let flags = unsafe { (*state.rseq).flags };
        if (flags & RSEQ_CS_FLAG_SLICE_EXT_AVAILABLE) == 0 {
            return state;
        }

        unsafe {
            *libc::__errno_location() = 0;
        }
        let current = unsafe {
            libc::prctl(
                PR_RSEQ_SLICE_EXTENSION,
                PR_RSEQ_SLICE_EXTENSION_GET,
                0,
                0,
                0,
            )
        };
        if current == -1 {
            return state;
        }

        if (current as libc::c_ulong & PR_RSEQ_SLICE_EXT_ENABLE) == 0 {
            unsafe {
                *libc::__errno_location() = 0;
            }
            let rc = unsafe {
                libc::prctl(
                    PR_RSEQ_SLICE_EXTENSION,
                    PR_RSEQ_SLICE_EXTENSION_SET,
                    PR_RSEQ_SLICE_EXT_ENABLE,
                    0,
                    0,
                )
            };
            if rc == -1 {
                return state;
            }
        }

        unsafe {
            (*state.rseq).slice_ctrl.all = 0;
        }
        compiler_fence(Ordering::SeqCst);
        state.enabled = true;
        state
    }

    #[inline(always)]
    fn with_thread_state<R>(f: impl FnOnce(&mut TimesliceThreadState) -> R) -> R {
        THREAD_STATE.with(|state| {
            let state = unsafe { &mut *state.get() };
            if !state.initialized {
                *state = init_thread_state();
            }
            f(state)
        })
    }

    #[inline(always)]
    fn with_enabled_thread_state<R>(f: impl FnOnce(&mut TimesliceThreadState) -> R) -> Option<R> {
        with_thread_state(|state| {
            if !state.enabled {
                return None;
            }
            Some(f(state))
        })
    }

    #[inline(always)]
    fn grant_is_active(state: &TimesliceThreadState) -> bool {
        unsafe { (*state.rseq).slice_ctrl.granted() != 0 }
    }

    #[inline(always)]
    fn grant_was_cleared_by_kernel_for_state(state: &TimesliceThreadState) -> bool {
        let slice_ctrl = unsafe { &(*state.rseq).slice_ctrl };
        slice_ctrl.request() == 0 && slice_ctrl.granted() == 0
    }

    #[inline(always)]
    pub(super) fn request_extension() -> bool {
        with_enabled_thread_state(|state| {
            // Keep the documented request=1; barrier(); protected-region ordering.
            unsafe {
                *(*state.rseq).slice_ctrl.request_ptr() = 1;
            }
            compiler_fence(Ordering::SeqCst);
            true
        })
        .unwrap_or(false)
    }

    #[inline(always)]
    pub(super) fn grant_was_cleared_by_kernel() -> bool {
        with_enabled_thread_state(|state| grant_was_cleared_by_kernel_for_state(state))
            .unwrap_or(false)
    }

    #[inline(always)]
    pub(super) fn clear_extension() {
        let _ = with_enabled_thread_state(|state| {
            // Keep the documented protected-region; barrier(); request=0 ordering.
            compiler_fence(Ordering::SeqCst);
            unsafe {
                *(*state.rseq).slice_ctrl.request_ptr() = 0;
            }
        });
    }

    #[inline(always)]
    pub(super) fn yield_extended_slice() -> bool {
        with_enabled_thread_state(|state| {
            if !grant_is_active(state) {
                return false;
            }

            unsafe {
                *libc::__errno_location() = 0;
            }
            let rc = unsafe { libc::syscall(RSEQ_SLICE_YIELD_SYSCALL) };
            if rc == 0 {
                return true;
            }

            let err = unsafe { *libc::__errno_location() };
            if is_unsupported_errno(err) {
                state.enabled = false;
            }
            false
        })
        .unwrap_or(false)
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
mod imp {
    #[inline(always)]
    pub(super) fn request_extension() -> bool {
        false
    }

    #[inline(always)]
    pub(super) fn grant_was_cleared_by_kernel() -> bool {
        false
    }

    #[inline(always)]
    pub(super) fn clear_extension() {}

    #[inline(always)]
    pub(super) fn yield_extended_slice() -> bool {
        false
    }
}
