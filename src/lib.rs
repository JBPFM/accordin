// SPDX-License-Identifier: GPL-2.0-only
//
// 动态链接库入口，在加载时初始化 eBPF 调度器

mod bpf_skel;
pub use bpf_skel::*;
mod arch;
pub mod bpf_intf;
mod lock_backend;
mod lock_stats;
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
const DEBUG_COUNTERS_ENV: &str = "LB_SIMPLE_DEBUG_COUNTERS";
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
fn publish_scheduler_topology(
    skel: &mut BpfSkel<'_>,
    topology: &NumaTopology,
    stats_only: bool,
    debug_counters: bool,
) {
    for (cpu, node_id) in &topology.cpu_to_node {
        let _ =
            skel.maps
                .cpu_to_node
                .update(&cpu.to_ne_bytes(), &node_id.to_ne_bytes(), MapFlags::ANY);
    }

    if let Some(bss) = skel.maps.bss_data.as_mut() {
        bss.dominant_node = topology.dominant_node;
        bss.stats_only_mode = u32::from(stats_only);
        bss.dbg_counters_enabled = u32::from(debug_counters);
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

fn init_scheduler(debug: bool, stats_only: bool, debug_counters: bool) -> Result<SchedulerState> {
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
    publish_scheduler_topology(&mut skel, &topology, stats_only, debug_counters);

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
    let debug_counters = env_flag(DEBUG_COUNTERS_ENV);

    // 初始化调度器（只执行一次）
    let _ =
        SCHEDULER_STATE.get_or_init(|| match init_scheduler(false, stats_only, debug_counters) {
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
    fn compact(source: &str) -> String {
        source.split_whitespace().collect()
    }

    #[test]
    fn mcs_tas_simple_replaces_lb_simple_target_name() {
        let cargo = include_str!("../Cargo.toml");
        let target_manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcs_tas_simple/Cargo.toml"),
        )
        .unwrap_or_default();
        let readme = include_str!("../bench/mutexbench/README.md");
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
    fn tick_publishes_current_ssc_sample_before_elapsed_window_quorum_evaluation() {
        let main = compact(include_str!("bpf/main.bpf.c"));

        assert!(
            main.contains("publish_ssc_core_vote(tc,p,now);if(ssc_vote_epoch&&ssc_vote_publish_count*2>ssc_active_count&&now>ssc_vote_start_ns&&now-ssc_vote_start_ns>=ssc_vote_window_ns){"),
            "simple_tick should publish the current SSC-core sample before evaluating an elapsed quorum window",
        );
    }

    #[test]
    fn tick_quorum_logic_requires_majority_before_adjusting_active_count() {
        let main = compact(include_str!("bpf/main.bpf.c"));
        let maps = include_str!("bpf/maps.bpf.h");
        let stats = compact(include_str!("bpf/stats.bpf.h"));

        assert!(
            main.contains("if(ssc_vote_epoch&&ssc_vote_publish_count*2>ssc_active_count&&now>ssc_vote_start_ns&&now-ssc_vote_start_ns>=ssc_vote_window_ns){"),
            "simple_tick should evaluate a completed vote window only after quorum and elapsed window time",
        );
        assert!(
            stats.contains("SSC_MIN_BOOTSTRAP_UNLOCKS"),
            "stats helpers should define a minimum unlock threshold for the first effective controller decision",
        );
        assert!(
            stats.contains("SSC_MIN_BOOTSTRAP_WINDOWS"),
            "stats helpers should define how many mature windows are required before controller bootstrap completes",
        );
        assert!(
            main.contains("if(!ssc_best_score&&ssc_vote_sum_unlock_count<SSC_MIN_BOOTSTRAP_UNLOCKS){ssc_bootstrap_mature_windows=0;rotate_ssc_vote_window(now);}elseif(!ssc_best_score&&++ssc_bootstrap_mature_windows<SSC_MIN_BOOTSTRAP_WINDOWS){rotate_ssc_vote_window(now);}else{__u64score=compute_ssc_vote_score(ssc_active_count);"),
            "simple_tick should require multiple mature windows before the first score-driven controller state update",
        );
        assert!(
            main.contains("if(!ssc_best_score){ssc_best_score=ssc_vote_sum_unlock_count*SSC_SCORE_SCALE;ssc_best_count=ssc_active_count;reset_ssc_refine_bounds(ssc_active_count);ssc_pending_capped_grow=1;seeded_best=true;}"),
            "bootstrap should seed the first best_score from a conservative raw unlock signal and arm the one-shot capped-grow state",
        );
        assert!(
            main.contains("rotate_ssc_vote_window(now);publish_ssc_core_vote(tc,p,now);"),
            "simple_tick should rotate to a fresh window before publishing the current SSC-core sample",
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
            maps.contains("ssc_pending_capped_grow"),
            "BPF globals should expose an explicit one-shot state for the post-bootstrap capped grow",
        );
        assert!(
            main.contains("if(!ssc_best_score){ssc_best_score=ssc_vote_sum_unlock_count*SSC_SCORE_SCALE;ssc_best_count=ssc_active_count;reset_ssc_refine_bounds(ssc_active_count);ssc_pending_capped_grow=1;seeded_best=true;}"),
            "bootstrap should arm the explicit one-shot capped-grow state when it seeds the first best score",
        );
        assert!(
            main.contains("booluse_capped_grow=ssc_pending_capped_grow&&ssc_active_count>=8;ssc_pending_capped_grow=0;")
                && main.contains("__u32grow_target=ssc_active_count<<1;if(use_capped_grow)grow_target=ssc_active_count+(ssc_active_count>>1);"),
            "simple_tick should consume the explicit one-shot state on the first actual grow decision and only cap that grow when the current width is at least 8",
        );
        assert!(
            main.contains("ssc_set_active_count(grow_target,score);"),
            "post-bootstrap capped growth should still flow through the clamped resize helper",
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
            main.contains("ssc_enter_refine_mode(ssc_best_count,ssc_active_count,score);"),
            "simple_tick should still enter bounded refinement after a clear regression",
        );
        assert!(
            !main.contains("detect_ssc_workload_shift"),
            "simple_tick should not reset back to seek through the removed workload-shift detector",
        );
        assert!(
            !stats.contains("demand_shift"),
            "controller logic should still avoid demand-based shift signals",
        );
    }

    #[test]
    fn ssc_resize_helper_refreshes_effective_score_on_noop_resize() {
        let stats = compact(include_str!("bpf/stats.bpf.h"));

        assert!(
            stats.contains("if(active_count==ssc_active_count){"),
            "ssc_set_active_count should keep an explicit no-op resize branch",
        );
        assert!(
            stats.contains("ssc_note_resize(effective_score);return;}"),
            "ssc_set_active_count should refresh the effective-score anchor before returning from a no-op resize",
        );
    }

    #[test]
    fn best_score_decay_headers_define_candidate_tracking() {
        let maps = include_str!("bpf/maps.bpf.h");
        let stats = compact(include_str!("bpf/stats.bpf.h"));

        assert!(
            maps.contains("ssc_best_candidate_count"),
            "BPF globals should track which SSC width is waiting for best-score promotion",
        );
        assert!(
            maps.contains("ssc_best_candidate_streak"),
            "BPF globals should track how many mature windows have confirmed the candidate width",
        );
        assert!(
            stats.contains("SSC_BEST_SCORE_DECAY_SHIFT"),
            "stats helpers should define the per-window best-score decay rate",
        );
        assert!(
            stats.contains("SSC_BEST_CONFIRM_WINDOWS"),
            "stats helpers should define how many mature windows confirm a new best anchor",
        );
        assert!(
            stats.contains("static__always_inlinevoidssc_reset_best_candidate(void){ssc_best_candidate_count=0;ssc_best_candidate_streak=0;}"),
            "stats helpers should expose a helper that clears pending best-score promotion state",
        );
        assert!(
            stats.contains("static__always_inline__u64ssc_decay_best_score(void){"),
            "stats helpers should expose a helper that decays the historical best score once per mature window",
        );
        assert!(
            stats.contains("static__always_inlinevoidssc_maybe_promote_best_candidate(__u32active_count,__u64score,__u64compare_best_score){"),
            "stats helpers should expose a helper that requires repeated mature windows before rewriting the best anchor",
        );
        assert!(
            stats.contains("ssc_reset_best_candidate();ssc_vote_last_score=0;ssc_vote_last_effective_score=effective_score;"),
            "resize bookkeeping should clear any pending best-score candidate before refreshing the effective-score anchor",
        );
    }

    #[test]
    fn best_score_decay_quorum_logic_requires_decay_and_two_window_confirmation() {
        let main = compact(include_str!("bpf/main.bpf.c"));

        assert!(
            main.contains("compare_best_score=seeded_best?ssc_best_score:ssc_decay_best_score();ssc_maybe_promote_best_candidate(ssc_active_count,score,compare_best_score);"),
            "simple_tick should decay the best anchor before routing mature-window scores through the two-window promotion helper",
        );
        assert!(
            main.contains("if(score>=compare_best_score){if(ssc_active_count>ssc_refine_low)ssc_refine_low=ssc_active_count;}elseif(ssc_active_count>ssc_refine_low){ssc_refine_high=ssc_active_count;}"),
            "refine-mode bounds should compare against the decayed best anchor instead of a permanent historical peak",
        );
        assert!(
            !main.contains("if(score>ssc_best_score){ssc_best_score=score;ssc_best_count=ssc_active_count;}"),
            "a single mature window should no longer overwrite the best anchor immediately",
        );
        assert!(
            !main.contains("if(score>=ssc_best_score){ssc_best_score=score;ssc_best_count=ssc_active_count;"),
            "refine-mode should no longer rewrite the best anchor inline on the first qualifying window",
        );
        assert!(
            !main.contains("ssc_best_count=ssc_active_count;ssc_best_score=score;ssc_set_active_count(grow_target,score);"),
            "seek-mode growth should stop overwriting the best anchor just because the controller is about to grow",
        );
    }

    #[test]
    fn refine_bad_steady_state_rebases_within_refine() {
        let main = compact(include_str!("bpf/main.bpf.c"));
        let stats = compact(include_str!("bpf/stats.bpf.h"));

        assert!(
            stats.contains("SSC_REFINE_BAD_STEADY_RATIO_NUM"),
            "stats helpers should define the score-gap ratio for bad steady-state detection",
        );
        assert!(
            stats.contains("SSC_REFINE_BAD_STEADY_RATIO_DEN"),
            "stats helpers should define the denominator for bad steady-state detection",
        );
        assert!(
            stats.contains("SSC_REFINE_BAD_STEADY_WINDOWS"),
            "stats helpers should define how many shrink windows are needed before rebasing",
        );
        assert!(
            main.contains("if(ssc_refine_low==ssc_refine_high&&next_target==ssc_active_count&&ssc_best_score&&score*SSC_REFINE_BAD_STEADY_RATIO_DEN<ssc_best_score*SSC_REFINE_BAD_STEADY_RATIO_NUM&&ssc_vote_consec_shrink>=SSC_REFINE_BAD_STEADY_WINDOWS){"),
            "controller should only trigger bad steady-state handling when single-point no-op refine is also a persistent score regression",
        );
        assert!(
            main.contains("reset_ssc_refine_bounds(ssc_active_count);ssc_note_resize(score);"),
            "bad steady-state refine should rebase anchors around the current score without reopening range or rewriting best anchors",
        );
        assert!(
            !main.contains("ssc_search_phase=SSC_SEARCH_SEEK;"),
            "bad steady-state handling should not jump back to SEEK",
        );
    }

    #[test]
    fn collapsed_refine_target_clamps_stale_best_anchor_within_current_interval() {
        let stats = compact(include_str!("bpf/stats.bpf.h"));

        assert!(
            stats.contains("static__always_inline__u32ssc_clamp_refine_target(__u32target){target=clamp_ssc_active_count(target);if(target<ssc_refine_low)returnssc_refine_low;if(target>ssc_refine_high)returnssc_refine_high;returntarget;}"),
            "stats helpers should expose a refine-target clamp that keeps chosen anchors inside the current interval",
        );
        assert!(
            stats.contains("if(ssc_refine_high<=ssc_refine_low+1)returnssc_clamp_refine_target(ssc_best_count?ssc_best_count:ssc_refine_low);"),
            "collapsed refine target selection should clamp the chosen anchor back into the current refine interval",
        );
        assert!(
            !stats.contains("if(ssc_refine_high<=ssc_refine_low+1)returnssc_best_count?ssc_best_count:ssc_refine_low;"),
            "collapsed refine target selection should not return a stale best anchor without interval clamping",
        );
    }

    #[test]
    fn capped_grow_step_uses_explicit_one_shot_post_bootstrap_state() {
        let maps = include_str!("bpf/maps.bpf.h");
        let main = compact(include_str!("bpf/main.bpf.c"));

        assert!(
            maps.contains("ssc_pending_capped_grow"),
            "BPF globals should expose an explicit one-shot state for the post-bootstrap capped grow",
        );
        assert!(
            main.contains("if(!ssc_best_score){ssc_best_score=ssc_vote_sum_unlock_count*SSC_SCORE_SCALE;ssc_best_count=ssc_active_count;reset_ssc_refine_bounds(ssc_active_count);ssc_pending_capped_grow=1;seeded_best=true;}"),
            "bootstrap should arm the explicit one-shot capped-grow state when it seeds the first best score",
        );
        assert!(
            main.contains("booluse_capped_grow=ssc_pending_capped_grow&&ssc_active_count>=8;ssc_pending_capped_grow=0;")
                && main.contains("__u32grow_target=ssc_active_count<<1;if(use_capped_grow)grow_target=ssc_active_count+(ssc_active_count>>1);"),
            "simple_tick should consume the explicit one-shot state on the first actual grow decision and only cap that grow when the current width is at least 8",
        );
        assert!(
            !main.contains("booluse_capped_grow=ssc_bootstrap_mature_windows<=SSC_MIN_BOOTSTRAP_WINDOWS+2;")
                && !main.contains("if(ssc_best_score&&ssc_bootstrap_mature_windows<SSC_MIN_BOOTSTRAP_WINDOWS+3)ssc_bootstrap_mature_windows++;")
                && !main.contains("if(ssc_bootstrap_mature_windows==SSC_MIN_BOOTSTRAP_WINDOWS&&ssc_active_count>=8)grow_target=ssc_active_count+(ssc_active_count>>1);"),
            "simple_tick should stop tying capped-grow lifetime to bootstrap mature-window counting after bootstrap",
        );
    }

    #[test]
    fn debug_counter_headers_define_refine_state_probes() {
        let maps = include_str!("bpf/maps.bpf.h");
        let stats = compact(include_str!("bpf/stats.bpf.h"));
        let main = compact(include_str!("bpf/main.bpf.c"));
        let admission = compact(include_str!("bpf/admission.bpf.h"));

        assert!(
            maps.contains("dbg_refine_entries"),
            "BPF globals should expose a counter for refine-mode entries",
        );
        assert!(
            maps.contains("dbg_refine_single_point"),
            "BPF globals should expose a counter for refine intervals that collapse to a single point",
        );
        assert!(
            maps.contains("dbg_refine_noop_targets"),
            "BPF globals should expose a counter for refine targets that do not move active_count",
        );
        assert!(
            maps.contains("dbg_noop_resizes"),
            "BPF globals should expose a counter for helper resizes that clamp back to the current width",
        );
        assert!(
            maps.contains("dbg_active_count_changes"),
            "BPF globals should expose a counter for real active-count changes",
        );
        assert!(
            maps.contains("dbg_bad_steady_rebases"),
            "BPF globals should expose a counter for bad steady-state rebase events",
        );
        assert!(
            maps.contains("dbg_task_ctx_creates"),
            "BPF globals should expose a counter for successful task_ctx creations",
        );
        assert!(
            maps.contains("dbg_task_ctx_misses"),
            "BPF globals should expose a counter for tasks that miss userspace ctx registration",
        );
        assert!(
            maps.contains("dbg_grow_uses_capped_step"),
            "BPF globals should expose a counter for growth decisions that used the capped post-bootstrap step",
        );
        assert!(
            maps.contains("dbg_last_grow_target"),
            "BPF globals should expose the most recent seek-growth target width",
        );
        assert!(
            stats.contains("if(dbg_counters_enabled)dbg_noop_resizes++;"),
            "resize helper should count no-op resize attempts when debug counters are enabled",
        );
        assert!(
            stats.contains("if(dbg_counters_enabled)dbg_active_count_changes++;"),
            "resize helper should count real active-count changes when debug counters are enabled",
        );
        assert!(
            admission.contains("if(dbg_counters_enabled)dbg_task_ctx_creates++;"),
            "task ctx creation path should count successful userspace ctx attachments when debug counters are enabled",
        );
        assert!(
            admission.contains("if(dbg_counters_enabled)dbg_task_ctx_misses++;"),
            "task ctx creation path should count tasks that have no registered userspace ctx when debug counters are enabled",
        );
        assert!(
            main.contains("if(dbg_counters_enabled){dbg_last_grow_target=grow_target;"),
            "grow path should record the most recent grow target",
        );
        assert!(
            main.contains("if(grow_target!=(ssc_active_count<<1))dbg_grow_uses_capped_step++;"),
            "grow path should count when it used the capped post-bootstrap step",
        );
        assert!(
            main.contains("if(dbg_counters_enabled)dbg_refine_entries++;ssc_enter_refine_mode(ssc_best_count,ssc_active_count,score);"),
            "shrink path should count refine-mode entries before switching into refinement",
        );
        assert!(
            main.contains("if(dbg_counters_enabled&&ssc_refine_low==ssc_refine_high)dbg_refine_single_point++;"),
            "controller should count refinement intervals that collapse to a single point",
        );
        assert!(
            main.contains("if(ssc_refine_low==ssc_refine_high&&next_target==ssc_active_count&&ssc_best_score&&score*SSC_REFINE_BAD_STEADY_RATIO_DEN<ssc_best_score*SSC_REFINE_BAD_STEADY_RATIO_NUM&&ssc_vote_consec_shrink>=SSC_REFINE_BAD_STEADY_WINDOWS){if(dbg_counters_enabled){dbg_refine_noop_targets++;dbg_bad_steady_rebases++;}"),
            "controller should count bad steady-state rebase events when single-point no-op refine qualifies for local rebasing",
        );
        assert!(
            main.contains("}elseif(dbg_counters_enabled){dbg_refine_noop_targets++;}"),
            "controller should also count non-single-point refine targets that still do not move the active width",
        );
    }

    #[test]
    fn rust_sources_can_enable_bpf_debug_counters() {
        let lib = include_str!("lib.rs");

        assert!(
            lib.contains("const DEBUG_COUNTERS_ENV: &str = \"LB_SIMPLE_DEBUG_COUNTERS\";"),
            "Rust sources should define an env var for enabling BPF debug counters",
        );
        assert!(
            lib.contains("let debug_counters = env_flag(DEBUG_COUNTERS_ENV);"),
            "scheduler init should read the debug-counter env flag",
        );
        assert!(
            lib.contains("bss.dbg_counters_enabled = u32::from(debug_counters);"),
            "scheduler setup should publish the debug-counter toggle into BPF globals",
        );
        assert!(
            lib.contains("init_scheduler(false, stats_only, debug_counters)"),
            "library init should pass the debug-counter toggle into scheduler setup",
        );
    }

    #[test]
    fn benchmark_scripts_preserve_lb_simple_debug_counter_env() {
>>>>>>> holdmetrics
        let multi = include_str!("../bench/mutexbench/scripts/sweep_mutex_throughput_multi_lock.sh");
        let run_example = include_str!("../bench/mutexbench/run_example.sh");

        assert!(cargo.contains("src/mcs_tas_simple"));
        assert!(target_manifest.contains("name = \"mcs_tas_simple\""));
        assert!(target_manifest.contains("[lib]\nname = \"mcs_tas_simple\""));
        assert!(!readme.contains("`lb_simple`（通过 `LD_PRELOAD=target/release/liblb_simple.so`）"));
        assert!(readme.contains("mcs_tas_simple"));
        assert!(readme.contains("MCS_TAS_SIMPLE_DISABLE_BPF"));
        assert!(!multi.contains("lb_simple_no_bpf"));
        assert!(multi.contains("mcs_tas_simple"));
        assert!(multi.contains("resolve_mcs_tas_simple_lib_path"));
        assert!(multi.contains("$PROJECT_ROOT/target/release/libmcs_tas_simple.so"));
        assert!(run_example.contains("mcs_tas_simple"));
    }

    #[test]
    fn user_visible_logs_use_new_target_names() {
        let mcs_tas_simple = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcs_tas_simple/src/lib.rs"),
        )
        .unwrap_or_default();
        let flexguard = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/libflexguard/src/lib.rs"),
        )
        .unwrap_or_default();

        assert!(mcs_tas_simple.contains("[mcs_tas_simple] eBPF scheduler loaded successfully"));
        assert!(!mcs_tas_simple.contains("[lb_simple] eBPF scheduler loaded successfully"));
        assert!(flexguard.contains("const SCHEDULER_NAME: &str = \"flexguard_simple\";"));
        assert!(flexguard.contains("[flexguard_simple] eBPF scheduler loaded successfully"));
        assert!(!flexguard.contains("[lb_simple] eBPF scheduler loaded successfully"));
    }
}
