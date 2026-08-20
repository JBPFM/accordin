/// Emits the boilerplate that loads the per-crate BPF skeleton and registers `.init_array`/
/// `.fini_array` constructors plus the dynamic-CPU-affinity C shims.
///
/// The calling crate must already provide:
/// - `mod bpf_skel; pub use bpf_skel::*;` so that `BpfSkel`, `BpfSkelBuilder` and the
///   `accordin_ops` token used by the `scx_ops_*!` macros are in scope.
/// - dependencies on `anyhow`, `libbpf_rs`, `log`, `simplelog`, `scx_utils`, and `libc`.
///
/// # Parameters
/// - `scheduler_name` — lowercase name used for log lines, the `[<name>]` prefix, and as the
///   argument to `lock_stats::print_process_stats`.
/// - `env_prefix` — uppercase prefix for `*_DISABLE_BPF`, `*_STATS_ONLY`,
///   `*_DEBUG_COUNTERS`, and `*_INACTIVE_PREVIOUS_LOCK_PERCENT`.
/// - `single_lock_mode` — optional boolean that tells the BPF scheduler this backend uses the
///   legacy global admission word and therefore does not need per-lock inactive DSQs.
///
/// `ACCORDIN_CV_ROUTE` and `ACCORDIN_CV_PRIO` are read unprefixed, because the
/// mutex hook reads the same names whichever backend is preloaded.
#[macro_export]
macro_rules! define_scheduler_loader {
    (scheduler_name = $scheduler:expr, env_prefix = $env_prefix:expr $(,)?) => {
        $crate::define_scheduler_loader!(
            scheduler_name = $scheduler,
            env_prefix = $env_prefix,
            single_lock_mode = false,
        );
    };
    (
        scheduler_name = $scheduler:expr,
        env_prefix = $env_prefix:expr,
        single_lock_mode = $single_lock_mode:expr $(,)?
    ) => {
        const SCHEDULER_NAME: &str = $scheduler;
        const DISABLE_BPF_ENV: &str = concat!($env_prefix, "_DISABLE_BPF");
        const STATS_ONLY_ENV: &str = concat!($env_prefix, "_STATS_ONLY");
        const DEBUG_COUNTERS_ENV: &str = concat!($env_prefix, "_DEBUG_COUNTERS");
        const INACTIVE_PREVIOUS_LOCK_PERCENT_ENV: &str =
            concat!($env_prefix, "_INACTIVE_PREVIOUS_LOCK_PERCENT");
        // Not prefixed: the mutex hook publishes the cond-sleep bit under the
        // same variable, and the two sides have to agree whatever backend the
        // preload happens to be.
        const CV_ROUTE_ENV: &str = "ACCORDIN_CV_ROUTE";
        const CV_PRIORITY_STREAK_ENV: &str = "ACCORDIN_CV_PRIO";

        // The scheduler reads the user admission word through the layout these
        // constants describe, so a divergence between the generated bindings
        // and the userspace mirror has to fail the build rather than misroute
        // at run time.
        const _: () = {
            assert!(
                crate::bpf_intf::USER_ADMISSION_FLAG_MASK
                    == $crate::admission::USER_ADMISSION_FLAG_MASK
            );
            assert!(
                crate::bpf_intf::USER_ADMISSION_LOCK_ID_SHIFT
                    == $crate::admission::USER_ADMISSION_LOCK_ID_SHIFT
            );
            assert!(crate::bpf_intf::MAX_LOCK_CLASSES == $crate::admission::MAX_LOCK_CLASSES);
            // The userspace mirror of the flag itself is private to the
            // admission module, so the bit is pinned by position and by the
            // two invariants the routing depends on: it lives inside the flag
            // field, and it does not collide with the other three flags.
            assert!(crate::bpf_intf::USER_ADMISSION_CV_SLEEP == 1 << 3);
            assert!(
                crate::bpf_intf::USER_ADMISSION_CV_SLEEP
                    & crate::bpf_intf::USER_ADMISSION_FLAG_MASK
                    == crate::bpf_intf::USER_ADMISSION_CV_SLEEP
            );
            assert!(
                crate::bpf_intf::USER_ADMISSION_CV_SLEEP
                    & (crate::bpf_intf::USER_ADMISSION_IN_CRITICAL_SECTION
                        | crate::bpf_intf::USER_ADMISSION_SLOW_PATH_PENDING
                        | crate::bpf_intf::USER_ADMISSION_TOKEN_CONSUMED)
                    == 0
            );
            assert!(
                crate::bpf_intf::USER_ADMISSION_FLAG_MASK
                    == (1 << crate::bpf_intf::USER_ADMISSION_LOCK_ID_SHIFT) - 1
            );
        };

        static SCHEDULER_STATE: ::std::sync::OnceLock<SchedulerState> = ::std::sync::OnceLock::new();

        struct SchedulerState {
            _link: Option<::libbpf_rs::Link>,
            _skel: Option<BpfSkel<'static>>,
        }

        unsafe impl Send for SchedulerState {}
        unsafe impl Sync for SchedulerState {}

        fn init_scheduler(
            debug: bool,
            stats_only: bool,
            debug_counters: bool,
        ) -> ::anyhow::Result<SchedulerState> {
            let mut skel_builder = BpfSkelBuilder::default();
            skel_builder.obj_builder.debug(debug);

            let open_object: &'static mut ::std::mem::MaybeUninit<::libbpf_rs::OpenObject> =
                Box::leak(Box::new(::std::mem::MaybeUninit::uninit()));

            let mut skel = ::scx_utils::scx_ops_open!(skel_builder, open_object, accordin_ops, None)?;
            if let Some(bss) = skel.maps.bss_data.as_deref_mut() {
                bss.stats_only_mode = u32::from(stats_only);
                bss.single_lock_mode = u32::from($single_lock_mode);
                bss.debug_counters_mode = u32::from(debug_counters);
                bss.registered_thread_count = 0;
                bss.inactive_previous_lock_percent = $crate::env::env_u32_clamped(
                    INACTIVE_PREVIOUS_LOCK_PERCENT_ENV,
                    crate::bpf_intf::INACTIVE_PREVIOUS_LOCK_PERCENT_DEFAULT,
                    0,
                    100,
                );
                bss.cv_route_enabled = u32::from($crate::env::env_flag(CV_ROUTE_ENV));
                bss.cv_priority_streak_limit = $crate::env::env_u32_clamped(
                    CV_PRIORITY_STREAK_ENV,
                    crate::bpf_intf::CV_PRIORITY_STREAK_LIMIT_DEFAULT,
                    0,
                    crate::bpf_intf::CV_PRIORITY_STREAK_LIMIT_MAX,
                );
                bss.width_control_enabled = u32::from($crate::width_control::enabled());
                if let Some(class_widths) = $crate::width_control::fixed_class_widths() {
                    bss.class_width = class_widths;
                }
            }
            let mut skel = ::scx_utils::scx_ops_load!(skel, accordin_ops, uei)?;

            let thread_ctx_map = ::libbpf_rs::MapHandle::try_from(&skel.maps.thread_ctx_addr_map)?;
            $crate::mutex_hook::set_thread_ctx_map(thread_ctx_map);
            if let Some(bss) = skel.maps.bss_data.as_deref_mut() {
                $crate::mutex_hook::set_registered_thread_count_ptr(
                    &mut bss.registered_thread_count as *mut u32,
                );
                $crate::width_control::set_class_state_ptrs(
                    bss.class_width.as_mut_ptr(),
                    bss.class_active.as_mut_ptr(),
                    bss.class_active_peak.as_mut_ptr(),
                    bss.class_inactive_depth.as_mut_ptr(),
                    bss.class_inactive_depth_peak.as_mut_ptr(),
                    &mut bss.inactive_probe_span as *mut u32,
                );
                if debug_counters {
                    $crate::bpf_counters::set_routing_counter_ptrs([
                        &mut bss.select_local_direct as *mut u64,
                        &mut bss.wake_consumed_seen as *mut u64,
                        &mut bss.wake_consumed_granted as *mut u64,
                        &mut bss.wake_consumed_inactive as *mut u64,
                        &mut bss.wake_consumed_normal as *mut u64,
                        &mut bss.wake_read_fail as *mut u64,
                        &mut bss.running_pending_grant_success as *mut u64,
                        &mut bss.running_pending_grant_failure as *mut u64,
                        &mut bss.block_release_read_fail as *mut u64,
                        &mut bss.cv_wake_enq as *mut u64,
                        &mut bss.cv_grant_at_enq as *mut u64,
                        &mut bss.cv_parked as *mut u64,
                        &mut bss.cv_dispatch as *mut u64,
                        &mut bss.cv_dispatch_forced as *mut u64,
                        &mut bss.cv_word_read_fail as *mut u64,
                    ]);
                }
            }

            let link = ::scx_utils::scx_ops_attach!(skel, accordin_ops)?;

            ::log::info!("{SCHEDULER_NAME} scheduler started via LD_PRELOAD");
            Ok(SchedulerState {
                _link: Some(link),
                _skel: Some(skel),
            })
        }

        impl Drop for SchedulerState {
            fn drop(&mut self) {
                $crate::mutex_hook::set_registered_thread_count_ptr(::std::ptr::null_mut());
                $crate::width_control::clear_class_state_ptrs();
                $crate::bpf_counters::clear_routing_counter_ptrs();
                let _ = self._link.take();
                let _ = self._skel.take();
                ::log::info!("{SCHEDULER_NAME} scheduler stopped");
            }
        }

        fn init_ebpf() {
            if cfg!(test) {
                return;
            }

            let _ = ::simplelog::TermLogger::init(
                ::simplelog::LevelFilter::Info,
                ::simplelog::Config::default(),
                ::simplelog::TerminalMode::Stderr,
                ::simplelog::ColorChoice::Auto,
            );

            let stats_only = $crate::env::env_flag(STATS_ONLY_ENV);
            let debug_counters = $crate::env::env_flag(DEBUG_COUNTERS_ENV);
            // Userspace hint counters stay valid without the scheduler, so they
            // are armed before the disable-bpf exit.
            $crate::mutex_hook::set_cv_admission_counters_enabled(debug_counters);

            if $crate::env::env_flag(DISABLE_BPF_ENV) {
                ::log::info!(
                    "{SCHEDULER_NAME} scheduler disabled by env {}",
                    DISABLE_BPF_ENV
                );
                eprintln!(
                    "[{}] eBPF scheduler disabled by {}",
                    SCHEDULER_NAME, DISABLE_BPF_ENV
                );
                return;
            }

            let _ = SCHEDULER_STATE.get_or_init(|| match init_scheduler(false, stats_only, debug_counters) {
                Ok(state) => {
                    if stats_only {
                        ::log::info!(
                            "{SCHEDULER_NAME} stats-only env {} requested; lock-aware scheduling disabled",
                            STATS_ONLY_ENV
                        );
                        eprintln!(
                            "[{}] stats-only env {} requested; lock-aware scheduling disabled",
                            SCHEDULER_NAME, STATS_ONLY_ENV
                        );
                    }
                    if $single_lock_mode {
                        ::log::info!("{SCHEDULER_NAME} single-lock BPF mode enabled");
                        eprintln!("[{}] single-lock BPF mode enabled", SCHEDULER_NAME);
                    }
                    if debug_counters {
                        ::log::info!("{SCHEDULER_NAME} BPF debug counters enabled");
                        eprintln!("[{}] BPF debug counters enabled", SCHEDULER_NAME);
                    }
                    eprintln!("[{}] eBPF scheduler loaded successfully", SCHEDULER_NAME);
                    state
                }
                Err(e) => {
                    eprintln!("[{}] Failed to load eBPF scheduler: {:#}", SCHEDULER_NAME, e);
                    panic!("eBPF initialization failed");
                }
            });
        }

        #[unsafe(link_section = ".init_array")]
        #[used]
        static INIT: extern "C" fn() = {
            extern "C" fn init() {
                init_ebpf();
            }
            init
        };

        #[unsafe(link_section = ".fini_array")]
        #[used]
        static FINI: extern "C" fn() = {
            extern "C" fn fini() {
                $crate::lock_stats::print_process_stats(SCHEDULER_NAME);
            }
            fini
        };

        #[unsafe(no_mangle)]
        pub extern "C" fn accordin_dynamic_cpu_affinity_is_stable() -> ::libc::c_int {
            i32::from($crate::lock_stats::dynamic_cpu_affinity_is_stable())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn accordin_dynamic_cpu_affinity_freeze() {
            $crate::lock_stats::dynamic_cpu_affinity_freeze();
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn accordin_dynamic_cpu_affinity_begin_measurement() {
            $crate::lock_stats::dynamic_cpu_affinity_begin_measurement_for_thread();
        }
    };
}
