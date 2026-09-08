extern uintptr_t __wasmux_tp;
static inline uintptr_t __get_tp(void) { return __wasmux_tp; }
