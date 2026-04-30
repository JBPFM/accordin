use std::cell::UnsafeCell;
use std::mem::offset_of;
use std::ptr;
use std::sync::atomic::{Ordering, compiler_fence};

use libc::c_int;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimesliceExtensionMode {
    Off,
    Auto,
    Require,
}

impl TimesliceExtensionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Require => "require",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TimesliceExtensionStatus {
    pub enabled: bool,
    pub reason: Option<&'static str>,
    pub error_number: i32,
}

#[inline(always)]
fn compiler_barrier() {
    compiler_fence(Ordering::SeqCst);
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod imp {
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
    pub struct RseqSliceCtrl {
        pub all: u32,
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
    pub struct RseqWithSliceCtrl {
        pub cpu_id_start: u32,
        pub cpu_id: u32,
        pub rseq_cs: u64,
        pub flags: u32,
        pub node_id: u32,
        pub mm_cid: u32,
        pub slice_ctrl: RseqSliceCtrl,
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
    pub struct TimesliceThreadState {
        pub initialized: bool,
        pub enabled: bool,
        pub error_number: i32,
        pub reason: Option<&'static str>,
        pub rseq: *mut RseqWithSliceCtrl,
    }

    impl TimesliceThreadState {
        pub const fn new() -> Self {
            Self {
                initialized: false,
                enabled: false,
                error_number: 0,
                reason: None,
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

        let thread_pointer = thread_pointer();
        if thread_pointer.is_null() {
            return ptr::null_mut();
        }

        let offset = unsafe { __rseq_offset as isize };
        unsafe { thread_pointer.offset(offset).cast::<RseqWithSliceCtrl>() }
    }

    fn is_unsupported_errno(err: i32) -> bool {
        err == libc::EOPNOTSUPP || err == libc::ENOSYS || err == libc::EINVAL || err == 524
    }

    fn init_timeslice_thread_state() -> TimesliceThreadState {
        let mut state = TimesliceThreadState::new();
        state.initialized = true;

        if RSEQ_SLICE_YIELD_SYSCALL == -1 {
            state.reason = Some("user-space headers do not expose rseq_slice_yield");
            return state;
        }

        if unsafe { __rseq_size } == 0 {
            state.reason = Some("glibc did not register rseq for this thread");
            return state;
        }

        state.rseq = current_thread_rseq_with_slice_ctrl();
        if state.rseq.is_null() {
            state.reason =
                Some("glibc rseq area is too small to expose slice_ctrl (need 32 bytes)");
            return state;
        }

        let flags = unsafe { (*state.rseq).flags };
        if (flags & RSEQ_CS_FLAG_SLICE_EXT_AVAILABLE) == 0 {
            state.reason = Some("kernel did not advertise rseq slice extension");
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
            state.error_number = unsafe { *libc::__errno_location() };
            state.reason = Some("prctl(PR_RSEQ_SLICE_EXTENSION_GET) failed");
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
                state.error_number = unsafe { *libc::__errno_location() };
                state.reason = Some("prctl(PR_RSEQ_SLICE_EXTENSION_SET) failed");
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

    fn with_thread_state<R>(f: impl FnOnce(&mut TimesliceThreadState) -> R) -> R {
        THREAD_STATE.with(|state| {
            let state = unsafe { &mut *state.get() };
            if !state.initialized {
                *state = init_timeslice_thread_state();
            }
            f(state)
        })
    }

    fn current_thread_state_snapshot() -> TimesliceThreadState {
        with_thread_state(|state| *state)
    }

    #[cold]
    #[track_caller]
    fn abort_timeslice_extension(context: &str, reason: &str, error_number: i32) -> ! {
        if error_number != 0 {
            eprintln!(
                "timeslice extension {} failed: {} (errno={}, {})",
                context,
                reason,
                error_number,
                std::io::Error::from_raw_os_error(error_number)
            );
        } else {
            eprintln!("timeslice extension {} failed: {}", context, reason);
        }
        std::process::abort();
    }

    pub struct CriticalSectionTimesliceExtension {
        mode: TimesliceExtensionMode,
    }

    impl CriticalSectionTimesliceExtension {
        pub const fn new(mode: TimesliceExtensionMode) -> Self {
            Self { mode }
        }

        pub fn prepare_thread(&self) {
            if self.mode == TimesliceExtensionMode::Off {
                return;
            }

            let snapshot = current_thread_state_snapshot();
            if !snapshot.enabled && self.mode == TimesliceExtensionMode::Require {
                abort_timeslice_extension(
                    "enable",
                    snapshot.reason.unwrap_or("unknown failure"),
                    snapshot.error_number,
                );
            }
        }

        #[inline(always)]
        pub fn on_critical_section_enter(&self) {
            if self.mode == TimesliceExtensionMode::Off {
                return;
            }

            with_thread_state(|state| {
                if !state.enabled {
                    return;
                }

                compiler_barrier();
                unsafe {
                    *(*state.rseq).slice_ctrl.request_ptr() = 1;
                }
                compiler_barrier();
            });
        }

        #[inline(always)]
        pub fn on_critical_section_exit(&self) {
            if self.mode == TimesliceExtensionMode::Off {
                return;
            }

            with_thread_state(|state| {
                if !state.enabled {
                    return;
                }

                compiler_barrier();
                unsafe {
                    *(*state.rseq).slice_ctrl.request_ptr() = 0;
                }
                compiler_barrier();

                let granted = unsafe { (*state.rseq).slice_ctrl.granted() };
                if granted == 0 {
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
                    state.error_number = err;
                    state.reason = Some("rseq_slice_yield is unavailable");
                    if self.mode == TimesliceExtensionMode::Require {
                        abort_timeslice_extension(
                            "yield",
                            state.reason.unwrap_or("unknown failure"),
                            state.error_number,
                        );
                    }
                }
            });
        }
    }

    pub fn current_thread_timeslice_extension_status(
        mode: TimesliceExtensionMode,
    ) -> TimesliceExtensionStatus {
        if mode == TimesliceExtensionMode::Off {
            return TimesliceExtensionStatus::default();
        }

        let snapshot = current_thread_state_snapshot();
        TimesliceExtensionStatus {
            enabled: snapshot.enabled,
            reason: snapshot.reason,
            error_number: snapshot.error_number,
        }
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
mod imp {
    use super::*;

    pub struct CriticalSectionTimesliceExtension {
        mode: TimesliceExtensionMode,
    }

    impl CriticalSectionTimesliceExtension {
        pub const fn new(mode: TimesliceExtensionMode) -> Self {
            Self { mode }
        }

        pub fn prepare_thread(&self) {
            if self.mode == TimesliceExtensionMode::Require {
                eprintln!("timeslice extension enable failed: unsupported platform");
                std::process::abort();
            }
        }

        #[inline(always)]
        pub fn on_critical_section_enter(&self) {}

        #[inline(always)]
        pub fn on_critical_section_exit(&self) {}
    }

    pub fn current_thread_timeslice_extension_status(
        mode: TimesliceExtensionMode,
    ) -> TimesliceExtensionStatus {
        if mode == TimesliceExtensionMode::Off {
            TimesliceExtensionStatus::default()
        } else {
            TimesliceExtensionStatus {
                enabled: false,
                reason: Some("timeslice extension is only supported on Linux/glibc"),
                error_number: 0,
            }
        }
    }
}

pub use imp::{CriticalSectionTimesliceExtension, current_thread_timeslice_extension_status};
