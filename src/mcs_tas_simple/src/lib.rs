mod bpf_skel;
pub use bpf_skel::*;

pub use lb_shared::arch;
#[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
pub mod bpf_intf {
    include!(concat!(env!("OUT_DIR"), "/bpf_intf.rs"));
}
pub use lb_shared::lock_backend;
pub use lb_shared::lock_stats;
mod mcs_tas;
mod mutex_hook;

use std::mem::MaybeUninit;
use std::sync::OnceLock;

use anyhow::Result;
use libbpf_rs::{Link, MapHandle, OpenObject};
use log::info;
use scx_utils::{scx_ops_attach, scx_ops_load, scx_ops_open};

const SCHEDULER_NAME: &str = "mcs_tas_simple";
const DISABLE_BPF_ENV: &str = "MCS_TAS_SIMPLE_DISABLE_BPF";
const STATS_ONLY_ENV: &str = "MCS_TAS_SIMPLE_STATS_ONLY";
const DEBUG_COUNTERS_ENV: &str = "MCS_TAS_SIMPLE_DEBUG_COUNTERS";

static SCHEDULER_STATE: OnceLock<SchedulerState> = OnceLock::new();

struct SchedulerState {
    _link: Option<Link>,
    _skel: Option<BpfSkel<'static>>,
}

unsafe impl Send for SchedulerState {}
unsafe impl Sync for SchedulerState {}

fn init_scheduler(debug: bool, _stats_only: bool, _debug_counters: bool) -> Result<SchedulerState> {
    let mut skel_builder = BpfSkelBuilder::default();
    skel_builder.obj_builder.debug(debug);

    let open_object: &'static mut MaybeUninit<OpenObject> =
        Box::leak(Box::new(MaybeUninit::uninit()));

    let mut skel = scx_ops_open!(skel_builder, open_object, lb_simple_ops, None)?;
    let mut skel = scx_ops_load!(skel, lb_simple_ops, uei)?;

    let thread_ctx_map = MapHandle::try_from(&skel.maps.thread_ctx_addr_map)?;
    mutex_hook::set_thread_ctx_map(thread_ctx_map);

    let link = scx_ops_attach!(skel, lb_simple_ops)?;

    info!("{SCHEDULER_NAME} scheduler started via LD_PRELOAD");
    Ok(SchedulerState {
        _link: Some(link),
        _skel: Some(skel),
    })
}

impl Drop for SchedulerState {
    fn drop(&mut self) {
        let _ = self._link.take();
        let _ = self._skel.take();
        info!("{SCHEDULER_NAME} scheduler stopped");
    }
}

fn env_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim();
            value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("on")
        }
        Err(_) => false,
    }
}

fn init_ebpf() {
    if cfg!(test) {
        return;
    }

    let _ = simplelog::TermLogger::init(
        simplelog::LevelFilter::Info,
        simplelog::Config::default(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    );

    if env_flag(DISABLE_BPF_ENV) {
        info!(
            "{SCHEDULER_NAME} scheduler disabled by env {}",
            DISABLE_BPF_ENV
        );
        eprintln!("[mcs_tas_simple] eBPF scheduler disabled by {}", DISABLE_BPF_ENV);
        return;
    }

    let stats_only = env_flag(STATS_ONLY_ENV);
    let debug_counters = env_flag(DEBUG_COUNTERS_ENV);

    let _ = SCHEDULER_STATE.get_or_init(|| match init_scheduler(false, stats_only, debug_counters) {
        Ok(state) => {
            if stats_only {
                info!(
                    "{SCHEDULER_NAME} stats-only env {} requested but ignored by minimal BPF controller",
                    STATS_ONLY_ENV
                );
                eprintln!(
                    "[mcs_tas_simple] stats-only env {} requested but ignored by minimal BPF controller",
                    STATS_ONLY_ENV
                );
            }
            if debug_counters {
                info!(
                    "{SCHEDULER_NAME} debug-counter env {} requested but ignored by minimal BPF controller",
                    DEBUG_COUNTERS_ENV
                );
                eprintln!(
                    "[mcs_tas_simple] debug-counter env {} requested but ignored by minimal BPF controller",
                    DEBUG_COUNTERS_ENV
                );
            }
            eprintln!("[mcs_tas_simple] eBPF scheduler loaded successfully");
            state
        }
        Err(e) => {
            eprintln!("[mcs_tas_simple] Failed to load eBPF scheduler: {:#}", e);
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
