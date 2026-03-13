// Copyright (c) Andrea Righi <andrea.righi@linux.dev>
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

fn main() {
    scx_cargo::BpfBuilder::new()
        .unwrap()
        .enable_intf("src/bpf/intf.h", "bpf_intf.rs")
        .enable_skel("src/bpf/main.bpf.c", "bpf")
        .build()
        .unwrap();

    // Declare the custom cfg so rustc doesn't warn about it.
    println!("cargo::rustc-check-cfg=cfg(lb_simple_tse_available)");

    // Enable timeslice extension on x86_64 Linux with glibc.
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_arch == "x86_64" && target_os == "linux" && target_env == "gnu" {
        println!("cargo::rustc-cfg=lb_simple_tse_available");
    }
}
