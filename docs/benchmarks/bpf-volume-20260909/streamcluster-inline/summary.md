# streamcluster comparison of fixed commits

Seconds over valid samples only, lower is better; the relative column is the ratio of mean seconds against `bpfbase`, so below 1.000 is faster.

## stream / mcs_accordin

| arm | commit | n valid | n invalid | mean s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 34.992 | 0.733 | 2.09 | 1.000x | 35.798 / 34.368 / 34.808 |
| inline | `0082ed7` | 3 | 0 | 34.517 | 0.889 | 2.58 | 0.986x | 34.157 / 33.864 / 35.530 |

## stream / mcs_tas_accordin

| arm | commit | n valid | n invalid | mean s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 35.178 | 0.250 | 0.71 | 1.000x | 35.182 / 35.426 / 34.926 |
| inline | `0082ed7` | 3 | 0 | 34.314 | 0.711 | 2.07 | 0.975x | 34.471 / 34.933 / 33.538 |

## Invalid runs

None.

