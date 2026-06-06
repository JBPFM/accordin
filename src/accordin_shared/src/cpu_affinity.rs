pub const MAX_CPUS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicCpuAffinityUpdate {
    pub requested_cpus: usize,
    pub applied_cpus: usize,
    pub changed: bool,
}

pub fn init_from_env(_label: &str) {}

pub fn ensure_current_thread_affinity() {}

pub fn configured_cpu_count_env_present() -> bool {
    false
}

pub fn update_dynamic_cpu_count(
    _requested_cpus: usize,
) -> Result<Option<DynamicCpuAffinityUpdate>, String> {
    Ok(None)
}

pub fn current_dynamic_cpu_count() -> Option<usize> {
    None
}

pub fn current_first_numa_available_cpu_count() -> Option<usize> {
    None
}

pub fn current_available_cpu_count() -> Option<usize> {
    None
}
