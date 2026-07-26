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
                );
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

            let stats_only = $crate::env::env_flag(STATS_ONLY_ENV);
            let debug_counters = $crate::env::env_flag(DEBUG_COUNTERS_ENV);

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
