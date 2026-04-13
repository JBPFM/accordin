#!/bin/bash
# Generate compile_commands.json for all supported BPF targets

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

declare -A BUILD_OUT_DIRS=()
declare -A BUILD_OUT_TIMESTAMPS=()

json_escape() {
    local value="$1"
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//$'\n'/\\n}
    printf '%s' "$value"
}

collect_latest_build_out_dirs() {
    local profile build_root out_dir rel_path build_dir crate_name key mtime

    for profile in debug release; do
        build_root="$SCRIPT_DIR/target/$profile/build"
        [ -d "$build_root" ] || continue

        while IFS= read -r -d '' out_dir; do
            rel_path="${out_dir#$build_root/}"
            build_dir="${rel_path%%/*}"
            crate_name="${build_dir%-*}"
            key="$profile:$crate_name"
            mtime="$(stat -c %Y "$out_dir")"

            if [ -z "${BUILD_OUT_TIMESTAMPS[$key]:-}" ] || [ "$mtime" -gt "${BUILD_OUT_TIMESTAMPS[$key]}" ]; then
                BUILD_OUT_TIMESTAMPS["$key"]="$mtime"
                BUILD_OUT_DIRS["$key"]="$(realpath "$out_dir")"
            fi
        done < <(find "$build_root" -path '*/out/scx_utils-bpf_h' -type d -print0 2>/dev/null || true)
    done
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

emit_sources_for_target() {
    case "$1" in
        mcs_simple|mcs_tas_simple)
            printf '%s\n' "$SCRIPT_DIR/src/bpf/main.bpf.c"
            ;;
        libflexguard|flexguard_simple)
            printf '%s\n' \
                "$SCRIPT_DIR/src/bpf/main.bpf.c" \
                "$SCRIPT_DIR/src/bpf/flexguard.bpf.c"
            ;;
        *)
            return 1
            ;;
    esac
}

display_target_name() {
    case "$1" in
        libflexguard)
            printf '%s' "flexguard_simple"
            ;;
        *)
            printf '%s' "$1"
            ;;
    esac
}

collect_latest_build_out_dirs
if [ "${#BUILD_OUT_DIRS[@]}" -eq 0 ]; then
    echo "Error: No generated scx_utils BPF headers found. Please run 'cargo build' first." >&2
    exit 1
fi

TARGET_ARCH_DEFINE="$(detect_target_arch_define)"
MULTIARCH_INCLUDE_DIR="$(detect_multiarch_include_dir)"
CLANG_BIN="$(command -v clang || echo clang)"

COMMON_BPF_FLAGS=(
    "--target=bpf"
    "-I$SCRIPT_DIR/src/bpf"
    "-I/usr/local/include"
    "-I/usr/include"
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
    COMMON_BPF_FLAGS=("-I$MULTIARCH_INCLUDE_DIR" "${COMMON_BPF_FLAGS[@]}")
fi

declare -a TRACKED_ENTRIES=()
entry_count=0
recognized_target_count=0
tmp_compile_commands="$(mktemp "$SCRIPT_DIR/compile_commands.json.tmp.XXXXXX")"
trap 'rm -f "$tmp_compile_commands"' EXIT

{
    printf '[\n'
    first_entry=1

    while IFS= read -r key; do
        profile="${key%%:*}"
        crate_name="${key#*:}"
        build_out_dir="${BUILD_OUT_DIRS[$key]}"

        if ! mapfile -t bpf_sources < <(emit_sources_for_target "$crate_name"); then
            echo "Warning: skipping unsupported BPF target '$crate_name' from $build_out_dir" >&2
            continue
        fi

        recognized_target_count=$((recognized_target_count + 1))

        for bpf_source in "${bpf_sources[@]}"; do
            if [ ! -f "$bpf_source" ]; then
                echo "Error: expected BPF source not found for target '$crate_name': $bpf_source" >&2
                exit 1
            fi

            bpf_flags=("${COMMON_BPF_FLAGS[@]}" "-I$build_out_dir")
            command="$(printf '%q ' "$CLANG_BIN" "${bpf_flags[@]}" -c "$bpf_source")"
            command="${command% }"

            if [ "$first_entry" -eq 0 ]; then
                printf ',\n'
            fi
            first_entry=0

            printf '  {\n'
            printf '    "directory": "%s",\n' "$(json_escape "$SCRIPT_DIR")"
            printf '    "command": "%s",\n' "$(json_escape "$command")"
            printf '    "file": "%s"\n' "$(json_escape "$bpf_source")"
            printf '  }'

            TRACKED_ENTRIES+=("$(display_target_name "$crate_name")/$profile -> $(basename "$bpf_source")")
            entry_count=$((entry_count + 1))
        done
    done < <(printf '%s\n' "${!BUILD_OUT_DIRS[@]}" | sort)

    printf '\n]\n'
} > "$tmp_compile_commands"

if [ "$recognized_target_count" -eq 0 ]; then
    echo "Error: Found generated scx_utils headers, but none matched the supported BPF targets." >&2
    exit 1
fi

if [ "$entry_count" -eq 0 ]; then
    echo "Error: No compile command entries were generated." >&2
    exit 1
fi

mv "$tmp_compile_commands" compile_commands.json
trap - EXIT

echo "Successfully generated compile_commands.json"
for tracked_entry in "${TRACKED_ENTRIES[@]}"; do
    echo "Tracked: $tracked_entry"
done
