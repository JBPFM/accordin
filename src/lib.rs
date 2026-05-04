// SPDX-License-Identifier: GPL-2.0-only
//
// 动态链接库入口，在加载时初始化 eBPF 调度器

mod bpf_skel;
pub use bpf_skel::*;
pub mod bpf_intf;
pub use accordin_shared::arch;
pub use accordin_shared::cpu_affinity;
pub use accordin_shared::env::env_flag;
pub use accordin_shared::lock_backend;
pub use accordin_shared::lock_stats;
#[path = "mcs_tas_accordin/src/mcs_tas.rs"]
mod mcs_tas;
mod mutex_hook;

type AccordinRawLock = mcs_tas::McsTasLockRaw;

accordin_shared::define_scheduler_loader!(
    scheduler_name = "accordin",
    env_prefix = "ACCORDIN",
);
