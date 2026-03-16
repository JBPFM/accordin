#[inline(always)]
pub(crate) fn prepare_thread() {
    imp::prepare_thread();
}

#[inline(always)]
pub(crate) fn on_contended_lock_enter() -> bool {
    imp::on_contended_lock_enter()
}

#[inline(always)]
pub(crate) fn on_critical_section_exit() {
    imp::on_critical_section_exit();
}

#[cfg(lb_simple_tse_available)]
mod imp {
    use std::cell::UnsafeCell;
    use std::mem::offset_of;
    use std::ptr;

    use libc::c_int;

    use crate::arch::compiler_barrier;

    const PR_RSEQ_SLICE_EXTENSION: c_int = 79;
    const PR_RSEQ_SLICE_EXTENSION_GET: libc::c_ulong = 1;
    const PR_RSEQ_SLICE_EXTENSION_SET: libc::c_ulong = 2;
    const PR_RSEQ_SLICE_EXT_ENABLE: libc::c_ulong = 0x01;

    const RSEQ_CS_FLAG_SLICE_EXT_AVAILABLE: u32 = 1 << 4;
    const RSEQ_SLICE_YIELD_SYSCALL: libc::c_long = 471;

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
        unsafe {
            let mut tp: *mut u8;
            core::arch::asm!("mov {}, fs:0", out(reg) tp, options(nostack, preserves_flags));
            tp
        }
    }

    #[inline(always)]
    fn current_thread_rseq_with_slice_ctrl() -> *mut RseqWithSliceCtrl {
        let size = unsafe { __rseq_size as usize };
        if size < RSEQ_SLICE_CTRL_END {
            return ptr::null_mut();
        }

        let thread_pointer = thread_pointer();
        if thread_pointer.is_null() {
            return ptr::null_mut();
        }

        let offset = unsafe { __rseq_offset as isize };
        unsafe { thread_pointer.offset(offset).cast::<RseqWithSliceCtrl>() }
    }

    #[inline(always)]
    fn is_unsupported_errno(err: i32) -> bool {
        err == libc::EOPNOTSUPP || err == libc::ENOSYS || err == libc::EINVAL || err == 524
    }

    fn init_thread_state() -> TimesliceThreadState {
        let mut state = TimesliceThreadState::new();
        state.initialized = true;

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
        compiler_barrier();
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
    pub(crate) fn prepare_thread() {
        with_thread_state(|_| {});
    }

    #[inline(always)]
    pub(crate) fn on_contended_lock_enter() -> bool {
        with_thread_state(|state| {
            if !state.enabled {
                return false;
            }

            compiler_barrier();
            unsafe {
                *(*state.rseq).slice_ctrl.request_ptr() = 1;
            }
            compiler_barrier();
            true
        })
    }

    #[inline(always)]
    pub(crate) fn on_critical_section_exit() {
        with_thread_state(|state| {
            if !state.enabled {
                return;
            }

            compiler_barrier();
            unsafe {
                *(*state.rseq).slice_ctrl.request_ptr() = 0;
            }
            compiler_barrier();

            if unsafe { (*state.rseq).slice_ctrl.granted() } == 0 {
                return;
            }

            unsafe {
                *libc::__errno_location() = 0;
            }
            let rc = unsafe { libc::syscall(RSEQ_SLICE_YIELD_SYSCALL) };
            if rc == 0 {
                return;
            }

            let err = unsafe { *libc::__errno_location() };
            if is_unsupported_errno(err) {
                state.enabled = false;
            }
        });
    }
}

#[cfg(not(lb_simple_tse_available))]
mod imp {
    #[inline(always)]
    pub(crate) fn prepare_thread() {}

    #[inline(always)]
    pub(crate) fn on_contended_lock_enter() -> bool {
        false
    }

    #[inline(always)]
    pub(crate) fn on_critical_section_exit() {}
}
