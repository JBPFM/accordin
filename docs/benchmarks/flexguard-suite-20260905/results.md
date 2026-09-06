# Performance results

Values are arithmetic means; higher ops/s and lower seconds are better. Ratios >1 mean faster than FlexGuard.

| Workload | Threads | MCS-Accordin | MCS-TAS-Accordin | FlexGuard | MCS / FG | TAS / FG |
|---|---:|---:|---:|---:|---:|---:|
| scheduling | 96 | 1.978 Mops/s (n=3, CV 3.2%) | 2.859 Mops/s (n=3, CV 6.7%) | 1.219 Mops/s (n=3, CV 4.7%) | 1.623x | 2.345x |
| scheduling | 192 | 1.744 Mops/s (n=3, CV 5.8%) | 2.799 Mops/s (n=3, CV 1.5%) | 1.004 Mops/s (n=3, CV 8.5%) | 1.738x | 2.789x |
| buckets | 96 | 0.018 Mops/s (n=3, CV 3.2%) | 0.023 Mops/s (n=3, CV 1.3%) | 0.472 Mops/s (n=3, CV 13.0%) | 0.038x | 0.049x |
| buckets | 192 | 0.013 Mops/s (n=3, CV 1.7%) | 0.015 Mops/s (n=3, CV 2.2%) | 0.013 Mops/s (n=3, CV 3.6%) | 0.989x | 1.194x |
| leveldb-readrandom | 96 | 0.584 Mops/s (n=3, CV 0.8%) | 0.908 Mops/s (n=3, CV 3.0%) | 0.523 Mops/s (n=3, CV 10.9%) | 1.116x | 1.736x |
| leveldb-readrandom | 192 | 0.611 Mops/s (n=3, CV 2.7%) | 0.949 Mops/s (n=3, CV 1.8%) | 0.383 Mops/s (n=3, CV 2.8%) | 1.595x | 2.475x |
| leveldb-fillrandom | 96 | 0.042 Mops/s (n=3, CV 5.1%) | 0.029 Mops/s (n=3, CV 3.3%) | 0.125 Mops/s (n=3, CV 0.7%) | 0.339x | 0.230x |
| leveldb-fillrandom | 192 | 0.017 Mops/s (n=3, CV 0.7%) | 0.017 Mops/s (n=3, CV 1.0%) | 0.039 Mops/s (n=3, CV 2.8%) | 0.428x | 0.432x |
| leveldb-fillseq | 96 | 0.044 Mops/s (n=3, CV 7.0%) | 0.027 Mops/s (n=3, CV 4.2%) | 0.133 Mops/s (n=3, CV 1.0%) | 0.332x | 0.205x |
| leveldb-fillseq | 192 | 0.016 Mops/s (n=3, CV 0.9%) | 0.017 Mops/s (n=3, CV 1.4%) | 0.040 Mops/s (n=3, CV 4.5%) | 0.405x | 0.433x |
| leveldb-readseq | 96 | 45.740 Mops/s (n=3, CV 7.1%) | 42.911 Mops/s (n=3, CV 0.5%) | 240.123 Mops/s (n=3, CV 13.0%) | 0.190x | 0.179x |
| leveldb-readseq | 192 | 49.948 Mops/s (n=3, CV 0.5%) | 47.043 Mops/s (n=3, CV 3.7%) | 233.867 Mops/s (n=3, CV 5.4%) | 0.214x | 0.201x |
| leveldb-overwrite | 96 | 0.045 Mops/s (n=3, CV 8.3%) | 0.028 Mops/s (n=3, CV 3.5%) | 0.124 Mops/s (n=3, CV 0.8%) | 0.364x | 0.223x |
| leveldb-overwrite | 192 | 0.016 Mops/s (n=3, CV 1.8%) | 0.017 Mops/s (n=3, CV 1.2%) | 0.039 Mops/s (n=3, CV 1.9%) | 0.418x | 0.424x |
| kyotocabinet | 96 | timeout | timeout | 0.121 Mops/s (n=3, CV 3.3%) | - | - |
| kyotocabinet | 192 | timeout | timeout | 0.136 Mops/s (n=3, CV 0.3%) | - | - |
| raytrace | 96 | 1.929 s (n=3, CV 3.6%) | 1.796 s (n=3, CV 3.5%) | 0.972 s (n=3, CV 26.4%) | 0.504x | 0.541x |
| raytrace | 192 | 1.676 s (n=3, CV 3.5%) | 1.542 s (n=3, CV 1.6%) | 1.327 s (n=3, CV 21.0%) | 0.792x | 0.861x |
| dedup | 96 | 2.924 s (n=3, CV 2.3%) | 2.947 s (n=3, CV 2.8%) | 2.823 s (n=3, CV 1.7%) | 0.966x | 0.958x |
| dedup | 192 | 3.050 s (n=3, CV 2.8%) | 3.085 s (n=3, CV 2.0%) | 3.168 s (n=3, CV 2.1%) | 1.039x | 1.027x |
| volrend | 96 | 194.601 s (n=3, CV 1.4%) | 198.051 s (n=3, CV 2.7%) | 19.267 s (n=3, CV 2.2%) | 0.099x | 0.097x |
| volrend | 192 | 503.118 s (n=3, CV 1.6%) | 503.244 s (n=3, CV 0.2%) | 302.570 s (n=3, CV 4.2%) | 0.601x | 0.601x |
| streamcluster | 96 | 118.436 s (n=3, CV 4.7%) | 86.715 s (n=3, CV 3.8%) | 19.047 s (n=3, CV 3.0%) | 0.161x | 0.220x |
| streamcluster | 192 | 400.277 s (n=3, CV 3.1%) | 364.112 s (n=3, CV 2.4%) | 178.838 s (n=3, CV 3.3%) | 0.447x | 0.491x |
| index | 96 | failed | 0.443 Mops/s (n=3, CV 16.9%) | 0.813 Mops/s (n=3, CV 2.5%) | - | 0.545x |
| index | 192 | failed | 1.005 Mops/s (n=3, CV 1.7%) | 0.355 Mops/s (n=3, CV 4.0%) | - | 2.831x |
| index-4k | 96 | 0.952 Mops/s (n=3, CV 0.9%) | 0.878 Mops/s (n=3, CV 1.8%) | 0.853 Mops/s (n=3, CV 2.7%) | 1.117x | 1.030x |
| index-4k | 192 | 0.978 Mops/s (n=3, CV 2.5%) | 0.991 Mops/s (n=3, CV 0.1%) | 0.560 Mops/s (n=3, CV 5.5%) | 1.745x | 1.769x |
