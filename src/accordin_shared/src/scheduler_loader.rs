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
///   argument to `cpu_affinity::init_from_env` and `lock_stats::print_process_stats`.
/// - `env_prefix` — uppercase prefix for the three `*_DISABLE_BPF` / `*_STATS_ONLY` /
///   `*_DEBUG_COUNTERS` environment variables.
#[macro_export]
macro_rules! define_scheduler_loader {
    (scheduler_name = $scheduler:expr, env_prefix = $env_prefix:expr $(,)?) => {
        const SCHEDULER_NAME: &str = $scheduler;
        const DISABLE_BPF_ENV: &str = concat!($env_prefix, "_DISABLE_BPF");
        const STATS_ONLY_ENV: &str = concat!($env_prefix, "_STATS_ONLY");
        const DEBUG_COUNTERS_ENV: &str = concat!($env_prefix, "_DEBUG_COUNTERS");
        const INACTIVE_POOL_ENV: &str = "ACCORDIN_INACTIVE_POOL";
        const BPF_DEBUG_ENV: &str = "ACCORDIN_BPF_DEBUG";

        static SCHEDULER_STATE: ::std::sync::OnceLock<SchedulerState> = ::std::sync::OnceLock::new();

        struct SchedulerState {
            _link: Option<::libbpf_rs::Link>,
            _skel: Option<BpfSkel<'static>>,
        }

        unsafe impl Send for SchedulerState {}
        unsafe impl Sync for SchedulerState {}

        fn init_scheduler(
            debug: bool,
            _stats_only: bool,
            _debug_counters: bool,
            distributed_inactive_pool: bool,
            initial_lock_budget: u32,
        ) -> ::anyhow::Result<SchedulerState> {
            let mut skel_builder = BpfSkelBuilder::default();
            skel_builder.obj_builder.debug(debug);

            let open_object: &'static mut ::std::mem::MaybeUninit<::libbpf_rs::OpenObject> =
                Box::leak(Box::new(::std::mem::MaybeUninit::uninit()));

            let mut skel = ::scx_utils::scx_ops_open!(skel_builder, open_object, accordin_ops, None)?;
            configure_bpf_rodata(&mut skel, distributed_inactive_pool, initial_lock_budget);
            let mut skel = ::scx_utils::scx_ops_load!(skel, accordin_ops, uei)?;

            let thread_ctx_map = ::libbpf_rs::MapHandle::try_from(&skel.maps.thread_ctx_addr_map)?;
            $crate::mutex_hook::set_thread_ctx_map(thread_ctx_map);

            let link = ::scx_utils::scx_ops_attach!(skel, accordin_ops)?;

            ::log::info!("{SCHEDULER_NAME} scheduler started via LD_PRELOAD");
            Ok(SchedulerState {
                _link: Some(link),
                _skel: Some(skel),
            })
        }

        impl Drop for SchedulerState {
            fn drop(&mut self) {
                let _ = self._link.take();
                let _ = self._skel.take();
                ::log::info!("{SCHEDULER_NAME} scheduler stopped");
            }
        }

        fn inactive_pool_distributed() -> bool {
            match ::std::env::var(INACTIVE_POOL_ENV) {
                Ok(value) => {
                    let value = value.trim();
                    if value.eq_ignore_ascii_case("local")
                        || value.eq_ignore_ascii_case("per_cpu")
                        || value.eq_ignore_ascii_case("per-cpu")
                        || value == "0"
                        || value.eq_ignore_ascii_case("false")
                        || value.eq_ignore_ascii_case("no")
                        || value.eq_ignore_ascii_case("off")
                    {
                        false
                    } else {
                        value.is_empty()
                            || value.eq_ignore_ascii_case("distributed")
                            || value == "1"
                            || value.eq_ignore_ascii_case("true")
                            || value.eq_ignore_ascii_case("yes")
                            || value.eq_ignore_ascii_case("on")
                    }
                }
                Err(::std::env::VarError::NotPresent) => true,
                Err(::std::env::VarError::NotUnicode(_)) => false,
            }
        }

        fn initial_lock_budget_from_env() -> u32 {
            match $crate::cpu_affinity::requested_cpu_count_from_env() {
                Ok(Some(value)) => value.min(u32::MAX as usize) as u32,
                Ok(None) => 0,
                Err(error) => {
                    eprintln!(
                        "[{}] distributed inactive pool K ignored: {error}",
                        SCHEDULER_NAME
                    );
                    0
                }
            }
        }

        fn configure_bpf_rodata(
            skel: &mut OpenBpfSkel<'_>,
            distributed_inactive_pool: bool,
            initial_lock_budget: u32,
        ) {
            let Some(rodata) = skel.maps.rodata_data.as_deref_mut() else {
                return;
            };

            rodata.distributed_inactive_pool = u32::from(distributed_inactive_pool);
            rodata.initial_lock_budget = initial_lock_budget;
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

            let distributed_inactive_pool = inactive_pool_distributed();
            let initial_lock_budget = if distributed_inactive_pool {
                $crate::cpu_affinity::disable_process_affinity_control();
                initial_lock_budget_from_env()
            } else {
                0
            };

            if distributed_inactive_pool {
                eprintln!(
                    "[{}] distributed inactive pool enabled with initial K={}",
                    SCHEDULER_NAME,
                    if initial_lock_budget == 0 {
                        "all".to_string()
                    } else {
                        initial_lock_budget.to_string()
                    }
                );
            } else {
                $crate::cpu_affinity::init_from_env(SCHEDULER_NAME);
            }

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
            let bpf_debug = $crate::env::env_flag(BPF_DEBUG_ENV);

            let _ = SCHEDULER_STATE.get_or_init(|| match init_scheduler(
                bpf_debug,
                stats_only,
                debug_counters,
                distributed_inactive_pool,
                initial_lock_budget,
            ) {
                Ok(state) => {
                    if stats_only {
                        ::log::info!(
                            "{SCHEDULER_NAME} stats-only env {} requested but ignored by minimal BPF controller",
                            STATS_ONLY_ENV
                        );
                        eprintln!(
                            "[{}] stats-only env {} requested but ignored by minimal BPF controller",
                            SCHEDULER_NAME, STATS_ONLY_ENV
                        );
                    }
                    if debug_counters {
                        ::log::info!(
                            "{SCHEDULER_NAME} debug-counter env {} requested but ignored by minimal BPF controller",
                            DEBUG_COUNTERS_ENV
                        );
                        eprintln!(
                            "[{}] debug-counter env {} requested but ignored by minimal BPF controller",
                            SCHEDULER_NAME, DEBUG_COUNTERS_ENV
                        );
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
