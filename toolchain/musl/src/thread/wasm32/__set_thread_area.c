#include <stdint.h>
uintptr_t __wasmux_tp;
int __set_thread_area(void *p) { __wasmux_tp = (uintptr_t)p; return 0; }
