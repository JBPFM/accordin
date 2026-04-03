fn main() {
    let mut builder = scx_cargo::BpfBuilder::new().unwrap();
    builder
        .enable_intf("src/bpf_intf.h", "bpf_intf.rs")
        .enable_skel("../bpf/main.bpf.c", "bpf")
        .add_source("../bpf/flexguard.bpf.c");
    builder.compile_link_gen().unwrap();
}
