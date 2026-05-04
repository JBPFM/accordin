mod bpf_skel;
pub use bpf_skel::*;

pub use accordin_shared::admission;
pub use accordin_shared::arch;
pub use accordin_shared::env::env_flag;
pub use accordin_shared::lock_backend;
pub use accordin_shared::lock_stats;
#[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
pub mod bpf_intf {
    include!(concat!(env!("OUT_DIR"), "/bpf_intf.rs"));
}
mod mutex_hook;
mod reciprocating;

accordin_shared::define_scheduler_loader!(
    scheduler_name = "reciprocating_accordin",
    env_prefix = "RECIPROCATING_ACCORDIN",
);
