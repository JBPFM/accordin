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
/// - `single_lock_mode` — optional boolean that tells the BPF scheduler this backend uses the
///   legacy global admission word and should map it to the synthetic lock id.
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

        const _: () = assert!(
            $crate::cpu_affinity::MAX_CPUS == crate::bpf_intf::MAX_CPUS as usize,
            "cpu_affinity::MAX_CPUS and bpf_intf::MAX_CPUS must match"
        );

        static SCHEDULER_STATE: ::std::sync::OnceLock<SchedulerState> = ::std::sync::OnceLock::new();

        fn run_bpf_syscall_prog<T>(
            fd: ::std::os::fd::RawFd,
            args: &T,
            label: &str,
        ) -> ::std::result::Result<(), ::std::string::String> {
            let mut opts = ::libbpf_rs::libbpf_sys::bpf_test_run_opts {
                sz: ::std::mem::size_of::<::libbpf_rs::libbpf_sys::bpf_test_run_opts>() as _,
                ctx_in: (args as *const T).cast(),
                ctx_size_in: ::std::mem::size_of::<T>() as u32,
                ..::std::default::Default::default()
            };

            let ret = unsafe {
                ::libbpf_rs::libbpf_sys::bpf_prog_test_run_opts(fd, &mut opts)
            };
            if ret != 0 {
                return Err(::std::format!(
                    "bpf_prog_test_run_opts({label}) failed: {}",
                    ::std::io::Error::last_os_error()
                ));
            }

            let retval = opts.retval as i32;
            if retval != 0 {
                return Err(::std::format!("{label} returned {retval}"));
            }

            Ok(())
        }

        struct ActiveCpusProgSink {
            set_prog_fd: ::std::os::fd::RawFd,
            nudge_prog_fd: ::std::os::fd::RawFd,
            previous: ::std::sync::Mutex<Option<[u8; $crate::cpu_affinity::MAX_CPUS]>>,
        }

        struct EagerReleaseProgSink {
            release_prog_fd: ::std::os::fd::RawFd,
        }

        impl ActiveCpusProgSink {
            fn set_active_cpus(
                &self,
                wanted: &[u8; $crate::cpu_affinity::MAX_CPUS],
            ) -> ::std::result::Result<(), ::std::string::String> {
                let mut wanted_words = [0u64; $crate::cpu_affinity::ACTIVE_CPU_WORDS];
                for (cpu, active) in wanted.iter().enumerate() {
                    if *active != 0 {
                        wanted_words[cpu / 64] |= 1u64 << (cpu % 64);
                    }
                }

                let args = crate::bpf_intf::accordin_active_cpus_args {
                    wanted0: wanted_words[0],
                    wanted1: wanted_words[1],
                    wanted2: wanted_words[2],
                    wanted3: wanted_words[3],
                    nr_cpus: crate::bpf_intf::MAX_CPUS,
                };
                run_bpf_syscall_prog(self.set_prog_fd, &args, "accordin_set_active_cpus")
            }

            fn nudge_cpu(
                &self,
                cpu: usize,
                drain_inactive: bool,
            ) -> ::std::result::Result<(), ::std::string::String> {
                let args = crate::bpf_intf::accordin_cpu_nudge_args {
                    cpu: cpu as u32,
                    drain_inactive: u32::from(drain_inactive),
                };
                run_bpf_syscall_prog(self.nudge_prog_fd, &args, "accordin_nudge_cpu")
            }

            fn nudge_changed_cpus(
                &self,
                previous: Option<[u8; $crate::cpu_affinity::MAX_CPUS]>,
                wanted: &[u8; $crate::cpu_affinity::MAX_CPUS],
            ) {
                let Some(previous) = previous else {
                    return;
                };

                let mut removed_any = false;
                for cpu in 0..$crate::cpu_affinity::MAX_CPUS {
                    if previous[cpu] != 0 && wanted[cpu] == 0 {
                        removed_any = true;
                        if let Err(error) = self.nudge_cpu(cpu, true) {
                            eprintln!(
                                "[{}] failed to drain inactive CPU {}: {}",
                                SCHEDULER_NAME, cpu, error
                            );
                        }
                    } else if previous[cpu] == 0 && wanted[cpu] != 0 {
                        if let Err(error) = self.nudge_cpu(cpu, false) {
                            eprintln!(
                                "[{}] failed to kick newly active CPU {}: {}",
                                SCHEDULER_NAME, cpu, error
                            );
                        }
                    }
                }

                if removed_any {
                    if let Some(cpu) = wanted.iter().position(|active| *active != 0) {
                        if let Err(error) = self.nudge_cpu(cpu, false) {
                            eprintln!(
                                "[{}] failed to kick active CPU {} after drain: {}",
                                SCHEDULER_NAME, cpu, error
                            );
                        }
                    }
                }
            }
        }

        impl $crate::cpu_affinity::BpfActiveCpusSink for ActiveCpusProgSink {
            fn push(
                &self,
                wanted: &[u8; $crate::cpu_affinity::MAX_CPUS],
            ) -> ::std::result::Result<(), ::std::string::String> {
                self.set_active_cpus(wanted)?;

                let previous = {
                    let mut previous = self
                        .previous
                        .lock()
                        .map_err(|_| "BPF active CPU previous-mask lock is poisoned".to_string())?;
                    let old = *previous;
                    *previous = Some(*wanted);
                    old
                };
                self.nudge_changed_cpus(previous, wanted);
                Ok(())
            }
        }

        impl $crate::admission::EagerReleaseSink for EagerReleaseProgSink {
            fn release_current(&self) -> ::std::result::Result<(), ::std::string::String> {
                let args = crate::bpf_intf::accordin_release_admission_args { reserved: 0 };
                run_bpf_syscall_prog(
                    self.release_prog_fd,
                    &args,
                    "accordin_release_current_admission",
                )
            }
        }

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
                bss.use_controlled_dsq =
                    u32::from($crate::cpu_affinity::controlled_dsq_required_by_env());
                bss.admission_debug_mode = u32::from(debug_counters);
                bss.global_inactive_dsq_mode =
                    u32::from($crate::env::inactive_dsq_global_enabled_by_env());
                bss.eager_token_release_mode =
                    u32::from($crate::env::eager_token_release_enabled_by_env());
            }
            let mut skel = ::scx_utils::scx_ops_load!(skel, accordin_ops, uei)?;

            let thread_ctx_map = ::libbpf_rs::MapHandle::try_from(&skel.maps.thread_ctx_addr_map)?;
            $crate::mutex_hook::set_thread_ctx_map(thread_ctx_map);

            let active_cpus_prog_fd = {
                use ::std::os::fd::{AsFd, AsRawFd};
                skel.progs.accordin_set_active_cpus.as_fd().as_raw_fd()
            };
            let nudge_cpu_prog_fd = {
                use ::std::os::fd::{AsFd, AsRawFd};
                skel.progs.accordin_nudge_cpu.as_fd().as_raw_fd()
            };
            let release_current_admission_prog_fd = {
                use ::std::os::fd::{AsFd, AsRawFd};
                skel.progs.accordin_release_current_admission.as_fd().as_raw_fd()
            };
            $crate::cpu_affinity::set_bpf_sink(Box::new(ActiveCpusProgSink {
                set_prog_fd: active_cpus_prog_fd,
                nudge_prog_fd: nudge_cpu_prog_fd,
                previous: ::std::sync::Mutex::new(Some([0; $crate::cpu_affinity::MAX_CPUS])),
            }))
            .map_err(::anyhow::Error::msg)?;
            $crate::admission::set_eager_release_sink(Box::new(EagerReleaseProgSink {
                release_prog_fd: release_current_admission_prog_fd,
            }))
            .map_err(::anyhow::Error::msg)?;
            $crate::cpu_affinity::init_from_env(SCHEDULER_NAME);

            let link = ::scx_utils::scx_ops_attach!(skel, accordin_ops)?;
            $crate::cpu_affinity::push_initial_mask_to_bpf().map_err(::anyhow::Error::msg)?;

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
                $crate::cpu_affinity::init_from_env(SCHEDULER_NAME);
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
                        ::log::info!(
                            "{SCHEDULER_NAME} admission debug counters enabled by env {}",
                            DEBUG_COUNTERS_ENV
                        );
                        eprintln!(
                            "[{}] admission debug counters enabled by env {}",
                            SCHEDULER_NAME, DEBUG_COUNTERS_ENV
                        );
                    }
                    if $crate::env::inactive_dsq_global_enabled_by_env() {
                        eprintln!("[{}] global inactive DSQ ablation enabled by {}", SCHEDULER_NAME, $crate::env::INACTIVE_DSQ_ENV);
                    }
                    if $crate::env::eager_token_release_enabled_by_env() {
                        eprintln!("[{}] eager token release ablation enabled by {}", SCHEDULER_NAME, $crate::env::EAGER_TOKEN_RELEASE_ENV);
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
