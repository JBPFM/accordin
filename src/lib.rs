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
        std::ptr::addr_of_mut!(bss.num_preempted_cs),
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
            flexguard.contains("FLEXGUARD_CRITICAL_STATE_HANDOFF"),
            "shared FlexGuard header should expose the handoff critical-state marker",
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
            bpf.contains("if(flexguard_is_critical_state(qnode->cs_counter))"),
            "BPF program should decide preemption from the explicit userspace critical-state marker",
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
            "lib.rs should pass shared qnodes and num_preempted_cs into the Rust lock runtime",
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
    }

    #[test]
    fn mcs_tas_uses_flexguard_critical_state_markers() {
        let mcs_tas = include_str!("mcs_tas.rs");

        assert!(
            mcs_tas.contains("mark_handoff_thread"),
            "lock slow path should mark the handoff owner before phase2 acquisition",
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
            mcs_tas.contains("blocking_condition()"),
            "lock slow path should consult the shared preempted-critical-section counter",
        );
        assert!(
            mcs_tas.contains("FUTEX_WAIT_PRIVATE"),
            "lock slow path should block with futex when the BPF protocol requests it",
        );
    }
}
