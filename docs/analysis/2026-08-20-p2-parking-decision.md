# Decision: scheduler-custody short-wait parking (P2) is not built

P2 proposed parking short condition-variable waits in scheduler custody
(per-thread parking DSQs with a wake ring) instead of futex, on the theory
that short waits pay a full sleep/wake round trip that a scheduler-side
park could avoid. The design was gated on three conditions, all of which
had to hold. Measured against the diagnosis in
`2026-08-20-cv-diagnosis-m0.md` and the requeue ablation data:

1. **Short waits must exceed ~20% of CV wakeups on non-writer CVs.**
   Measured: under 0.05% of writer-queue waits complete within ~65 µs in
   any arm, and the distribution centers at 0.4–7 ms depending on the lock
   layer. readrandom performs no application-level CV waits at all. The
   population P2 would serve does not exist in the target workloads.
2. **Wake-to-lock latency must remain dominated by the wakeup leg after
   admission routing.** Measured: admission routing alone (CV_SLEEP wake
   routing, no parking) cuts the mean writer wait by 33% and moves the
   distribution mode a full log2 bucket; the residual wait is admission
   queueing at the lock, which parking custody would not change.
3. **The requeue protocol must be stable enough to build on.** This one
   holds — zero requeue fallbacks and exact drain reconciliation across
   every ablation run — but gates 1 and 2 fail independently.

Decision: **no-go.** The mechanism would add per-thread kernel state, a
wake-ring ACK protocol, and idempotent-consumption bookkeeping to serve a
wait population measured in hundredths of a percent. Long waits stay on
futex; broadcast herds are paced by staging; wake-one latency is addressed
by admission routing. Revisit only if a workload appears whose CV wait
distribution concentrates below ~50 µs — the diagnosis instrumentation
(`leveldb_cv_*` histograms and `scripts/futex_block_by_uaddr.bt`) is the
qualification test.
