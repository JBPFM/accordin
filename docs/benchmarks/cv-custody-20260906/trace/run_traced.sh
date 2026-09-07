#!/usr/bin/env bash
# One traced LevelDB fillrandom run of the condvar-custody build, with the same
# host contract as ../run.py: exclusive flock on the shared benchmark lock,
# sched_ext idle before the run, affinity over the online CPUs, the worktree's
# LiTL adapter preloaded against the worktree's direct library, and every
# ACCORDIN_/MCS_/SCX_/LD_/COND_VAR variable scrubbed from the environment first.
#
# Adds what the plain runner does not have: a bpftrace window taken after the
# run has settled, a sampler of the scheduler's admission slots, cpuidle
# residency accounting across the run, and an option to hold the deep C state
# out of the picture.
#
# Root is required (bpftrace, bpftool, cpuidle writes).
set -u -o pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SRC=/mnt/data/home/jz/accordin-simplify/target/cv-custody-20260906/custody-src
EXE=/mnt/data/home/jz/accordin-m0/target/flexguard-suite-20260905/leveldb/out-static/db_bench
EXE_SHA=f958f932acbffe73bba697e3e19898141d78c6486f06dc9830896b171b81a96d
SEED=/tmp/accordin-flexguard-suite-20260905/seed
LOCK=/tmp/mutexbench-sweep-multi-lock.lock
ADAPTER=$SRC/third_party/litl/lib/libmcstasaccordin_original.so
LIBDIR=$SRC/target/release
BPFOBJ=$SRC/target/release/accordin.bpf.o
THREADS=192

arm=custody
pre=0
trace=1
script=cv_regime
no_c6=0
out=""
time_ms=30000
warmup_ms=10000
trace_ms=5000
sample=1

usage() {
  cat <<'EOF'
usage: run_traced.sh [options]
  --arm custody|custody-off   custody-off sets ACCORDIN_CV_CUSTODY=0 (default custody)
  --pre                       run one readrandom of the same arm immediately before
  --script cv_regime|cv_sched which bpftrace script to attach (default cv_regime)
  --no-trace                  skip the bpftrace window entirely
  --no-sample                 skip the admission-slot sampler
  --no-c6                     disable cpuidle state3 (C6) on the online CPUs for the run
  --time-ms N                 db_bench --time_ms for the measured run (default 30000)
  --warmup-ms N               delay before attaching bpftrace (default 10000)
  --trace-ms N                bpftrace window length (default 5000)
  --out DIR                   output directory (default target/cv-custody-20260906/trace-<stamp>)
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --arm) arm=$2; shift 2 ;;
    --pre) pre=1; shift ;;
    --script) script=$2; shift 2 ;;
    --no-trace) trace=0; shift ;;
    --no-sample) sample=0; shift ;;
    --no-c6) no_c6=1; shift ;;
    --time-ms) time_ms=$2; shift 2 ;;
    --warmup-ms) warmup_ms=$2; shift 2 ;;
    --trace-ms) trace_ms=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option $1" >&2; usage; exit 2 ;;
  esac
done

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 2; }
case "$arm" in custody|custody-off) ;; *) echo "unknown arm $arm" >&2; exit 2 ;; esac
[ -f "$HERE/$script.bt" ] || { echo "no script $HERE/$script.bt" >&2; exit 2; }

stamp=$(date -u +%Y%m%dT%H%M%SZ)
[ -n "$out" ] || out=/mnt/data/home/jz/accordin-simplify/target/cv-custody-20260906/trace-$arm-$stamp
mkdir -p "$out"

log() { echo "$*" | tee -a "$out/run.log"; }

for f in "$EXE" "$ADAPTER" "$LIBDIR/libmcs_tas_accordin_direct.so" "$BPFOBJ"; do
  [ -f "$f" ] || { echo "missing $f" >&2; exit 2; }
done
have=$(sha256sum "$EXE" | cut -d' ' -f1)
[ "$have" = "$EXE_SHA" ] || log "WARNING db_bench sha256 $have != $EXE_SHA"
[ "$pre" = 0 ] || [ -d "$SEED" ] || { echo "missing seed $SEED" >&2; exit 2; }

online=$(cat /sys/devices/system/cpu/online)
cpu_list=$(echo "$online" | tr ',' '\n' | while read -r r; do
  case "$r" in *-*) seq "${r%-*}" "${r#*-}" ;; "") ;; *) echo "$r" ;; esac
done)
n_cpus=$(echo "$cpu_list" | wc -l)

# ---- cpuidle -------------------------------------------------------------

idle_snapshot() {  # $1 = destination file
  : > "$1"
  for s in /sys/devices/system/cpu/cpu0/cpuidle/state*; do
    [ -d "$s" ] || continue
    idx=${s##*/state}
    name=$(cat "$s/name")
    t=0; u=0
    for c in $cpu_list; do
      p=/sys/devices/system/cpu/cpu$c/cpuidle/state$idx
      [ -d "$p" ] || continue
      t=$(( t + $(cat "$p/time") ))
      u=$(( u + $(cat "$p/usage") ))
    done
    echo "$idx $name $t $u" >> "$1"
  done
}

idle_delta() {  # $1 before, $2 after, $3 wall seconds -> csv + printed shares
  local wall=$3
  echo "state,name,time_us,usage,share_of_cpu_time" > "$out/cpuidle-delta.csv"
  log "cpuidle residency over the measured run (${wall}s x ${n_cpus} CPUs):"
  while read -r idx name t u; do
    local t2 u2 dt du share
    read -r _ _ t2 u2 < <(grep "^$idx " "$2")
    dt=$(( t2 - t )); du=$(( u2 - u ))
    share=$(awk -v d="$dt" -v w="$wall" -v n="$n_cpus" \
      'BEGIN { if (w > 0 && n > 0) printf "%.4f", d / (w * n * 1000000); else print "0" }')
    echo "$idx,$name,$dt,$du,$share" >> "$out/cpuidle-delta.csv"
    log "  state$idx $name  time ${dt} us  entries ${du}  share ${share}"
  done < "$1"
}

C6_STATE=3
c6_saved=""
restore_c6() {
  [ -n "$c6_saved" ] || return 0
  for c in $cpu_list; do
    p=/sys/devices/system/cpu/cpu$c/cpuidle/state$C6_STATE/disable
    [ -w "$p" ] && echo "$c6_saved" > "$p"
  done
  c6_saved=""
}
trap 'restore_c6' EXIT INT TERM

if [ "$no_c6" = 1 ]; then
  p0=/sys/devices/system/cpu/cpu0/cpuidle/state$C6_STATE
  [ -d "$p0" ] || { echo "no cpuidle state$C6_STATE" >&2; exit 2; }
  [ "$(cat "$p0/name")" = C6 ] || log "WARNING state$C6_STATE is $(cat "$p0/name"), not C6"
  c6_saved=$(cat "$p0/disable")
  for c in $cpu_list; do
    echo 1 > /sys/devices/system/cpu/cpu$c/cpuidle/state$C6_STATE/disable
  done
  log "C6 disabled on $n_cpus online CPUs (previous disable value $c6_saved)"
fi

freq() {
  local p=/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq
  if [ -r "$p" ]; then cat "$p"; else echo absent; fi
}

# ---- host contract -------------------------------------------------------

wait_disabled() {
  local deadline=$(( SECONDS + 120 ))
  while [ "$(cat /sys/kernel/sched_ext/state 2>/dev/null)" != disabled ]; do
    [ $SECONDS -lt $deadline ] || { echo "sched_ext not disabled" >&2; exit 1; }
    sleep 0.1
  done
}

exec 9>>"$LOCK"
log "waiting for $LOCK"
flock -x 9
log "lock held"
wait_disabled

tmp=$(mktemp -d -p /tmp accordin-cv-trace-XXXXXXXX)
cleanup() { restore_c6; rm -rf "$tmp"; }
trap cleanup EXIT INT TERM

env_common=(
  env -u LD_PRELOAD -u LD_LIBRARY_PATH
  LC_ALL=C
  MCS_ACCORDIN_DIRECT_DISABLE_BPF=0 MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF=0
  MCS_ACCORDIN_DIRECT_STATS_ONLY=0 MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY=0
  ACCORDIN_DISABLE_ADMISSION=0 ACCORDIN_HOOK_STATS=0 ACCORDIN_CV_COUNTERS=1
  OMP_PROC_BIND=false OMP_WAIT_POLICY=PASSIVE
  LD_PRELOAD="$ADAPTER" LD_LIBRARY_PATH="$LIBDIR"
)
[ "$arm" = custody-off ] && env_common+=(ACCORDIN_CV_CUSTODY=0)

for v in $(env | sed -n 's/^\(ACCORDIN_[A-Z0-9_]*\|MCS_ACCORDIN_[A-Z0-9_]*\|MCS_TAS_ACCORDIN_[A-Z0-9_]*\|SCX_[A-Z0-9_]*\|LD_[A-Z0-9_]*\|COND_VAR[A-Z0-9_]*\|COND_VAE[A-Z0-9_]*\)=.*/\1/p'); do
  unset "$v" || true
done

# ---- optional preceding readrandom ---------------------------------------

if [ "$pre" = 1 ]; then
  log "PRE readrandom ($arm)"
  cp -a "$SEED" "$tmp/pre-db"
  taskset -c "$online" "${env_common[@]}" "$EXE" \
    --threads=$THREADS --time_ms="$time_ms" --benchmarks=readrandom \
    --use_existing_db=1 --db="$tmp/pre-db" > "$out/pre-readrandom.log" 2>&1
  log "PRE $(grep -m1 BENCH_TOTAL "$out/pre-readrandom.log" || echo 'no BENCH_TOTAL')"
  log "PRE $(grep -m1 '\[accordin_cv\]' "$out/pre-readrandom.log" || echo 'no counters')"
  rm -rf "$tmp/pre-db"
  wait_disabled
fi

# ---- measured fillrandom -------------------------------------------------

idle_snapshot "$out/cpuidle-before.txt"
freq > "$out/cpufreq-before.txt"
start_s=$(date +%s.%N)

log "START fillrandom arm=$arm pre=$pre trace=$trace script=$script no_c6=$no_c6"
# taskset execs env which execs db_bench, so the background pid is db_bench.
taskset -c "$online" "${env_common[@]}" "$EXE" \
  --threads=$THREADS --time_ms="$time_ms" --benchmarks=fillrandom \
  --use_existing_db=0 --db="$tmp/db" > "$out/fillrandom.log" 2>&1 &
pid=$!
sleep 0.5
log "db_bench pid $pid comm $(cat /proc/$pid/comm 2>/dev/null || echo gone)"

sampler=""
if [ "$sample" = 1 ]; then
  "$HERE/sample_bss.py" --object "$BPFOBJ" --out "$out/admission-slots.csv" \
    --interval-ms 100 > "$out/sample_bss.log" 2>&1 &
  sampler=$!
fi

tracer=""
if [ "$trace" = 1 ]; then
  sleep "$(awk -v m="$warmup_ms" 'BEGIN { print m / 1000 }')"
  if kill -0 "$pid" 2>/dev/null; then
    log "attaching $script.bt to pid $pid for ${trace_ms} ms"
    # cv_regime attaches only to the traced process; cv_sched needs the whole
    # machine for its tracepoints and filters on comm instead.
    if [ "$script" = cv_regime ]; then
      BPFTRACE_MAX_MAP_KEYS=16384 bpftrace -p "$pid" \
        -o "$out/$script.trace.txt" "$HERE/$script.bt" \
        > "$out/$script.stderr.txt" 2>&1 &
    else
      BPFTRACE_MAX_MAP_KEYS=16384 bpftrace \
        -o "$out/$script.trace.txt" "$HERE/$script.bt" \
        > "$out/$script.stderr.txt" 2>&1 &
    fi
    tracer=$!
    sleep "$(awk -v m="$trace_ms" 'BEGIN { print m / 1000 }')"
    kill -INT "$tracer" 2>/dev/null
    wait "$tracer" 2>/dev/null
    log "trace window closed"
  else
    log "db_bench already gone, no trace taken"
  fi
fi

wait "$pid"
rc=$?
end_s=$(date +%s.%N)
wall=$(awk -v a="$start_s" -v b="$end_s" 'BEGIN { printf "%.3f", b - a }')

[ -n "$sampler" ] && { kill -INT "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null; }

idle_snapshot "$out/cpuidle-after.txt"
freq > "$out/cpufreq-after.txt"
wait_disabled   # leave the host as ../run.py expects to find it

log "returncode $rc wall ${wall}s"
log "$(grep -m1 BENCH_TOTAL "$out/fillrandom.log" || echo 'no BENCH_TOTAL')"
log "$(grep -m1 '\[accordin_cv\]' "$out/fillrandom.log" || echo 'no [accordin_cv] counters')"
log "cpufreq before $(cat "$out/cpufreq-before.txt") after $(cat "$out/cpufreq-after.txt")"
idle_delta "$out/cpuidle-before.txt" "$out/cpuidle-after.txt" "$wall"

if [ -s "$out/admission-slots.csv" ]; then
  awk -F, 'NR > 1 { n++; busy += $2; total = $3; if ($2 == $3) full++ }
    END { if (n) printf "admission slots: %d samples, %d per sample, all busy in %.1f%% of samples, mean busy %.1f\n",
      n, total, 100 * full / n, busy / n }' "$out/admission-slots.csv" \
    | tee -a "$out/run.log"
fi

cat > "$out/config.json" <<EOF
{
  "arm": "$arm", "pre_readrandom": $pre, "traced": $trace, "script": "$script",
  "no_c6": $no_c6, "threads": $THREADS, "time_ms": $time_ms,
  "warmup_ms": $warmup_ms, "trace_ms": $trace_ms,
  "online_cpus": "$online", "n_cpus": $n_cpus,
  "adapter": "$ADAPTER", "libdir": "$LIBDIR", "db_bench": "$EXE",
  "db_bench_sha256": "$have", "wall_seconds": $wall, "returncode": $rc
}
EOF

log "output in $out"
exit $rc
