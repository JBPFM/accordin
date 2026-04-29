mod bpf_skel;
pub use bpf_skel::*;

pub use lb_shared::arch;
#[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
pub mod bpf_intf {
    include!(concat!(env!("OUT_DIR"), "/bpf_intf.rs"));
}
mod flexguard;
pub use lb_shared::lock_backend;
pub use lb_shared::lock_stats;
mod mutex_hook;

use std::mem::MaybeUninit;
use std::sync::OnceLock;

use anyhow::Result;
use libbpf_rs::{Link, MapHandle, OpenObject};
use log::info;
use scx_utils::{scx_ops_attach, scx_ops_load, scx_ops_open};

const SCHEDULER_NAME: &str = "flexguard_simple";
const DISABLE_BPF_ENV: &str = "LB_SIMPLE_DISABLE_BPF";
const STATS_ONLY_ENV: &str = "LB_SIMPLE_STATS_ONLY";

static SCHEDULER_STATE: OnceLock<SchedulerState> = OnceLock::new();

struct SchedulerState {
    _scheduler_link: Option<Link>,
    _flexguard_link: Option<Link>,
    _skel: Option<BpfSkel<'static>>,
}

unsafe impl Send for SchedulerState {}
unsafe impl Sync for SchedulerState {}

fn init_scheduler(debug: bool) -> Result<SchedulerState> {
    let mut skel_builder = BpfSkelBuilder::default();
    skel_builder.obj_builder.debug(debug);

    let open_object: &'static mut MaybeUninit<OpenObject> =
        Box::leak(Box::new(MaybeUninit::uninit()));

    let mut skel = scx_ops_open!(skel_builder, open_object, lb_simple_ops, None)?;
    let mut skel = scx_ops_load!(skel, lb_simple_ops, uei)?;

    let thread_ctx_map = MapHandle::try_from(&skel.maps.thread_ctx_addr_map)?;
    let nodes_map = MapHandle::try_from(&skel.maps.nodes_map)?;
    mutex_hook::set_thread_ctx_map(thread_ctx_map);
    mutex_hook::set_nodes_map(nodes_map);

    if let Some(bss) = skel.maps.bss_data.as_mut() {
        let qnodes: *mut crate::bpf_intf::flexguard_qnode_t =
            std::ptr::addr_of_mut!(bss.qnodes).cast();
        let num_preempted_holders: *mut i64 = std::ptr::addr_of_mut!(bss.num_preempted_holders);
        let preempted_flags: *mut u8 = std::ptr::addr_of_mut!(bss.preempted_flags).cast();
        flexguard::install_bpf_runtime(qnodes, num_preempted_holders, preempted_flags);
    }

    let scheduler_link = scx_ops_attach!(skel, lb_simple_ops)?;
    let flexguard_link = skel.links.sched_switch_btf.take();

    info!("{SCHEDULER_NAME} scheduler started via LD_PRELOAD");
    Ok(SchedulerState {
        _scheduler_link: Some(scheduler_link),
        _flexguard_link: flexguard_link,
        _skel: Some(skel),
    })
}

impl Drop for SchedulerState {
    fn drop(&mut self) {
        let _ = self._scheduler_link.take();
        let _ = self._flexguard_link.take();
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

    lb_shared::cpu_affinity::init_from_env("flexguard_simple");

    if env_flag(DISABLE_BPF_ENV) {
        info!(
            "{SCHEDULER_NAME} scheduler disabled by env {}",
            DISABLE_BPF_ENV
        );
        eprintln!(
            "[flexguard_simple] eBPF scheduler disabled by {}",
            DISABLE_BPF_ENV
        );
        return;
    }

    let stats_only = env_flag(STATS_ONLY_ENV);

    let _ = SCHEDULER_STATE.get_or_init(|| match init_scheduler(false) {
        Ok(state) => {
            if stats_only {
                info!(
                    "{SCHEDULER_NAME} stats-only env {} requested but ignored by minimal BPF controller",
                    STATS_ONLY_ENV
                );
                eprintln!(
                    "[flexguard_simple] stats-only env {} requested but ignored by minimal BPF controller",
                    STATS_ONLY_ENV
                );
            }
            eprintln!("[flexguard_simple] eBPF scheduler loaded successfully");
            state
        }
        Err(e) => {
            eprintln!("[flexguard_simple] Failed to load eBPF scheduler: {:#}", e);
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
        lock_stats::print_process_stats("flexguard_simple");
    }
    fini
};

#[unsafe(no_mangle)]
pub extern "C" fn lb_simple_dynamic_cpu_affinity_is_stable() -> libc::c_int {
    i32::from(lock_stats::dynamic_cpu_affinity_is_stable())
}

#[unsafe(no_mangle)]
pub extern "C" fn lb_simple_dynamic_cpu_affinity_freeze() {
    lock_stats::dynamic_cpu_affinity_freeze();
}

#[unsafe(no_mangle)]
pub extern "C" fn lb_simple_dynamic_cpu_affinity_begin_measurement() {
    lock_stats::dynamic_cpu_affinity_begin_measurement_for_thread();
}
