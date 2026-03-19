#!/usr/bin/env bash
set -euo pipefail

# OceanBase + TPC-C bootstrap script for TEST environments.
# It uses OceanBase All in One + OBD quick deployment and then installs/patches BenchmarkSQL.
# Default target: single-node performance sandbox (obd perf) + MySQL-mode tenant.

# ------------------------------
# Configurable variables
# ------------------------------
: "${OB_CLUSTER_NAME:=perf}"
: "${OB_DEPLOY_MODE:=perf}"          # perf or demo
: "${OB_TENANT_NAME:=test}"
: "${OB_TENANT_PASSWORD:=Test_tpcc_123!}"
: "${OB_DB_NAME:=tpccdb}"
: "${OB_UNIT_NAME:=tpcc_unit}"
: "${OB_POOL_NAME:=tpcc_pool}"
: "${OB_UNIT_CPU:=2}"
: "${OB_UNIT_MEMORY:=5G}"
: "${OB_UNIT_LOG_DISK:=10G}"
: "${WORKDIR:=$HOME/oceanbase-tpcc}"
: "${BENCHMARKSQL_DIR:=$WORKDIR/benchmarksql-5}"
: "${BENCHMARKSQL_GIT_URL:=https://github.com/angoca/Benchmarksql-5.git}"
: "${MYSQL_CONNECTOR_URL:=https://repo1.maven.org/maven2/mysql/mysql-connector-java/5.1.49/mysql-connector-java-5.1.49.jar}"
: "${WAREHOUSES:=10}"
: "${LOAD_WORKERS:=4}"
: "${TERMINALS:=10}"
: "${RUN_MINS:=10}"
: "${LIMIT_TXNS_PER_MIN:=0}"
: "${DEPLOY_LOG:=$WORKDIR/obd-${OB_DEPLOY_MODE}.log}"

mkdir -p "$WORKDIR"

log() { printf '\n[%s] %s\n' "$(date '+%F %T')" "$*"; }
die() { echo "ERROR: $*" >&2; exit 1; }
need_cmd() { command -v "$1" >/dev/null 2>&1 || die "missing command: $1"; }

parse_args() {
  STEP="all"
  while [ $# -gt 0 ]; do
    case "$1" in
      all|install-ob|deploy-ob|create-tenant|install-bmsql|run-tpcc|run-benchmark)
        STEP="$1"
        shift
        ;;
      --terminals)
        [ $# -ge 2 ] || die "missing value for --terminals"
        TERMINALS="$2"
        shift 2
        ;;
      --warehouses)
        [ $# -ge 2 ] || die "missing value for --warehouses"
        WAREHOUSES="$2"
        shift 2
        ;;
      --load-workers)
        [ $# -ge 2 ] || die "missing value for --load-workers"
        LOAD_WORKERS="$2"
        shift 2
        ;;
      --run-mins)
        [ $# -ge 2 ] || die "missing value for --run-mins"
        RUN_MINS="$2"
        shift 2
        ;;
      --limit-txns-per-min)
        [ $# -ge 2 ] || die "missing value for --limit-txns-per-min"
        LIMIT_TXNS_PER_MIN="$2"
        shift 2
        ;;
      -h|--help|help)
        usage
        exit 0
        ;;
      *)
        die "unknown argument: $1"
        ;;
    esac
  done
}

install_deps() {
  log "Installing OS dependencies"
  if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
      curl git unzip tar sed gawk grep coreutils procps \
      ant default-jdk mysql-client python3
  elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y \
      curl git unzip tar sed gawk grep coreutils procps-ng \
      ant java-1.8.0-openjdk-devel mysql python3
  elif command -v yum >/dev/null 2>&1; then
    sudo yum install -y \
      curl git unzip tar sed gawk grep coreutils procps-ng \
      ant java-1.8.0-openjdk-devel mysql python3
  else
    die "unsupported package manager; install curl/git/ant/java/mysql-client/python3 manually"
  fi
}

install_all_in_one() {
  if command -v obd >/dev/null 2>&1 && command -v obclient >/dev/null 2>&1; then
    log "OBD and OBClient already installed"
  else
    log "Installing OceanBase All in One"
    # Official online installer command published in OceanBase quick start docs/blog.
    bash -c "$(curl -fsSL https://obbusiness-private.oss-cn-shanghai.aliyuncs.com/download-center/opensource/oceanbase-all-in-one/installer.sh)"
  fi

  if [ -f "$HOME/.oceanbase-all-in-one/bin/env.sh" ]; then
    # shellcheck disable=SC1090
    source "$HOME/.oceanbase-all-in-one/bin/env.sh"
  fi
  need_cmd obd
  need_cmd obclient
}

quick_deploy_cluster() {
  # OBD quick deployment commands use fixed deploy names demo/perf.
  if obd cluster list 2>/dev/null | awk 'NR>1 {print $1}' | grep -qx "$OB_CLUSTER_NAME"; then
    log "Cluster $OB_CLUSTER_NAME already exists; skip deployment"
    return 0
  fi

  log "Deploying single-node OceanBase cluster via 'obd ${OB_DEPLOY_MODE}'"
  rm -f "$DEPLOY_LOG"
  if [ "$OB_DEPLOY_MODE" = "perf" ]; then
    yes y | obd perf 2>&1 | tee "$DEPLOY_LOG"
  else
    yes y | obd demo 2>&1 | tee "$DEPLOY_LOG"
  fi
}

cluster_exists() {
  obd cluster list 2>/dev/null | awk 'NR>1 {print $1}' | grep -qx "$OB_CLUSTER_NAME"
}

cluster_config_path() {
  printf '%s\n' "$HOME/.obd/cluster/$OB_CLUSTER_NAME/config.yaml"
}

load_sys_conn_from_cluster_config() {
  local config host port pass
  config="$(cluster_config_path)"
  [ -f "$config" ] || return 1

  host="$(
    awk '
      /^oceanbase-ce:/ { in_ob=1; next }
      in_ob && /^  servers:/ {
        getline
        sub(/^  - /, "", $0)
        print
        exit
      }
    ' "$config"
  )"
  port="$(sed -nE 's/^    mysql_port: ([0-9]+)$/\1/p' "$config" | head -n1)"
  pass="$(sed -nE 's/^    root_password: (.*)$/\1/p' "$config" | head -n1)"

  OB_SYS_HOST="${host:-127.0.0.1}"
  OB_SYS_PORT="${port:-2881}"
  OB_SYS_PASSWORD="$pass"
  export OB_SYS_HOST OB_SYS_PORT OB_SYS_PASSWORD

  log "SYS tenant endpoint from cluster config: ${OB_SYS_HOST}:${OB_SYS_PORT}"
  return 0
}

extract_sys_conn() {
  local line host port pass
  line="$(grep -Eo 'obclient[^[:cntrl:]]*-uroot@sys[^[:cntrl:]]*' "$DEPLOY_LOG" | tail -n1 || true)"
  if [ -z "$line" ]; then
    if load_sys_conn_from_cluster_config; then
      return 0
    fi
    log "Could not parse sys connection string from deployment log or cluster config; fallback to 127.0.0.1:2881 with empty password"
    OB_SYS_HOST="127.0.0.1"
    OB_SYS_PORT="2881"
    OB_SYS_PASSWORD=""
    export OB_SYS_HOST OB_SYS_PORT OB_SYS_PASSWORD
    return 0
  fi

  host="$(printf '%s\n' "$line" | sed -nE 's/.*-h([^ ]+).*/\1/p')"
  port="$(printf '%s\n' "$line" | sed -nE 's/.*-P([0-9]+).*/\1/p')"
  if printf '%s\n' "$line" | grep -qE -- '-p[^ ]+'; then
    pass="$(printf '%s\n' "$line" | sed -nE 's/.*-p([^ ]+).*/\1/p')"
  else
    pass=""
  fi

  OB_SYS_HOST="${host:-127.0.0.1}"
  OB_SYS_PORT="${port:-2881}"
  OB_SYS_PASSWORD="$pass"
  export OB_SYS_HOST OB_SYS_PORT OB_SYS_PASSWORD

  log "SYS tenant endpoint: ${OB_SYS_HOST}:${OB_SYS_PORT}"
}

ensure_cluster_reachable() {
  if obclient_sys 'select 1;' >/dev/null 2>&1; then
    return 0
  fi

  if cluster_exists; then
    log "Cluster ${OB_CLUSTER_NAME} is deployed but not reachable; trying to start it"
    if ! obd cluster start "$OB_CLUSTER_NAME"; then
      die "failed to start cluster ${OB_CLUSTER_NAME}; fix the OBD precheck errors and retry"
    fi
  fi
}

obclient_sys() {
  local sql="$1"
  if [ -n "${OB_SYS_PASSWORD:-}" ]; then
    obclient -h"$OB_SYS_HOST" -P"$OB_SYS_PORT" -uroot@sys -p"$OB_SYS_PASSWORD" -Doceanbase -A -N -e "$sql"
  else
    obclient -h"$OB_SYS_HOST" -P"$OB_SYS_PORT" -uroot@sys -Doceanbase -A -N -e "$sql"
  fi
}

obclient_tenant() {
  local sql="$1"
  obclient -h"$OB_SYS_HOST" -P"$OB_SYS_PORT" -uroot@${OB_TENANT_NAME} -p"$OB_TENANT_PASSWORD" -D"$OB_DB_NAME" -A -N -e "$sql"
}

wait_sys_ready() {
  log "Waiting for SYS tenant to become available"
  local i
  for i in $(seq 1 60); do
    if obclient_sys 'select 1;' >/dev/null 2>&1; then
      return 0
    fi
    sleep 5
  done
  die "SYS tenant not ready after waiting"
}

create_tpcc_tenant() {
  local zone_sql zone tenant_exists
  ensure_cluster_reachable
  wait_sys_ready
  zone_sql="SELECT ZONE FROM oceanbase.DBA_OB_ZONES LIMIT 1;"
  zone="$(obclient_sys "$zone_sql" | head -n1 | tr -d '\r')"
  [ -n "$zone" ] || die "failed to detect cluster zone"
  log "Detected zone: $zone"

  tenant_exists="$(obclient_sys "SELECT COUNT(*) FROM oceanbase.DBA_OB_TENANTS WHERE TENANT_NAME='${OB_TENANT_NAME}';" | tr -d '\r')"
  if [ "$tenant_exists" = "1" ]; then
    log "Tenant ${OB_TENANT_NAME} already exists; skip tenant creation"
  else
    log "Creating unit, pool, and MySQL tenant ${OB_TENANT_NAME}"
    obclient_sys "CREATE RESOURCE UNIT IF NOT EXISTS ${OB_UNIT_NAME} MAX_CPU ${OB_UNIT_CPU}, MIN_CPU ${OB_UNIT_CPU}, MEMORY_SIZE '${OB_UNIT_MEMORY}', LOG_DISK_SIZE '${OB_UNIT_LOG_DISK}';"
    obclient_sys "CREATE RESOURCE POOL IF NOT EXISTS ${OB_POOL_NAME} UNIT='${OB_UNIT_NAME}', UNIT_NUM=1, ZONE_LIST=('${zone}');"
    obclient_sys "CREATE TENANT IF NOT EXISTS ${OB_TENANT_NAME} CHARSET='utf8mb4', RESOURCE_POOL_LIST=('${OB_POOL_NAME}') SET ob_tcp_invited_nodes='%';"
  fi

  log "Trying to connect to tenant root user with empty password to initialize it"
  if obclient -h"$OB_SYS_HOST" -P"$OB_SYS_PORT" -uroot@${OB_TENANT_NAME} -Dtest -A -N -e 'select 1;' >/dev/null 2>&1; then
    obclient -h"$OB_SYS_HOST" -P"$OB_SYS_PORT" -uroot@${OB_TENANT_NAME} -A -N -e "ALTER USER root IDENTIFIED BY '${OB_TENANT_PASSWORD}';" || true
  fi

  log "Creating database ${OB_DB_NAME}"
  obclient -h"$OB_SYS_HOST" -P"$OB_SYS_PORT" -uroot@${OB_TENANT_NAME} -p"$OB_TENANT_PASSWORD" -A -N -e "CREATE DATABASE IF NOT EXISTS ${OB_DB_NAME};"
}

install_benchmarksql() {
  log "Installing BenchmarkSQL source tree"
  rm -rf "$BENCHMARKSQL_DIR"
  git clone --depth=1 "$BENCHMARKSQL_GIT_URL" "$BENCHMARKSQL_DIR"

  mkdir -p "$BENCHMARKSQL_DIR/lib/oceanbase"
  curl -fsSL "$MYSQL_CONNECTOR_URL" -o "$BENCHMARKSQL_DIR/lib/oceanbase/mysql-connector-java-5.1.49.jar"

  log "Patching BenchmarkSQL for OceanBase"
  python3 - "$BENCHMARKSQL_DIR" <<'PY'
import os, re, sys
root = sys.argv[1]

def patch_file(path, transform):
    with open(path, 'r', encoding='utf-8') as f:
        old = f.read()
    new = transform(old)
    if new != old:
        with open(path, 'w', encoding='utf-8') as f:
            f.write(new)

# src/client/jtpcc/jTPCC.java
p = os.path.join(root, 'src', 'client', 'jtpcc', 'jTPCC.java')
if os.path.exists(p):
    def tr(s):
        if 'DB_OCEANBASE' in s:
            return s
        pat = "else if (iDB.equals(\"postgres\"))\\n\\s*dbType = DB_POSTGRES;"
        rep = "else if (iDB.equals(\"postgres\"))\n            dbType = DB_POSTGRES;\n        else if (iDB.equals(\"oceanbase\"))\n            dbType = DB_OCEANBASE;"
        return re.sub(pat, rep, s)
    patch_file(p, tr)

# src/client/jtpcc/jTPCCConfig.java
p = os.path.join(root, 'src', 'client', 'jtpcc', 'jTPCCConfig.java')
if os.path.exists(p):
    def tr(s):
        if 'DB_OCEANBASE' in s:
            return s
        s = s.replace('DB_POSTGRES = 3;', 'DB_POSTGRES = 3, DB_OCEANBASE = 4;')
        return s
    patch_file(p, tr)

# src/client/jtpcc/jTPCCConnection.java
p = os.path.join(root, 'src', 'client', 'jtpcc', 'jTPCCConnection.java')
if os.path.exists(p):
    def tr(s):
        if ')AS L"' in s or ') AS L"' in s:
            return s
        return s.replace(') "+\n                    " )");', ') "+\n                    " ) AS L");')
    patch_file(p, tr)

# run/funcs.sh
p = os.path.join(root, 'run', 'funcs.sh')
if os.path.exists(p):
    def tr(s):
        if 'cp="../lib/oceanbase/*:../lib/*"' not in s:
            s = re.sub(
                r'(\n\s*postgres\)\n\s*cp="\.\./lib/postgres/\*:\.\./lib/\*"\n\s*;;)',
                r'\1\n\toceanbase)\n\t    cp="../lib/oceanbase/*:../lib/*"\n\t    ;;',
                s,
                count=1,
            )
        if 'firebird|oracle|postgres|oceanbase)' not in s:
            s = s.replace('firebird|oracle|postgres)', 'firebird|oracle|postgres|oceanbase)')
        return s
    patch_file(p, tr)

# run/runLoader.sh
p = os.path.join(root, 'run', 'runLoader.sh')
if os.path.exists(p):
    def tr(s):
        return s.replace('java -cp "$myCP" $myOPTS LoadData $*',
                         'java -cp "$myCP" $myOPTS jtpcc.LoadData $*')
    patch_file(p, tr)

# run/runBenchmark.sh
p = os.path.join(root, 'run', 'runBenchmark.sh')
if os.path.exists(p):
    def tr(s):
        return s.replace('java -cp "$myCP" $myOPTS jTPCC',
                         'java -cp "$myCP" $myOPTS jtpcc.jTPCC')
    patch_file(p, tr)

# run/runDatabaseBuild.sh
p = os.path.join(root, 'run', 'runDatabaseBuild.sh')
if os.path.exists(p):
    def tr(s):
        return s.replace('AFTER_LOAD="indexCreates foreignKeys extraHistID buildFinish"',
                         'AFTER_LOAD="indexCreates buildFinish"')
    patch_file(p, tr)
PY

  log "Building BenchmarkSQL"
  (cd "$BENCHMARKSQL_DIR" && ant)
}

ensure_benchmarksql_installed() {
  if [ -d "$BENCHMARKSQL_DIR/run" ]; then
    return 0
  fi

  log "BenchmarkSQL not found under $BENCHMARKSQL_DIR; installing it now"
  install_benchmarksql
}

generate_props() {
  local props="$BENCHMARKSQL_DIR/run/props.ob"
  cat > "$props" <<EOF_PROPS
# OceanBase BenchmarkSQL properties
# Generated by setup_oceanbase_tpcc.sh

db=oceanbase
driver=com.mysql.jdbc.Driver
conn=jdbc:mysql://${OB_SYS_HOST}:${OB_SYS_PORT}/${OB_DB_NAME}?useUnicode=true&characterEncoding=utf-8&rewriteBatchedStatements=true&allowMultiQueries=true&useSSL=false&verifyServerCertificate=false
user=root@${OB_TENANT_NAME}
password=${OB_TENANT_PASSWORD}

warehouses=${WAREHOUSES}
loadWorkers=${LOAD_WORKERS}
terminals=${TERMINALS}

# To run specified transactions per terminal, runMins must be zero.
runTxnsPerTerminal=0

# To run for specified minutes, runTxnsPerTerminal must be zero.
runMins=${RUN_MINS}

# Number of transactions per minute, 0 = no limit
limitTxnsPerMin=${LIMIT_TXNS_PER_MIN}

# Optional paths
# fileLocation=${WORKDIR}/tpcc-result
EOF_PROPS
  log "Generated $props"
}

run_tpcc() {
  ensure_benchmarksql_installed
  ensure_cluster_reachable
  wait_sys_ready
  generate_props
  log "Building TPC-C schema and loading data"
  (cd "$BENCHMARKSQL_DIR/run" && ./runDatabaseBuild.sh props.ob)

  log "Running BenchmarkSQL"
  (cd "$BENCHMARKSQL_DIR/run" && ./runBenchmark.sh props.ob)
}

run_benchmark_only() {
  ensure_benchmarksql_installed
  ensure_cluster_reachable
  wait_sys_ready
  generate_props
  log "Running BenchmarkSQL only (skip schema build and data load)"
  (cd "$BENCHMARKSQL_DIR/run" && ./runBenchmark.sh props.ob)
}

usage() {
  cat <<EOF_USAGE
Usage: $0 [all|install-ob|deploy-ob|create-tenant|install-bmsql|run-tpcc|run-benchmark] [options]

Options:
  --terminals N                 Benchmark terminal count / concurrency
  --run-mins N                  Benchmark duration in minutes
  --warehouses N                Warehouse count written to props.ob
  --load-workers N              Parallel data loaders for run-tpcc
  --limit-txns-per-min N        Rate limit, 0 means unlimited
  -h, --help                    Show this help

Environment variables you may override:
  OB_DEPLOY_MODE=perf|demo      Quick deployment mode (default: perf)
  OB_CLUSTER_NAME=perf|demo     Quick deployment name (default: perf)
  OB_TENANT_NAME=test           Tenant name
  OB_TENANT_PASSWORD=...        Tenant root password to set/use
  OB_DB_NAME=tpccdb             Database name
  OB_UNIT_CPU=2                 Unit CPU
  OB_UNIT_MEMORY=5G             Unit memory (docs say 5G default minimum for CREATE RESOURCE UNIT)
  WAREHOUSES=10                 TPC-C warehouse count
  LOAD_WORKERS=4                Data load worker count
  TERMINALS=10                  Terminal count
  RUN_MINS=10                   Benchmark duration
  LIMIT_TXNS_PER_MIN=0          Benchmark rate limit

Examples:
  $0 run-benchmark --terminals 20 --run-mins 5
  $0 run-tpcc --terminals 40 --load-workers 8
EOF_USAGE
}

main() {
  parse_args "$@"
  case "$STEP" in
    all)
      install_deps
      install_all_in_one
      quick_deploy_cluster
      extract_sys_conn
      create_tpcc_tenant
      install_benchmarksql
      run_tpcc
      ;;
    install-ob)
      install_deps
      install_all_in_one
      ;;
    deploy-ob)
      install_all_in_one
      quick_deploy_cluster
      ;;
    create-tenant)
      install_all_in_one
      extract_sys_conn
      create_tpcc_tenant
      ;;
    install-bmsql)
      install_deps
      install_benchmarksql
      ;;
    run-tpcc)
      install_all_in_one
      extract_sys_conn
      run_tpcc
      ;;
    run-benchmark)
      install_all_in_one
      extract_sys_conn
      run_benchmark_only
      ;;
    -h|--help|help)
      usage
      ;;
    *)
      usage
      exit 1
      ;;
  esac
}

main "$@"
