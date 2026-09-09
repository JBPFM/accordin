# streamcluster comparison of fixed commits

Seconds over valid samples only, lower is better; the relative column is the ratio of mean seconds against `bpfbase`, so below 1.000 is faster.

## stream / mcs_accordin

| arm | commit | n valid | n invalid | mean s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 36.235 | 0.944 | 2.61 | 1.000x | 36.791 / 35.145 / 36.769 |
| bpftip | `432bee6` | 3 | 0 | 35.196 | 0.191 | 0.54 | 0.971x | 35.063 / 35.415 / 35.111 |

## stream / mcs_tas_accordin

| arm | commit | n valid | n invalid | mean s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 36.458 | 0.288 | 0.79 | 1.000x | 36.254 / 36.334 / 36.787 |
| bpftip | `432bee6` | 3 | 0 | 35.274 | 0.698 | 1.98 | 0.968x | 34.477 / 35.779 / 35.566 |

## Invalid runs

None.

