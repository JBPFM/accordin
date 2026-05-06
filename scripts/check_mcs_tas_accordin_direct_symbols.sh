#!/usr/bin/env bash
set -euo pipefail

lib="${1:-target/release/libmcs_tas_accordin_direct.so}"

if [[ ! -f "$lib" ]]; then
  echo "missing library: $lib" >&2
  exit 1
fi

symbols="$(nm -D --defined-only "$lib")"

for symbol in \
  mcs_tas_accordin_direct_mutex_create \
  mcs_tas_accordin_direct_mutex_destroy \
  mcs_tas_accordin_direct_mutex_lock \
  mcs_tas_accordin_direct_mutex_trylock \
  mcs_tas_accordin_direct_mutex_unlock
do
  if ! grep -Eq "[[:space:]]${symbol}$" <<<"$symbols"; then
    echo "missing direct ABI symbol: $symbol" >&2
    exit 1
  fi
done

for symbol in \
  pthread_mutex_init \
  pthread_mutex_destroy \
  pthread_mutex_lock \
  pthread_mutex_trylock \
  pthread_mutex_unlock \
  pthread_cond_init \
  pthread_cond_destroy \
  pthread_cond_wait \
  pthread_cond_timedwait \
  pthread_cond_signal \
  pthread_cond_broadcast
do
  if grep -Eq "[[:space:]]${symbol}$" <<<"$symbols"; then
    echo "unexpected pthread hook symbol: $symbol" >&2
    exit 1
  fi
done
