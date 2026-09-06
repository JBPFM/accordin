static inline unsigned long long bench_ticks(void) { unsigned long long v; __asm__ volatile("isb; mrs %0, cntvct_el0" : "=r"(v) :: "memory"); return v; }
