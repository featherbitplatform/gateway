# Featherbit (static glibc, glibc malloc): tuning

Experimental arm: same as [`../featherbit-glibc/TUNING.md`](../featherbit-glibc/TUNING.md) but
built without the `mimalloc` cargo feature, so glibc's own malloc is the allocator. It asks
whether glibc's allocator is good enough to drop mimalloc and get back its peak-RSS cost.
