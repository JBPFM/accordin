use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const CPU_MASK_K_ENV: &str = "ACCORDIN_CPU_MASK_K";
pub const CPU_MASK_K_SHORT_ENV: &str = "K";
pub const MAX_CPUS: usize = 256;
pub const ACTIVE_CPU_WORDS: usize = MAX_CPUS / 64;

#[derive(Debug)]
struct CpuAffinityConfig {
    env_name: &'static str,
    requested_cpus: usize,
    available_cpus: Vec<usize>,
    active_cpu_count: AtomicUsize,
}

#[derive(Debug)]
enum CpuAffinityState {
    Disabled,
    Configured(CpuAffinityConfig),
    Invalid(String),
}

static CPU_AFFINITY_STATE: OnceLock<CpuAffinityState> = OnceLock::new();
static BPF_SINK: OnceLock<Box<dyn BpfActiveCpusSink>> = OnceLock::new();

thread_local! {
    static CURRENT_THREAD_AFFINITY_APPLIED: Cell<bool> = const { Cell::new(false) };
}

pub trait BpfActiveCpusSink: Send + Sync {
    fn push(&self, wanted: &[u8; MAX_CPUS]) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicCpuAffinityUpdate {
    pub requested_cpus: usize,
    pub applied_cpus: usize,
    pub changed: bool,
}

pub fn init_from_env(label: &str) {
    match cpu_affinity_state() {
        CpuAffinityState::Disabled => {}
        CpuAffinityState::Invalid(error) => {
            eprintln!("[{label}] CPU affinity env ignored: {error}");
        }
        CpuAffinityState::Configured(config) => {
            if !should_apply_userspace_affinity(bpf_sink_registered()) {
                eprintln!(
                    "[{label}] BPF active CPU mask initially limited by {}={} to CPUs: {}",
                    config.env_name,
                    config.requested_cpus,
                    format_cpu_list(&active_cpus(config))
                );
                return;
            }

            match apply_current_configured_affinity(config) {
                Ok(()) => {
                    eprintln!(
                        "[{label}] CPU affinity initially limited by {}={} to CPUs: {}",
                        config.env_name,
                        config.requested_cpus,
                        format_cpu_list(&active_cpus(config))
                    );
                }
                Err(error) => {
                    eprintln!(
                        "[{label}] failed to apply CPU affinity from {}={}: {error}",
                        config.env_name, config.requested_cpus
                    );
                }
            }
        }
    }
}

pub fn ensure_current_thread_affinity() {
    if !should_apply_userspace_affinity(bpf_sink_registered()) {
        return;
    }

    if let CpuAffinityState::Configured(config) = cpu_affinity_state() {
        let _ = apply_current_thread_affinity_if_needed(config);
    }
}

pub fn set_bpf_sink(sink: Box<dyn BpfActiveCpusSink>) -> Result<(), String> {
    BPF_SINK
        .set(sink)
        .map_err(|_| "BPF active CPU sink already registered".to_string())
}

pub fn push_initial_mask_to_bpf() -> Result<(), String> {
    let CpuAffinityState::Configured(config) = cpu_affinity_state() else {
        return Ok(());
    };

    let wanted = build_active_cpu_bitmap(
        &config.available_cpus,
        config.active_cpu_count.load(Ordering::Acquire),
    )?;
    push_active_mask(&wanted)
}

pub fn configured_cpu_count_env_present() -> bool {
    configured_cpu_count_env().is_some()
}

pub fn update_dynamic_cpu_count(
    requested_cpus: usize,
) -> Result<Option<DynamicCpuAffinityUpdate>, String> {
    let requested_cpus = requested_cpus.max(1);
    let config = match cpu_affinity_state() {
        CpuAffinityState::Disabled => return Ok(None),
        CpuAffinityState::Invalid(error) => return Err(error.clone()),
        CpuAffinityState::Configured(config) => config,
    };

    let applied_cpus = requested_cpus.min(config.available_cpus.len()).max(1);
    let previous_cpus = config.active_cpu_count.swap(applied_cpus, Ordering::AcqRel);
    let changed = previous_cpus != applied_cpus;
    if changed {
        let wanted = build_active_cpu_bitmap(&config.available_cpus, applied_cpus)?;
        if let Err(error) = push_active_mask(&wanted) {
            config
                .active_cpu_count
                .store(previous_cpus, Ordering::Release);
            return Err(error);
        }
    }

    Ok(Some(DynamicCpuAffinityUpdate {
        requested_cpus,
        applied_cpus,
        changed,
    }))
}

pub fn current_dynamic_cpu_count() -> Option<usize> {
    match cpu_affinity_state() {
        CpuAffinityState::Configured(config) => {
            Some(config.active_cpu_count.load(Ordering::Acquire))
        }
        _ => None,
    }
}

fn cpu_affinity_state() -> &'static CpuAffinityState {
    CPU_AFFINITY_STATE.get_or_init(load_cpu_affinity_state)
}

fn load_cpu_affinity_state() -> CpuAffinityState {
    let Some((env_name, value)) = configured_cpu_count_env() else {
        return CpuAffinityState::Disabled;
    };

    let requested_cpus = match parse_requested_cpu_count(&value, env_name) {
        Ok(requested_cpus) => requested_cpus,
        Err(error) => return CpuAffinityState::Invalid(error),
    };

    let numa_cpus = match numa_ordered_cpus() {
        Ok(cpus) => cpus,
        Err(error) => return CpuAffinityState::Invalid(error),
    };
    let current_allowed_cpus = current_affinity_cpus().ok();
    let available_cpus = select_numa_available_cpus(&numa_cpus, current_allowed_cpus.as_deref());
    if available_cpus.len() < requested_cpus {
        return CpuAffinityState::Invalid(format!(
            "requested {requested_cpus} CPUs from NUMA-ordered available CPUs but only {} are available",
            available_cpus.len()
        ));
    }

    CpuAffinityState::Configured(CpuAffinityConfig {
        env_name,
        requested_cpus,
        available_cpus,
        active_cpu_count: AtomicUsize::new(requested_cpus),
    })
}

fn configured_cpu_count_env() -> Option<(&'static str, String)> {
    match std::env::var(CPU_MASK_K_ENV) {
        Ok(value) => Some((CPU_MASK_K_ENV, value)),
        Err(std::env::VarError::NotPresent) => match std::env::var(CPU_MASK_K_SHORT_ENV) {
            Ok(value) => Some((CPU_MASK_K_SHORT_ENV, value)),
            Err(_) => None,
        },
        Err(error) => Some((CPU_MASK_K_ENV, error.to_string())),
    }
}

fn parse_requested_cpu_count(value: &str, env_name: &str) -> Result<usize, String> {
    let trimmed = value.trim();
    let requested_cpus = trimmed
        .parse::<usize>()
        .map_err(|_| format!("{env_name} must be a positive integer, got {value:?}"))?;
    if requested_cpus == 0 {
        return Err(format!("{env_name} must be > 0"));
    }
    Ok(requested_cpus)
}

fn numa_ordered_cpus() -> Result<Vec<usize>, String> {
    let mut node_cpulists = numa_node_cpulist_paths("/sys/devices/system/node");
    node_cpulists.sort_by_key(|(node_id, _)| *node_id);

    let mut cpus = Vec::new();
    let mut seen = BTreeSet::new();
    for (_node_id, cpulist_path) in node_cpulists {
        for cpu in read_cpu_list_file(&cpulist_path)? {
            if seen.insert(cpu) {
                cpus.push(cpu);
            }
        }
    }

    if !cpus.is_empty() {
        return Ok(cpus);
    }

    read_cpu_list_file(Path::new("/sys/devices/system/cpu/online"))
        .map_err(|error| format!("failed to find NUMA node CPUs: {error}"))
}

fn numa_node_cpulist_paths(root: impl AsRef<Path>) -> Vec<(u32, PathBuf)> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let file_name = entry.file_name();
            let file_name = file_name.to_str()?;
            let node_id = file_name.strip_prefix("node")?.parse::<u32>().ok()?;
            Some((node_id, entry.path().join("cpulist")))
        })
        .collect()
}

fn read_cpu_list_file(path: &Path) -> Result<Vec<usize>, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    parse_cpu_list(&contents)
        .map_err(|error| format!("invalid CPU list in {}: {error}", path.display()))
}

fn parse_cpu_list(text: &str) -> Result<Vec<usize>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let mut cpus = BTreeSet::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("empty entry in {text:?}"));
        }

        if let Some((start, end)) = part.split_once('-') {
            let start = parse_cpu_id(start, text)?;
            let end = parse_cpu_id(end, text)?;
            if start > end {
                return Err(format!("invalid descending range {part:?}"));
            }
            for cpu in start..=end {
                cpus.insert(cpu);
            }
        } else {
            cpus.insert(parse_cpu_id(part, text)?);
        }
    }

    Ok(cpus.into_iter().collect())
}

fn parse_cpu_id(value: &str, source: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("invalid CPU id {value:?} in {source:?}"))
}

fn current_affinity_cpus() -> io::Result<Vec<usize>> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let ret =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut cpus = Vec::new();
    for cpu in 0..libc::CPU_SETSIZE as usize {
        if unsafe { libc::CPU_ISSET(cpu, &set) } {
            cpus.push(cpu);
        }
    }
    Ok(cpus)
}

#[cfg(test)]
fn select_numa_cpus(
    numa_cpus: &[usize],
    allowed_cpus: Option<&[usize]>,
    requested_cpus: usize,
) -> Result<Vec<usize>, String> {
    let cpus = select_numa_available_cpus(numa_cpus, allowed_cpus)
        .into_iter()
        .take(requested_cpus)
        .collect::<Vec<_>>();

    if cpus.len() < requested_cpus {
        return Err(format!(
            "requested {requested_cpus} CPUs from NUMA-ordered available CPUs but only {} are available",
            cpus.len()
        ));
    }

    Ok(cpus)
}

fn select_numa_available_cpus(numa_cpus: &[usize], allowed_cpus: Option<&[usize]>) -> Vec<usize> {
    let allowed: Option<BTreeSet<usize>> = allowed_cpus.map(|cpus| cpus.iter().copied().collect());
    numa_cpus
        .iter()
        .copied()
        .filter(|cpu| allowed.as_ref().is_none_or(|allowed| allowed.contains(cpu)))
        .collect()
}

fn active_cpus(config: &CpuAffinityConfig) -> Vec<usize> {
    let active_cpu_count = config.active_cpu_count.load(Ordering::Acquire);
    config
        .available_cpus
        .iter()
        .copied()
        .take(active_cpu_count)
        .collect()
}

fn initial_cpus(config: &CpuAffinityConfig) -> Vec<usize> {
    config
        .available_cpus
        .iter()
        .copied()
        .take(config.requested_cpus)
        .collect()
}

fn build_active_cpu_bitmap(
    available_cpus: &[usize],
    active_cpu_count: usize,
) -> Result<[u8; MAX_CPUS], String> {
    let mut wanted = [0; MAX_CPUS];
    for &cpu in available_cpus.iter().take(active_cpu_count) {
        if cpu >= MAX_CPUS {
            return Err(format!("CPU {cpu} is outside BPF MAX_CPUS {}", MAX_CPUS));
        }
        wanted[cpu] = 1;
    }
    Ok(wanted)
}

fn push_active_mask(wanted: &[u8; MAX_CPUS]) -> Result<(), String> {
    let Some(sink) = BPF_SINK.get() else {
        return Err("BPF active CPU sink is not registered".to_string());
    };
    sink.push(wanted)
}

fn bpf_sink_registered() -> bool {
    BPF_SINK.get().is_some()
}

fn should_apply_userspace_affinity(bpf_sink_registered: bool) -> bool {
    !bpf_sink_registered
}

fn apply_current_configured_affinity(config: &CpuAffinityConfig) -> Result<(), String> {
    apply_cpu_affinity(&initial_cpus(config))
}

fn apply_current_thread_affinity_if_needed(config: &CpuAffinityConfig) -> Result<bool, String> {
    CURRENT_THREAD_AFFINITY_APPLIED.with(|applied| {
        if applied.get() {
            return Ok(false);
        }

        let result = apply_current_configured_affinity(config);
        applied.set(true);
        result.map(|()| true)
    })
}

fn apply_cpu_affinity(cpus: &[usize]) -> Result<(), String> {
    apply_cpu_affinity_to_tid(0, cpus).map_err(|error| error.to_string())
}

fn apply_cpu_affinity_to_tid(tid: libc::pid_t, cpus: &[usize]) -> io::Result<()> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::CPU_ZERO(&mut set);
    }
    for &cpu in cpus {
        if cpu >= libc::CPU_SETSIZE as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "CPU {cpu} is outside libc CPU_SETSIZE {}",
                    libc::CPU_SETSIZE
                ),
            ));
        }
        unsafe {
            libc::CPU_SET(cpu, &mut set);
        }
    }

    let ret = unsafe { libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn format_cpu_list(cpus: &[usize]) -> String {
    cpus.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::{
        build_active_cpu_bitmap, format_cpu_list, parse_cpu_list, select_numa_available_cpus,
        select_numa_cpus, should_apply_userspace_affinity,
    };

    #[test]
    fn parse_cpu_list_accepts_singletons_and_ranges() {
        assert_eq!(
            parse_cpu_list("0,2,4-6,10\n").unwrap(),
            vec![0, 2, 4, 5, 6, 10]
        );
    }

    #[test]
    fn parse_cpu_list_deduplicates_and_sorts() {
        assert_eq!(parse_cpu_list("4,2,2,0-1").unwrap(), vec![0, 1, 2, 4]);
    }

    #[test]
    fn parse_cpu_list_rejects_descending_ranges() {
        assert!(parse_cpu_list("4-2").unwrap_err().contains("descending"));
    }

    #[test]
    fn select_numa_cpus_respects_current_affinity() {
        let numa_cpus = vec![0, 2, 4, 6, 8, 10, 1, 3, 5, 7, 9, 11];
        let allowed_cpus = vec![2, 6, 8, 1, 3, 12];

        assert_eq!(
            select_numa_cpus(&numa_cpus, Some(&allowed_cpus), 5).unwrap(),
            vec![2, 6, 8, 1, 3]
        );
    }

    #[test]
    fn select_numa_cpus_requires_enough_available_cpus() {
        let error = select_numa_cpus(&[0, 2], Some(&[2]), 2).unwrap_err();
        assert!(error.contains("requested 2 CPUs"));
    }

    #[test]
    fn select_numa_available_cpus_keeps_full_pool_across_nodes() {
        let numa_cpus = vec![0, 2, 4, 6, 8, 10, 1, 3, 5, 7, 9, 11];
        let allowed_cpus = vec![2, 6, 8, 1, 3, 12];

        assert_eq!(
            select_numa_available_cpus(&numa_cpus, Some(&allowed_cpus)),
            vec![2, 6, 8, 1, 3]
        );
    }

    #[test]
    fn select_numa_cpus_spills_to_later_nodes_after_earlier_nodes() {
        let numa_cpus = vec![0, 2, 4, 1, 3, 5];

        assert_eq!(
            select_numa_cpus(&numa_cpus, None, 5).unwrap(),
            vec![0, 2, 4, 1, 3]
        );
    }

    #[test]
    fn format_cpu_list_joins_selected_cpus() {
        assert_eq!(format_cpu_list(&[0, 2, 4]), "0,2,4");
    }

    #[test]
    fn build_active_cpu_bitmap_marks_first_available_cpus() {
        let bitmap = build_active_cpu_bitmap(&[2, 6, 8, 1], 3).unwrap();

        assert_eq!(bitmap[0], 0);
        assert_eq!(bitmap[1], 0);
        assert_eq!(bitmap[2], 1);
        assert_eq!(bitmap[6], 1);
        assert_eq!(bitmap[8], 1);
    }

    #[test]
    fn userspace_affinity_is_disabled_when_bpf_sink_is_registered() {
        assert!(should_apply_userspace_affinity(false));
        assert!(!should_apply_userspace_affinity(true));
    }
}
