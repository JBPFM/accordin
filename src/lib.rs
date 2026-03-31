// SPDX-License-Identifier: GPL-2.0-only
//
// 动态链接库入口，在加载时初始化 FlexGuard userspace-state BPF 程序。

mod bpf_skel;
pub use bpf_skel::*;
mod arch;
pub mod bpf_intf;
mod mcs_tas;
mod mutex_hook;

use std::mem::MaybeUninit;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use libbpf_rs::MapHandle;
use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use log::info;

const RUNTIME_NAME: &str = "lb_simple";
const DISABLE_BPF_ENV: &str = "LB_SIMPLE_DISABLE_BPF";

static BPF_STATE: OnceLock<BpfState> = OnceLock::new();

struct BpfState {
    _skel: Option<BpfSkel<'static>>,
}

// SAFETY: The skeleton is kept alive for process lifetime and only accessed through libbpf.
unsafe impl Send for BpfState {}
unsafe impl Sync for BpfState {}

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

fn init_bpf(debug: bool) -> Result<BpfState> {
    let mut skel_builder = BpfSkelBuilder::default();
    skel_builder.obj_builder.debug(debug);

    let open_object: &'static mut MaybeUninit<libbpf_rs::OpenObject> =
        Box::leak(Box::new(MaybeUninit::uninit()));

    let open_skel = skel_builder
        .open(open_object)
        .context("failed to open FlexGuard BPF skeleton")?;
    let mut skel = open_skel
        .load()
        .context("failed to load FlexGuard BPF skeleton")?;

    let nodes_map = MapHandle::try_from(&skel.maps.nodes_map)
        .context("failed to duplicate nodes_map handle")?;
    let bss = skel
        .maps
        .bss_data
        .as_mut()
        .context("FlexGuard BPF skeleton did not expose bss data")?;

    mcs_tas::install_bpf_runtime(
        bss.qnodes.as_mut_ptr(),
        std::ptr::addr_of_mut!(bss.num_preempted_holders),
        bss.preempted_flags.as_mut_ptr(),
    );
    mutex_hook::set_nodes_map(nodes_map);

    skel.attach()
        .context("failed to attach FlexGuard BPF skeleton")?;

    info!("{RUNTIME_NAME} FlexGuard userspace-state BPF loaded");
    Ok(BpfState { _skel: Some(skel) })
}

impl Drop for BpfState {
    fn drop(&mut self) {
        let _ = self._skel.take();
        info!("{RUNTIME_NAME} FlexGuard userspace-state BPF stopped");
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
            "{RUNTIME_NAME} FlexGuard userspace-state BPF disabled by env {}",
            DISABLE_BPF_ENV
        );
        eprintln!(
            "[lb_simple] FlexGuard userspace-state BPF disabled by {}",
            DISABLE_BPF_ENV
        );
        return;
    }

    let _ = BPF_STATE.get_or_init(|| match init_bpf(false) {
        Ok(state) => {
            eprintln!("[lb_simple] FlexGuard userspace-state BPF loaded successfully");
            state
        }
        Err(err) => {
            eprintln!("[lb_simple] Failed to load FlexGuard userspace-state BPF: {err:#}");
            panic!("FlexGuard userspace-state BPF initialization failed");
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

#[cfg(test)]
mod tests {
    fn compact(source: &str) -> String {
        source.split_whitespace().collect()
    }

    #[test]
    fn bpf_source_files_follow_flexguard_protocol() {
        let build_script = include_str!("../build.rs");
        let intf = include_str!("bpf/intf.h");
        let platform_defs = include_str!("bpf/platform_defs.h");
        let flexguard = include_str!("bpf/flexguard_bpf.h");
        let bpf = compact(include_str!("bpf/flexguard_userspace_state.bpf.c"));

        assert!(
            build_script.contains("src/bpf/flexguard_userspace_state.bpf.c"),
            "build.rs should generate the skeleton from the FlexGuard userspace-state BPF source",
        );
        assert!(
            intf.contains("#include \"flexguard_bpf.h\""),
            "bindgen bridge header should expose the shared FlexGuard protocol definitions",
        );
        assert!(
            platform_defs.contains("MAX_NUMBER_THREADS 1600"),
            "shared platform definitions should expose the FlexGuard thread-slot limit",
        );
        assert!(
            flexguard.contains("FLEXGUARD_CRITICAL_STATE_FRONT"),
            "shared FlexGuard header should expose the front-runner critical-state marker",
        );
        assert!(
            bpf.contains("flexguard_qnode_tqnodes[MAX_NUMBER_THREADS];"),
            "BPF program should export the shared qnode array in .bss",
        );
        assert!(
            bpf.contains("}nodes_mapSEC(\".maps\");"),
            "BPF program should expose nodes_map for tid-to-thread-index lookup",
        );
        assert!(
            bpf.contains("preempted_flags[MAX_NUMBER_THREADS]"),
            "BPF program should export per-thread preempted flags in .bss",
        );
        assert!(
            bpf.contains("num_preempted_holders"),
            "BPF program should track preempted holders separately from front-runner bypass",
        );
        assert!(
            bpf.contains("thread_index=*thread_id;"),
            "BPF program should cache the validated thread index before indexing shared .bss arrays",
        );
        assert!(
            bpf.contains("if(!preempted_flags[thread_index]&&flexguard_is_critical_state(state))"),
            "BPF program should decide preemption from the explicit userspace critical-state marker using the bounded thread index",
        );
        assert!(
            !bpf.contains("is_preempted_map"),
            "BPF program should not keep the legacy preempted map once userspace only consumes shared flags/counters",
        );
    }

    #[test]
    fn loader_uses_flexguard_bpf_runtime() {
        let lib = include_str!("lib.rs");
        let production = lib
            .split("#[cfg(test)]")
            .next()
            .expect("lib.rs should contain a test module split point");

        assert!(
            production.contains("mcs_tas::install_bpf_runtime"),
            "lib.rs should pass shared qnodes and preemption metadata into the Rust lock runtime",
        );
        assert!(
            production.contains("mutex_hook::set_nodes_map"),
            "lib.rs should publish nodes_map so mutex hooks can register thread indices",
        );
        assert!(
            production.contains(".attach()"),
            "lib.rs should attach the generated FlexGuard tracepoint skeleton",
        );
        assert!(
            !production.contains("use scx_utils::"),
            "lib.rs should no longer depend on sched_ext helper imports",
        );
        assert!(
            !production.contains("thread_ctx_addr_map"),
            "lib.rs should no longer depend on the removed thread_ctx_addr_map protocol",
        );
    }

    #[test]
    fn narrow_front_bypass_contract() {
        let lib = include_str!("lib.rs");
        let mcs_tas = include_str!("mcs_tas.rs");
        let flexguard = include_str!("bpf/flexguard_bpf.h");
        let bpf = compact(include_str!("bpf/flexguard_userspace_state.bpf.c"));

        assert!(
            flexguard.contains("FLEXGUARD_CRITICAL_STATE_FRONT"),
            "shared protocol should define a FRONT state for the lock front-runner",
        );
        assert!(
            !flexguard.contains("FLEXGUARD_CRITICAL_STATE_HANDOFF"),
            "broad handoff markers should be removed from the shared protocol",
        );
        assert!(
            mcs_tas.contains("front_runner"),
            "lock state should carry a per-lock front-runner signal instead of relying only on global blocking",
        );
        assert!(
            mcs_tas.contains("front_runner_blocked"),
            "lock slow path should expose a direct front-runner blocked predicate",
        );
        assert!(
            !mcs_tas.contains("fn blocking_condition()"),
            "global blocking should be narrowed and renamed away from the old broad condition",
        );
        assert!(
            bpf.contains("preempted_flags[MAX_NUMBER_THREADS]"),
            "BPF should export per-thread preempted state instead of only a global counter",
        );
        assert!(
            bpf.contains("num_preempted_holders"),
            "BPF should keep a holder-only global preemption counter",
        );
        assert!(
            lib.contains("bss.preempted_flags.as_mut_ptr()"),
            "loader should hand the shared preempted flags into the Rust runtime",
        );
        assert!(
            lib.contains("bss.num_preempted_holders"),
            "loader should hand the holder-preemption counter into the Rust runtime",
        );
    }

    #[test]
    fn mutex_hook_registers_thread_indices_in_nodes_map() {
        let hook = include_str!("mutex_hook.rs");

        assert!(
            hook.contains("set_nodes_map"),
            "mutex hook should accept the new nodes_map handle",
        );
        assert!(
            hook.contains("current_thread_index()"),
            "mutex hook should resolve a stable FlexGuard thread index before registration",
        );
        assert!(
            hook.contains("thread_index.to_ne_bytes()"),
            "mutex hook should publish thread indices to nodes_map",
        );
        assert!(
            !hook.contains("thread_ctx()"),
            "mutex hook should no longer export user-space context pointers to BPF",
        );
        assert!(
            hook.contains("NODES_MAP.get().is_none()"),
            "mutex hook should skip per-lock registration work entirely when BPF nodes_map is absent",
        );
        assert!(
            hook.contains("if r.get()"),
            "mutex hook should short-circuit repeated registration checks for already-registered threads",
        );
    }

    #[test]
    fn mcs_tas_uses_flexguard_critical_state_markers() {
        let mcs_tas = include_str!("mcs_tas.rs");

        assert!(
            mcs_tas.contains("mark_front_runner"),
            "lock slow path should mark the front-runner before phase2 acquisition",
        );
        assert!(
            mcs_tas.contains("mark_lock_holder"),
            "lock acquisition should mark the thread as an explicit FlexGuard lock holder",
        );
        assert!(
            mcs_tas.contains("clear_critical_state"),
            "unlock path should clear the FlexGuard critical-state marker",
        );
        assert!(
            mcs_tas.contains("holder_preempted()"),
            "lock slow path should consult the shared preempted-holder counter",
        );
        assert!(
            mcs_tas.contains("front_runner"),
            "lock slow path should keep a per-lock front-runner signal for preempted front waiters",
        );
        assert!(
            mcs_tas.contains("fn should_enqueue_mcs(&self) -> bool"),
            "lock slow path should centralize the MCS admission gate so holder-preempted fallback can skip queue churn early",
        );
        assert!(
            mcs_tas.contains("if holder_preempted() {\n            return false;"),
            "holder-preempted fallback should short-circuit MCS admission before the front-runner signal is consulted",
        );
        assert!(
            mcs_tas.contains("QNODE_NEXT_PARKED"),
            "lock slow path should reserve a parked sentinel in qnode.next for blocking-aware MCS exit",
        );
        assert!(
            mcs_tas.contains("link_successor"),
            "lock slow path should use sentinel-aware successor linking so parked predecessors can be woken safely",
        );
        assert!(
            mcs_tas.contains("mcs_exit_blocking"),
            "blocking conditions should retire already-enqueued MCS waiters through a dedicated safe-exit path",
        );
        assert!(
            mcs_tas.contains("fn front_runner_blocked(&self) -> bool"),
            "lock slow path should expose a direct front-runner blocked predicate",
        );
        assert!(
            mcs_tas.contains("if self.phase2_blocking() {\n                            break;"),
            "queued waiters should stop local MCS spinning once the blocking condition becomes active",
        );
        assert!(
            !mcs_tas.contains("queue_bypass"),
            "queue_bypass relay logic should be removed once front-runner signaling is direct",
        );
        assert!(
            mcs_tas.contains("FUTEX_WAIT_PRIVATE"),
            "lock slow path should block with futex when the BPF protocol requests it",
        );
    }
}
