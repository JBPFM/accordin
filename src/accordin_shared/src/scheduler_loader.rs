/// Loads a direct library's BPF skeleton at initialization.
/// The caller provides the generated skeleton,
/// interface bindings, and the libbpf/scx/logging dependencies.
#[macro_export]
macro_rules! define_scheduler_loader {
    (scheduler_name = $scheduler:expr, env_prefix = $env_prefix:expr $(,)?) => {
        const SCHEDULER_NAME: &str = $scheduler;
        const DISABLE_BPF_ENV: &str = concat!($env_prefix, "_DISABLE_BPF");
        const STATS_ONLY_ENV: &str = concat!($env_prefix, "_STATS_ONLY");
        // Fail the build if the userspace/BPF protocol diverges.
        const _: () = {
            assert!(crate::bpf_intf::USER_HELD == $crate::admission::HELD);
            assert!(crate::bpf_intf::USER_WAITING == $crate::admission::WAITING);
            assert!(crate::bpf_intf::USER_SPINNING == $crate::admission::SPINNING);
            assert!(crate::bpf_intf::USER_FLAGS == $crate::admission::FLAGS);
            assert!(crate::bpf_intf::MAX_CPUS as usize == $crate::admission::MAX_CPUS);
            assert!(::std::mem::size_of::<crate::bpf_intf::admission_state>()
                == ::std::mem::size_of::<$crate::admission::SchedulerAdmission>());
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
        ) -> ::anyhow::Result<SchedulerState> {
            let mut skel_builder = BpfSkelBuilder::default();
            skel_builder.obj_builder.debug(debug);

            let open_object: &'static mut ::std::mem::MaybeUninit<::libbpf_rs::OpenObject> =
                Box::leak(Box::new(::std::mem::MaybeUninit::uninit()));

            let mut skel = ::scx_utils::scx_ops_open!(skel_builder, open_object, accordin_ops, None)?;
            if let Some(bss) = skel.maps.bss_data.as_deref_mut() {
                bss.stats_only_mode = u32::from(stats_only);
            }
            let mut skel = ::scx_utils::scx_ops_load!(skel, accordin_ops, uei)?;

            let thread_ctx_map = ::libbpf_rs::MapHandle::try_from(&skel.maps.thread_ctx_addr_map)?;
            $crate::direct_runtime::set_thread_ctx_map(thread_ctx_map);
            let link = ::scx_utils::scx_ops_attach!(skel, accordin_ops)?;
            if let Some(bss) = skel.maps.bss_data.as_deref_mut() {
                unsafe {
                    $crate::admission::set_scheduler(
                        ::std::ptr::addr_of_mut!(bss.admission).cast(),
                    );
                }
            }

            ::log::info!("{SCHEDULER_NAME} scheduler started via the direct library");
            Ok(SchedulerState {
                _link: Some(link),
                _skel: Some(skel),
            })
        }

        impl Drop for SchedulerState {
            fn drop(&mut self) {
                unsafe { $crate::admission::set_scheduler(::std::ptr::null_mut()); }
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

            let _ = SCHEDULER_STATE.get_or_init(|| match init_scheduler(false, stats_only) {
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

    };
}
