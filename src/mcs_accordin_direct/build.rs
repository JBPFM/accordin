fn main() {
    scx_cargo::BpfBuilder::new()
        .unwrap()
        .enable_intf("../bpf/intf.h", "bpf_intf.rs")
        .enable_skel("../bpf/main.bpf.c", "bpf")
        .build()
        .unwrap();
}
