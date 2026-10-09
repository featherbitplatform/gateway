# Featherbit (static glibc): tuning

Experimental arm: same as [`../featherbit/TUNING.md`](../featherbit/TUNING.md) — same configs,
env, `dist` profile and mimalloc allocator — except the binary is a fully static **glibc**
build (`Dockerfile.glibc`, `+crt-static`, still `FROM scratch`) instead of static musl.
It isolates the libc itself (mostly `memcpy`/`memmove`/`memchr`) from the allocator.
