# Featherbit (static glibc, glibc malloc): tuning

Experimental arm: same as [`../featherbit/TUNING.md`](../featherbit/TUNING.md) but built
without the `mimalloc` cargo feature, so glibc's own malloc is the allocator. It asks whether
glibc's allocator is good enough to drop mimalloc and get back its peak-RSS cost. Last measured
(2026-10-09, laptop): peak RSS 10-15 MB vs 30-55 MB, but 8-24% less throughput at 4 cores.
