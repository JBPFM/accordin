#!/bin/bash
# Script to update .clangd configuration with the latest build directory

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

find_latest_build_out_dir() {
    { find "$SCRIPT_DIR/target/debug/build" -path '*/out/scx_utils-bpf_h' -type d -printf '%T@ %p\n' 2>/dev/null || true; } \
        | sort -rn \
        | awk 'NR == 1 { print $2 }'
}

detect_target_arch_define() {
    case "$(uname -m)" in
        x86_64|amd64)
            echo "__TARGET_ARCH_x86"
            ;;
        aarch64|arm64)
            echo "__TARGET_ARCH_arm64"
            ;;
        armv7*|armv6*|arm)
            echo "__TARGET_ARCH_arm"
            ;;
        riscv64*)
            echo "__TARGET_ARCH_riscv"
            ;;
        s390x)
            echo "__TARGET_ARCH_s390"
            ;;
        ppc64le|ppc64)
            echo "__TARGET_ARCH_powerpc"
            ;;
        mips64*|mips*)
            echo "__TARGET_ARCH_mips"
            ;;
        *)
            echo "Error: unsupported architecture: $(uname -m)" >&2
            exit 1
            ;;
    esac
}

detect_multiarch_include_dir() {
    local multiarch=""

    if command -v cc >/dev/null 2>&1; then
        multiarch="$(cc -print-multiarch 2>/dev/null || true)"
    fi

    if [ -n "$multiarch" ] && [ -d "/usr/include/$multiarch" ]; then
        echo "/usr/include/$multiarch"
    fi
}

BUILD_OUT_DIR="$(find_latest_build_out_dir)"
if [ -z "$BUILD_OUT_DIR" ]; then
    echo "Error: No generated scx_utils BPF headers found. Please run 'cargo build' first." >&2
    exit 1
fi
BUILD_OUT_DIR="$(realpath "$BUILD_OUT_DIR")"

TARGET_ARCH_DEFINE="$(detect_target_arch_define)"
MULTIARCH_INCLUDE_DIR="$(detect_multiarch_include_dir)"

BPF_FLAGS=(
    "--target=bpf"
    "-I$SCRIPT_DIR/src/bpf"
    "-I/usr/local/include"
    "-I/usr/include"
    "-I$BUILD_OUT_DIR"
    "-D__BPF__"
    "-D__BPF_TRACING__"
    "-D$TARGET_ARCH_DEFINE"
    "-Wno-unknown-attributes"
    "-Wno-visibility"
    "-Wno-address-of-packed-member"
    "-Wno-compare-distinct-pointer-types"
    "-Wno-gnu-variable-sized-type-not-at-end"
    "-Wno-pointer-sign"
    "-Wno-pragma-once-outside-header"
    "-Wno-unused-value"
)

if [ -n "$MULTIARCH_INCLUDE_DIR" ]; then
    BPF_FLAGS=("-I$MULTIARCH_INCLUDE_DIR" "${BPF_FLAGS[@]}")
fi

{
    cat <<EOF
If:
  PathMatch: ^src/bpf/.*$
CompileFlags:
  Compiler: clang
  Add:
EOF
    for flag in "${BPF_FLAGS[@]}"; do
        printf '    - %s\n' "$flag"
    done
    cat <<'EOF'
  Remove:
    - -msse*
    - -march*

Diagnostics:
  ClangTidy:
    Remove:
      - bugprone-*
      - modernize-*
      - readability-*
      - google-*
      - cppcoreguidelines-*

Index:
  Background: Build
EOF
} > .clangd

echo "Successfully updated .clangd configuration"
echo "Using generated headers from: $BUILD_OUT_DIR"
