// SPDX-License-Identifier: GPL-2.0-only
//
// 动态链接库入口，在加载时初始化 eBPF 调度器

mod bpf_skel;
pub use bpf_skel::*;
mod arch;
pub mod bpf_intf;
mod mcs_tas;
mod mutex_hook;

use std::mem::MaybeUninit;
use std::sync::OnceLock;

use anyhow::Result;
use libbpf_rs::Link;
use libbpf_rs::MapCore;
use libbpf_rs::MapFlags;
use libbpf_rs::MapHandle;
use libbpf_rs::OpenObject;
use log::info;
use scx_utils::scx_ops_attach;
use scx_utils::scx_ops_load;
use scx_utils::scx_ops_open;

const SCHEDULER_NAME: &str = "lb_simple";
const DISABLE_BPF_ENV: &str = "LB_SIMPLE_DISABLE_BPF";
const STATS_ONLY_ENV: &str = "LB_SIMPLE_STATS_ONLY";
const SSC_CPU_CAP: usize = bpf_intf::MAX_CPUS as usize;

// 全局状态，保持 eBPF 程序和 OpenObject 的生命周期
static SCHEDULER_STATE: OnceLock<SchedulerState> = OnceLock::new();

struct SchedulerState {
    // Keep link and loaded skel alive for the entire process lifetime.
    _link: Option<Link>,
    _skel: Option<BpfSkel<'static>>,
}

// SAFETY: BpfSkel/Link are internally thread-safe for this usage.
unsafe impl Send for SchedulerState {}
unsafe impl Sync for SchedulerState {}

#[derive(Debug, Default)]
struct NumaTopology {
    cpu_to_node: Vec<(u32, u32)>,
    dominant_node: i32,
    local_cpu_count: i64,
    remote_cpu_count: i64,
    first_socket_node: i32,
    first_socket_cpus: Vec<u32>,
}

fn parse_cpu_list(cpulist: &str) -> Vec<u32> {
    let mut cpus = Vec::new();

    for range_str in cpulist.trim().split(',') {
        let range_str = range_str.trim();
        if range_str.is_empty() {
            continue;
        }

        if let Some((start_s, end_s)) = range_str.split_once('-') {
            let start: u32 = match start_s.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let end: u32 = match end_s.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            if start > end {
                continue;
            }
            for cpu in start..=end {
                cpus.push(cpu);
            }
        } else if let Ok(cpu) = range_str.parse::<u32>() {
            cpus.push(cpu);
        }
    }

    cpus
}

fn build_numa_topology_from_nodes(mut node_cpus: Vec<(u32, Vec<u32>)>) -> NumaTopology {
    node_cpus.retain(|(_, cpus)| !cpus.is_empty());
    node_cpus.sort_by_key(|(node_id, _)| *node_id);

    if node_cpus.is_empty() {
        let fallback = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
            .max(1);
        let fallback_cpus: Vec<u32> = (0..fallback).collect();

        return NumaTopology {
            cpu_to_node: fallback_cpus.iter().copied().map(|cpu| (cpu, 0)).collect(),
            dominant_node: 0,
            local_cpu_count: i64::from(fallback),
            remote_cpu_count: 0,
            first_socket_node: 0,
            first_socket_cpus: fallback_cpus,
        };
    }

    let first_socket_node = node_cpus[0].0;
    let first_socket_cpus = node_cpus[0].1.clone();
    let mut cpu_to_node = Vec::new();
    let mut node_counts = Vec::new();

    for (node_id, cpus) in node_cpus {
        node_counts.push((node_id, cpus.len() as i64));
        for cpu in cpus {
            cpu_to_node.push((cpu, node_id));
        }
    }

    node_counts.sort_by_key(|(node_id, count)| (-*count, *node_id as i64));
    let (dominant_node, local_cpu_count) = node_counts[0];
    let total_cpu_count: i64 = node_counts.iter().map(|(_, count)| *count).sum();

    NumaTopology {
        cpu_to_node,
        dominant_node: dominant_node as i32,
        local_cpu_count: local_cpu_count.max(1),
        remote_cpu_count: (total_cpu_count - local_cpu_count).max(0),
        first_socket_node: first_socket_node as i32,
        first_socket_cpus,
    }
}

fn detect_numa_topology() -> NumaTopology {
    let node_base = "/sys/devices/system/node";
    let entries = match std::fs::read_dir(node_base) {
        Ok(e) => e,
        Err(_) => return build_numa_topology_from_nodes(Vec::new()),
    };

    let mut node_cpus = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("node") {
            continue;
        }
        let node_id: u32 = match name_str[4..].parse() {
            Ok(n) => n,
            Err(_) => continue,
        };

        let cpulist_path = format!("{}/{}/cpulist", node_base, name_str);
        let cpulist = match std::fs::read_to_string(&cpulist_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let cpus = parse_cpu_list(&cpulist);
        if cpus.is_empty() {
            continue;
        }

        node_cpus.push((node_id, cpus));
    }

    build_numa_topology_from_nodes(node_cpus)
}

fn configure_scheduler_topology(_skel: &mut OpenBpfSkel<'_>, _topology: &NumaTopology) {}

fn publish_ssc_cpu_topology(
    ssc_cpu_count: &mut u32,
    ssc_cpu_list: &mut [u32; SSC_CPU_CAP],
    ssc_cpu_rank: &mut [u16; SSC_CPU_CAP],
    topology: &NumaTopology,
) {
    ssc_cpu_list.fill(0);
    ssc_cpu_rank.fill(SSC_CPU_CAP as u16);

    let mut count = 0usize;
    for cpu in topology.first_socket_cpus.iter().copied() {
        let cpu_idx = cpu as usize;
        if cpu_idx >= SSC_CPU_CAP || count >= SSC_CPU_CAP {
            continue;
        }

        ssc_cpu_list[count] = cpu;
        ssc_cpu_rank[cpu_idx] = count as u16;
        count += 1;
    }
    *ssc_cpu_count = count as u32;
}

fn initial_ssc_active_count(ssc_cpu_count: u32) -> u32 {
    ssc_cpu_count.clamp(2, 8)
}

/// Populate cpu_to_node BPF map and publish NUMA defaults.
fn publish_scheduler_topology(skel: &mut BpfSkel<'_>, topology: &NumaTopology, stats_only: bool) {
    for (cpu, node_id) in &topology.cpu_to_node {
        let _ =
            skel.maps
                .cpu_to_node
                .update(&cpu.to_ne_bytes(), &node_id.to_ne_bytes(), MapFlags::ANY);
    }

    if let Some(bss) = skel.maps.bss_data.as_mut() {
        bss.dominant_node = topology.dominant_node;
        bss.stats_only_mode = u32::from(stats_only);
        publish_ssc_cpu_topology(
            &mut bss.ssc_cpu_count,
            &mut bss.ssc_cpu_list,
            &mut bss.ssc_cpu_rank,
            topology,
        );
    }

    if let Some(data) = skel.maps.data_data.as_mut() {
        data.ssc_active_count = initial_ssc_active_count(
            u32::try_from(topology.first_socket_cpus.len()).unwrap_or(u32::MAX),
        );
    }

    info!(
        "lb_simple topology initialized: dominant_node={} local_cpus={} remote_cpus={} first_socket_node={} ssc_cpu_count={} stats_only={}",
        topology.dominant_node,
        topology.local_cpu_count,
        topology.remote_cpu_count,
        topology.first_socket_node,
        topology.first_socket_cpus.len(),
        stats_only
    );
}

fn init_scheduler(debug: bool, stats_only: bool) -> Result<SchedulerState> {
    let topology = detect_numa_topology();
    let mut skel_builder = BpfSkelBuilder::default();
    skel_builder.obj_builder.debug(debug);

    // 使用 Box::leak 来保持 OpenObject 的生命周期
    let open_object: &'static mut MaybeUninit<OpenObject> =
        Box::leak(Box::new(MaybeUninit::uninit()));

    // Open the BPF skeleton
    let mut skel = scx_ops_open!(skel_builder, open_object, lb_simple_ops, None)?;
    configure_scheduler_topology(&mut skel, &topology);

    // NOTE: SWITCH_PARTIAL is intentionally NOT set.
    // All tasks go through sched_ext so that BPF callbacks can still see every
    // runnable thread. Admission accounting inside BPF is restricted to threads
    // that registered thread_ctx_addr_map entries, so unrelated system tasks do
    // not dilute lb_simple's wait-ratio feedback loop.

    // Load the BPF program
    let mut skel = scx_ops_load!(skel, lb_simple_ops, uei)?;

    // Duplicate the map handle so mutex hooks can use libbpf helpers directly.
    let thread_ctx_map = MapHandle::try_from(&skel.maps.thread_ctx_addr_map)?;
    mutex_hook::set_thread_ctx_map(thread_ctx_map);

    // Publish cpu_to_node map and NUMA defaults after load.
    publish_scheduler_topology(&mut skel, &topology, stats_only);

    // Attach the scheduler
    let link = scx_ops_attach!(skel, lb_simple_ops)?;

    info!("{SCHEDULER_NAME} scheduler started via LD_PRELOAD");
    Ok(SchedulerState {
        _link: Some(link),
        _skel: Some(skel),
    })
}

impl Drop for SchedulerState {
    fn drop(&mut self) {
        // Drop link first, then skeleton.
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

/// 初始化 eBPF 调度器
fn init_ebpf() {
    if cfg!(test) {
        return;
    }

    // 初始化日志
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
        eprintln!("[lb_simple] eBPF scheduler disabled by {}", DISABLE_BPF_ENV);
        return;
    }

    let stats_only = env_flag(STATS_ONLY_ENV);

    // 初始化调度器（只执行一次）
    let _ = SCHEDULER_STATE.get_or_init(|| match init_scheduler(false, stats_only) {
        Ok(state) => {
            if stats_only {
                info!(
                    "{SCHEDULER_NAME} scheduler running in stats-only mode via env {}",
                    STATS_ONLY_ENV
                );
                eprintln!(
                    "[lb_simple] eBPF scheduler stats-only mode enabled by {}",
                    STATS_ONLY_ENV
                );
            }
            eprintln!("[lb_simple] eBPF scheduler loaded successfully");
            state
        }
        Err(e) => {
            eprintln!("[lb_simple] Failed to load eBPF scheduler: {:#}", e);
            panic!("eBPF initialization failed");
        }
    });
}

// 库加载时的构造函数
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
    use super::*;

    fn compact(source: &str) -> String {
        source.split_whitespace().collect()
    }

    #[test]
    fn bpf_headers_do_not_keep_wait_sample_scaling_residue() {
        let intf = include_str!("bpf/intf.h");
        let stats = include_str!("bpf/stats.bpf.h");
        let compact_stats = compact(stats);

        assert!(
            !intf.contains("WAIT_TIME_SAMPLE_STRIDE"),
            "BPF interface should not keep wait sample stride after sampling removal",
        );
        assert!(
            !intf.contains("cumulative sampled wait time"),
            "BPF interface comment should describe raw cumulative wait time",
        );
        assert!(
            !stats.contains("scale_sampled_wait_ns"),
            "BPF stats should not scale wait deltas after sampling removal",
        );
        assert!(
            !stats.contains("8x sample scaling"),
            "BPF stats comments should not mention removed wait sampling",
        );
        assert!(
            compact_stats.contains("completed_wait=uctx.wait_ns_total-tc->last_wait_ns;"),
            "BPF stats should derive completed wait directly from the raw cumulative delta",
        );
        assert!(
            compact_stats.contains("wait_delta+=completed_wait-tc->pending_wait_ns;"),
            "BPF stats should subtract already-accounted pending wait instead of scaling samples",
        );
        assert!(
            compact_stats.contains("wait_delta+=pending_wait-tc->pending_wait_ns;"),
            "BPF stats should accumulate in-flight wait without any sample scaling residue",
        );
    }

    #[test]
    fn ssc_topology_keeps_first_socket_cpu_list_even_when_not_dominant() {
        let topology = build_numa_topology_from_nodes(vec![
            (1, vec![8, 9, 10, 11]),
            (0, vec![0, 2]),
            (2, vec![12]),
        ]);

        assert_eq!(topology.first_socket_node, 0);
        assert_eq!(topology.first_socket_cpus, vec![0, 2]);
        assert_eq!(topology.dominant_node, 1);
        assert_eq!(topology.local_cpu_count, 4);
        assert_eq!(topology.remote_cpu_count, 3);
    }

    #[test]
    fn initial_ssc_active_count_uses_a_warm_start_seed() {
        assert_eq!(initial_ssc_active_count(0), 2);
        assert_eq!(initial_ssc_active_count(1), 2);
        assert_eq!(initial_ssc_active_count(4), 4);
        assert_eq!(initial_ssc_active_count(8), 8);
        assert_eq!(initial_ssc_active_count(20), 8);
    }

    #[test]
    fn bpf_headers_define_ssc_cpu_globals_and_helper() {
        let maps = include_str!("bpf/maps.bpf.h");
        let admission = include_str!("bpf/admission.bpf.h");

        assert!(
            maps.contains("ssc_cpu_count"),
            "BPF globals should expose SSC CPU count",
        );
        assert!(
            maps.contains("ssc_cpu_list"),
            "BPF globals should expose SSC CPU list",
        );
        assert!(
            maps.contains("ssc_active_count"),
            "BPF globals should expose active SSC CPU count",
        );
        assert!(
            maps.contains("ssc_cpu_rank"),
            "BPF globals should expose SSC CPU rank table",
        );
        assert!(
            admission.contains("get_ssc_cpu_by_index"),
            "BPF helpers should expose indexed SSC CPU lookup",
        );
    }

    #[test]
    fn ssc_helper_headers_define_task_membership_helpers() {
        let admission = include_str!("bpf/admission.bpf.h");

        assert!(
            admission.contains("is_cpu_ssc_core"),
            "BPF helpers should expose CPU-level SSC-core membership checks",
        );
        assert!(
            admission.contains("is_task_on_ssc_core"),
            "BPF helpers should expose task-level SSC-core membership checks",
        );
        assert!(
            admission.contains("ssc_cpu_rank[cpu]"),
            "CPU-level SSC-core checks should read per-CPU SSC rank",
        );
        assert!(
            admission.contains("rank < ssc_active_count"),
            "CPU-level SSC-core checks should clamp membership by active count",
        );
    }

    #[test]
    fn ssc_vote_headers_define_epoch_and_score_state() {
        let intf = include_str!("bpf/intf.h");
        let maps = include_str!("bpf/maps.bpf.h");
        let stats = include_str!("bpf/stats.bpf.h");

        assert!(
            intf.contains("struct ssc_vote_slot"),
            "BPF interface should define per-SSC-core vote slots",
        );
        assert!(
            intf.contains("last_unlock_count"),
            "BPF interface should snapshot per-slot unlock counters",
        );
        assert!(
            maps.contains("ssc_vote_epoch"),
            "BPF globals should expose the current SSC vote epoch",
        );
        assert!(
            maps.contains("ssc_vote_window_ns"),
            "BPF globals should expose the SSC vote timeout window",
        );
        assert!(
            maps.contains("ssc_vote_publish_count"),
            "BPF globals should expose the SSC vote publish count",
        );
        assert!(
            maps.contains("ssc_vote_sum_unlock_count"),
            "BPF globals should expose the aggregated vote-window unlock count",
        );
        assert!(
            maps.contains("ssc_vote_last_effective_score"),
            "BPF globals should expose the last effective SSC score",
        );
        assert!(
            maps.contains("ssc_vote_consec_grow"),
            "BPF globals should track consecutive score growth",
        );
        assert!(
            stats.contains("estimate_ssc_window_unlocks"),
            "BPF stats helpers should estimate vote-window unlock throughput",
        );
        assert!(
            stats.contains("rotate_ssc_vote_window"),
            "BPF stats helpers should rotate stale SSC vote epochs",
        );
        assert!(
            maps.contains("ssc_vote_slot_map"),
            "BPF maps should expose per-core SSC vote slots",
        );
    }

    #[test]
    fn unlock_control_headers_define_state() {
        let intf = include_str!("bpf/intf.h");
        let maps = include_str!("bpf/maps.bpf.h");
        let stats = include_str!("bpf/stats.bpf.h");

        assert!(
            intf.contains("enum ssc_search_phase"),
            "BPF interface should define the SSC search phase enum",
        );
        assert!(
            intf.contains("unlock_count"),
            "BPF interface should expose userspace unlock counters",
        );
        assert!(
            intf.contains("last_unlock_count"),
            "BPF task state should snapshot the last observed unlock counter",
        );
        assert!(
            intf.contains("unlock_count_window"),
            "BPF task state should retain the current window's unlock count",
        );
        assert!(
            maps.contains("ssc_search_phase"),
            "BPF globals should expose the current SSC search phase",
        );
        assert!(
            maps.contains("ssc_refine_low"),
            "BPF globals should expose the lower refinement bound",
        );
        assert!(
            maps.contains("ssc_refine_high"),
            "BPF globals should expose the upper refinement bound",
        );
        assert!(
            maps.contains("ssc_vote_sum_unlock_count"),
            "BPF globals should aggregate unlock counts in the current vote window",
        );
        assert!(
            !maps.contains("ssc_wait_ratio_ewma"),
            "BPF globals should no longer expose wait-ratio EWMA state",
        );
        assert!(
            !maps.contains("ssc_shift_streak"),
            "BPF globals should no longer track wait-ratio shift streaks",
        );
        assert!(
            !maps.contains("ssc_resize_holdoff"),
            "BPF globals should no longer keep wait-ratio resize holdoff state",
        );
        assert!(
            stats.contains("SSC_UNLOCK_GATE_THRESHOLD"),
            "BPF stats helpers should define a fixed unlock gate threshold",
        );
        assert!(
            !stats.contains("detect_ssc_workload_shift"),
            "BPF stats helpers should no longer expose workload-shift detection",
        );
    }

    #[test]
    fn tick_quorum_logic_requires_majority_before_adjusting_active_count() {
        let main = compact(include_str!("bpf/main.bpf.c"));

        assert!(
            main.contains("publish_ssc_core_vote(tc,p,now);"),
            "simple_tick should publish SSC-core samples into the vote state",
        );
        assert!(
            main.contains("ssc_vote_publish_count*2>ssc_active_count"),
            "simple_tick should require a strict majority quorum",
        );
        assert!(
            main.contains("ssc_vote_consec_grow>=2"),
            "simple_tick should only double active_count after two consecutive increases",
        );
        assert!(
            main.contains("ssc_vote_consec_shrink>=2"),
            "simple_tick should only halve active_count after two consecutive drops below the effective score",
        );
        assert!(
            main.contains("ssc_set_active_count(ssc_active_count<<1,score);"),
            "simple_tick should funnel doubled active_count through the clamped resize helper",
        );
        assert!(
            main.contains("ssc_set_active_count(ssc_next_refine_target(),ssc_best_score);"),
            "simple_tick should funnel refinement targets through the clamped resize helper",
        );
    }

    #[test]
    fn quorum_refine_logic_keeps_op_count_search_without_shift_detection() {
        let main = compact(include_str!("bpf/main.bpf.c"));
        let stats = include_str!("bpf/stats.bpf.h");

        assert!(
            !main.contains("detect_ssc_workload_shift"),
            "simple_tick should not run wait-ratio workload-shift detection anymore",
        );
        assert!(
            stats.contains("ssc_enter_refine_mode"),
            "BPF stats helpers should expose refine-mode entry",
        );
        assert!(
            stats.contains("ssc_next_refine_target"),
            "BPF stats helpers should expose bounded refinement target selection",
        );
        assert!(
            main.contains("ssc_enter_refine_mode(ssc_best_count,ssc_active_count,score);"),
            "simple_tick should enter bounded refinement after a clear regression",
        );
        assert!(
            !main.contains("ssc_search_phase=SSC_SEARCH_SEEK;"),
            "simple_tick should not reset back to seek mode through the removed shift detector",
        );
        assert!(
            !stats.contains("demand_shift"),
            "controller logic should still avoid demand-based shift signals",
        );
    }

    #[test]
    fn ssc_active_count_clamp_keeps_a_floor_of_two() {
        let stats = compact(include_str!("bpf/stats.bpf.h"));

        assert!(
            stats.contains("if(active_count<2)active_count=2;"),
            "SSC active-count clamp should keep a minimum of 2",
        );
        assert!(
            !stats.contains("if(ssc_cpu_count<2)returnssc_cpu_count;"),
            "SSC active-count clamp should not drop below 2 when CPU count is smaller",
        );
    }

    #[test]
    fn stats_headers_do_not_use_probe_write_user_in_struct_ops_path() {
        let intf = include_str!("bpf/intf.h");
        let stats = include_str!("bpf/stats.bpf.h");

        assert!(
            intf.contains("pending_wait_ns"),
            "task context should keep pending wait accounting in BPF state",
        );
        assert!(
            intf.contains("unlock_count_window"),
            "task context should keep per-window unlock accounting in BPF state",
        );
        assert!(
            !stats.contains("bpf_probe_write_user"),
            "struct_ops stats path must not use bpf_probe_write_user",
        );
    }

    #[test]
    fn rust_sources_remove_timeslice_extension_support() {
        let arch = include_str!("arch.rs");
        let mcs_tas = include_str!("mcs_tas.rs");
        let mutex_hook = include_str!("mutex_hook.rs");
        let build_script = include_str!("../build.rs");

        assert!(
            !std::path::Path::new("src/timeslice_extension.rs").exists(),
            "timeslice extension module file should be removed",
        );
        assert!(
            !mcs_tas.contains("timeslice_extension"),
            "mcs_tas should not depend on the removed timeslice extension module",
        );
        assert!(
            !mcs_tas.contains("prepare_thread_timeslice"),
            "mcs_tas should not keep thread preparation wrappers for the removed timeslice extension",
        );
        assert!(
            !mutex_hook.contains("prepare_thread_timeslice"),
            "mutex hook registration should not call removed timeslice preparation",
        );
        assert!(
            !build_script.contains("lb_simple_tse_available"),
            "build script should not keep cfg plumbing for removed timeslice extension support",
        );
        assert!(
            !arch.contains("pub fn compiler_barrier()"),
            "arch helpers should not keep the compiler barrier used only by the removed timeslice extension",
        );
    }

    #[test]
    fn rust_sources_use_tsc_conversion_for_wait_timestamps() {
        let arch = include_str!("arch.rs");
        let mcs_tas = include_str!("mcs_tas.rs");

        assert!(
            arch.contains("pub fn wait_time_to_ns("),
            "arch helpers should expose direct wait-time cycle to ns conversion",
        );
        assert!(
            !mcs_tas.contains("wait_time_now_ns()"),
            "mcs_tas slow path should not call wait_time_now_ns directly",
        );
    }

    #[test]
    fn unlock_count_control_path_replaces_wait_ratio_gate() {
        let intf = include_str!("bpf/intf.h");
        let maps = include_str!("bpf/maps.bpf.h");
        let stats = include_str!("bpf/stats.bpf.h");
        let main = compact(include_str!("bpf/main.bpf.c"));
        let mcs_tas = compact(include_str!("mcs_tas.rs"));

        assert!(
            intf.contains("unlock_count"),
            "shared thread context should export unlock counters",
        );
        assert!(
            maps.contains("ssc_vote_sum_unlock_count"),
            "BPF globals should retain the vote-window unlock aggregate",
        );
        assert!(
            stats.contains("estimate_ssc_window_unlocks"),
            "BPF stats should estimate unlock throughput from the current vote window",
        );
        assert!(
            compact(stats).contains("returnestimate_ssc_window_unlocks(active_count)*SSC_SCORE_SCALE;"),
            "SSC score should derive from the vote-window unlock estimate",
        );
        assert!(
            stats.contains("#define SSC_UNLOCK_GATE_THRESHOLD"),
            "BPF stats should define a fixed unlock gate threshold",
        );
        assert!(
            main.contains("__u64unlock_estimate=estimate_ssc_window_unlocks(ssc_active_count);"),
            "simple_tick should derive the admission signal from the global SSC vote window",
        );
        assert!(
            main.contains("if(unlock_estimate<SSC_UNLOCK_GATE_THRESHOLD){tc->admitted=0;p->scx.slice=0;}else{tc->admitted=1;}"),
            "simple_tick should gate admission on the fixed unlock threshold and restore admission above it",
        );
        assert!(
            !main.contains("tc->run_ns_window/10<tc->wait_ns_window"),
            "simple_tick should no longer gate admission on per-task wait ratio",
        );
        assert!(
            mcs_tas.contains("(*thread_ctx()).unlock_count+=1;"),
            "userspace lock path should increment unlock_count on unlock",
        );
        assert!(
            !stats.contains("useful_run"),
            "BPF stats should no longer keep useful-run scoring helpers",
        );
    }
}
