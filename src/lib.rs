// SPDX-License-Identifier: GPL-2.0-only
//
// 动态链接库入口，在加载时初始化 eBPF 调度器

mod bpf_skel;
pub use bpf_skel::*;
pub mod bpf_intf;
pub use accordin_shared::arch;
pub use accordin_shared::cpu_affinity;
pub use accordin_shared::lock_backend;
pub use accordin_shared::lock_stats;
#[path = "mcs_tas_accordin/src/mcs_tas.rs"]
mod mcs_tas;
mod mutex_hook;

type AccordinRawLock = mcs_tas::McsTasLockRaw;

use std::mem::MaybeUninit;
use std::sync::OnceLock;

use anyhow::Result;
use libbpf_rs::{Link, MapHandle, OpenObject};
use log::info;
use scx_utils::{scx_ops_attach, scx_ops_load, scx_ops_open};

const SCHEDULER_NAME: &str = "accordin";
const DISABLE_BPF_ENV: &str = "ACCORDIN_DISABLE_BPF";
const STATS_ONLY_ENV: &str = "ACCORDIN_STATS_ONLY";
const DEBUG_COUNTERS_ENV: &str = "ACCORDIN_DEBUG_COUNTERS";
const INACTIVE_POOL_ENV: &str = "ACCORDIN_INACTIVE_POOL";

static SCHEDULER_STATE: OnceLock<SchedulerState> = OnceLock::new();

struct SchedulerState {
    _link: Option<Link>,
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
) -> Result<SchedulerState> {
    let mut skel_builder = BpfSkelBuilder::default();
    skel_builder.obj_builder.debug(debug);

    let open_object: &'static mut MaybeUninit<OpenObject> =
        Box::leak(Box::new(MaybeUninit::uninit()));

    let mut skel = scx_ops_open!(skel_builder, open_object, accordin_ops, None)?;
    configure_bpf_rodata(&mut skel, distributed_inactive_pool, initial_lock_budget);
    let mut skel = scx_ops_load!(skel, accordin_ops, uei)?;

    let thread_ctx_map = MapHandle::try_from(&skel.maps.thread_ctx_addr_map)?;
    accordin_shared::mutex_hook::set_thread_ctx_map(thread_ctx_map);

    let link = scx_ops_attach!(skel, accordin_ops)?;

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

fn inactive_pool_distributed() -> bool {
    match std::env::var(INACTIVE_POOL_ENV) {
        Ok(value) => {
            let value = value.trim();
            value.eq_ignore_ascii_case("distributed")
                || value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("on")
        }
        Err(_) => false,
    }
}

fn initial_lock_budget_from_env() -> u32 {
    match accordin_shared::cpu_affinity::requested_cpu_count_from_env() {
        Ok(Some(value)) => value.min(u32::MAX as usize) as u32,
        Ok(None) => 0,
        Err(error) => {
            eprintln!("[accordin] distributed inactive pool K ignored: {error}");
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

    let _ = simplelog::TermLogger::init(
        simplelog::LevelFilter::Info,
        simplelog::Config::default(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    );

    let distributed_inactive_pool = inactive_pool_distributed();
    let initial_lock_budget = if distributed_inactive_pool {
        accordin_shared::cpu_affinity::disable_process_affinity_control();
        initial_lock_budget_from_env()
    } else {
        0
    };

    if distributed_inactive_pool {
        eprintln!(
            "[accordin] distributed inactive pool enabled with initial K={}",
            if initial_lock_budget == 0 {
                "all".to_string()
            } else {
                initial_lock_budget.to_string()
            }
        );
    } else {
        accordin_shared::cpu_affinity::init_from_env("accordin");
    }

    if env_flag(DISABLE_BPF_ENV) {
        info!(
            "{SCHEDULER_NAME} scheduler disabled by env {}",
            DISABLE_BPF_ENV
        );
        eprintln!("[accordin] eBPF scheduler disabled by {}", DISABLE_BPF_ENV);
        return;
    }

    let stats_only = env_flag(STATS_ONLY_ENV);
    let debug_counters = env_flag(DEBUG_COUNTERS_ENV);

    let _ = SCHEDULER_STATE.get_or_init(|| match init_scheduler(
        false,
        stats_only,
        debug_counters,
        distributed_inactive_pool,
        initial_lock_budget,
    ) {
        Ok(state) => {
            if stats_only {
                info!(
                    "{SCHEDULER_NAME} stats-only env {} requested but ignored by minimal BPF controller",
                    STATS_ONLY_ENV
                );
                eprintln!(
                    "[accordin] stats-only env {} requested but ignored by minimal BPF controller",
                    STATS_ONLY_ENV
                );
            }
            if debug_counters {
                info!(
                    "{SCHEDULER_NAME} debug-counter env {} requested but ignored by minimal BPF controller",
                    DEBUG_COUNTERS_ENV
                );
                eprintln!(
                    "[accordin] debug-counter env {} requested but ignored by minimal BPF controller",
                    DEBUG_COUNTERS_ENV
                );
            }
            eprintln!("[accordin] eBPF scheduler loaded successfully");
            state
        }
        Err(e) => {
            eprintln!("[accordin] Failed to load eBPF scheduler: {:#}", e);
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
        lock_stats::print_process_stats("accordin");
    }
    fini
};

#[unsafe(no_mangle)]
pub extern "C" fn accordin_dynamic_cpu_affinity_is_stable() -> libc::c_int {
    i32::from(lock_stats::dynamic_cpu_affinity_is_stable())
}

#[unsafe(no_mangle)]
pub extern "C" fn accordin_dynamic_cpu_affinity_freeze() {
    lock_stats::dynamic_cpu_affinity_freeze();
}

#[unsafe(no_mangle)]
pub extern "C" fn accordin_dynamic_cpu_affinity_begin_measurement() {
    lock_stats::dynamic_cpu_affinity_begin_measurement_for_thread();
}

#[cfg(test)]
mod tests {
    use super::env_flag;

    #[test]
    fn env_flag_defaults_to_false() {
        let key = "ACCORDIN_TEST_ENV_FLAG_MISSING";
        unsafe {
            std::env::remove_var(key);
        }
        assert!(!env_flag(key));
    }

    #[test]
    fn env_flag_accepts_common_truthy_values() {
        let key = "ACCORDIN_TEST_ENV_FLAG_TRUTHY";
        for value in ["1", "true", "TRUE", "yes", "on"] {
            unsafe {
                std::env::set_var(key, value);
            }
            assert!(env_flag(key), "expected truthy value {value}");
        }
        unsafe {
            std::env::remove_var(key);
        }
    }
}
