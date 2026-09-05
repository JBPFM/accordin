# scx C headers

Unmodified C support headers from sched-ext/scx commit
`813343a62994ed6b2350fd3467aa3cb476169926`, distributed with `scx_utils 1.0.22`.
Only the headers needed by this scheduler are included, with their original
copyright and SPDX license notices. The generated enum headers are pinned too.

Source: https://github.com/sched-ext/scx/tree/813343a62994ed6b2350fd3467aa3cb476169926/scheds/include/scx

These are the same scx headers used before the C migration. Builds use system
libbpf, Clang and bpftool directly; neither Cargo nor a Rust installation is
required. `vmlinux.h` is generated from `VMLINUX_BTF` at build time.
