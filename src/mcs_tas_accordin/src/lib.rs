mod bpf_skel;
pub use bpf_skel::*;

pub use accordin_shared::arch;
pub use accordin_shared::env::env_flag;
pub use accordin_shared::lock_backend;
pub use accordin_shared::lock_stats;
#[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
pub mod bpf_intf {
    include!(concat!(env!("OUT_DIR"), "/bpf_intf.rs"));
}
mod mcs_tas;
mod mutex_hook;

accordin_shared::define_scheduler_loader!(
    scheduler_name = "mcs_tas_accordin",
    env_prefix = "MCS_TAS_ACCORDIN",
);
