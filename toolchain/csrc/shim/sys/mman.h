/* wasm2c's runtime includes <sys/mman.h> unconditionally; on WASI there is no mmap and we
 * build the runtime in bounds-check mode (WASM_RT_USE_MMAP=0), so nothing here is ever called. */
#ifndef WASMUX_SHIM_SYS_MMAN_H
#define WASMUX_SHIM_SYS_MMAN_H
#include <stddef.h>
#include <sys/types.h>
#define PROT_NONE 0
#define PROT_READ 1
#define PROT_WRITE 2
#define MAP_PRIVATE 2
#define MAP_ANONYMOUS 0x20
#define MAP_FAILED ((void *)-1)
static inline void *mmap(void *a, size_t l, int p, int f, int fd, off_t o) { (void)a; (void)l; (void)p; (void)f; (void)fd; (void)o; return MAP_FAILED; }
static inline int munmap(void *a, size_t l) { (void)a; (void)l; return -1; }
static inline int mprotect(void *a, size_t l, int p) { (void)a; (void)l; (void)p; return -1; }
#endif
