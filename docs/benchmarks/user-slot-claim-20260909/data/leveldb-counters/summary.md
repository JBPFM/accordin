# LevelDB db_bench comparison of fixed commits

Throughput in Kops/s over valid samples only, relative column against `yield-counters`.

## readrandom / mcs_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| yield-counters | `9fc1b0a` | 1 | 0 | 1212.638 | 0.000 | 0.00 | 1.000x | 1212.638 |
| claim-counters | `9fc1b0a` | 1 | 0 | 1202.928 | 0.000 | 0.00 | 0.992x | 1202.928 |

| arm | renews | claims | undone | queued | adopted | swept | slots_left | demand |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| yield-counters | 0.0 | 0.0 | 0.0 | 84392799.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| claim-counters | 2.0 | 2.0 | 0.0 | 83723366.0 | 3.0 | 0.0 | 0.0 | 0.0 |

## readrandom / mcs_tas_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| yield-counters | `9fc1b0a` | 1 | 0 | 1086.368 | 0.000 | 0.00 | 1.000x | 1086.368 |
| claim-counters | `9fc1b0a` | 1 | 0 | 1102.468 | 0.000 | 0.00 | 1.015x | 1102.468 |

| arm | renews | claims | undone | queued | adopted | swept | slots_left | demand |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| yield-counters | 0.0 | 0.0 | 0.0 | 61831614.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| claim-counters | 8.0 | 3.0 | 0.0 | 66114688.0 | 11.0 | 0.0 | 0.0 | 0.0 |

## fillrandom / mcs_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| yield-counters | `9fc1b0a` | 1 | 0 | 273.624 | 0.000 | 0.00 | 1.000x | 273.624 |
| claim-counters | `9fc1b0a` | 1 | 0 | 272.466 | 0.000 | 0.00 | 0.996x | 272.466 |

| arm | renews | claims | undone | queued | adopted | swept | slots_left | demand |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| yield-counters | 0.0 | 0.0 | 0.0 | 2058836.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| claim-counters | 27086.0 | 2206.0 | 0.0 | 2049805.0 | 29291.0 | 0.0 | 0.0 | 0.0 |

## fillrandom / mcs_tas_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| yield-counters | `9fc1b0a` | 1 | 0 | 271.756 | 0.000 | 0.00 | 1.000x | 271.756 |
| claim-counters | `9fc1b0a` | 1 | 0 | 258.009 | 0.000 | 0.00 | 0.949x | 258.009 |

| arm | renews | claims | undone | queued | adopted | swept | slots_left | demand |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| yield-counters | 0.0 | 0.0 | 0.0 | 1481511.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| claim-counters | 15306.0 | 2114.0 | 0.0 | 1512773.0 | 17419.0 | 0.0 | 0.0 | 0.0 |

## Invalid runs

None.

