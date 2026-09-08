#include <stdint.h>
#include <features.h>
extern int
__attribute__((import_module("wasmux"), import_name("args")))
__wasmux_args(void *buf, int cap);   /* cap==0: return needed size; else fill and return size */
extern void
__attribute__((import_module("wasmux"), import_name("init")))
__wasmux_init(volatile int *sigpending);
extern volatile int __wasmux_sigpending;
/* clang renames a main(argc, argv) definition to __main_argc_argv on wasm32 */
int __main_argc_argv(int, char **);
/* musl calls main through a 3-argument pointer; wasm traps on signature mismatch */
static int main3(int argc, char **argv, char **envp) { return __main_argc_argv(argc, argv); }
int __libc_start_main(int (*)(), int, char **, void (*)(), void(*)(), void(*)());
__attribute__((__weak__)) void __wasm_call_ctors(void);
__attribute__((export_name("_start"))) void _start(void)
{
	if (__wasm_call_ctors) __wasm_call_ctors();
	__wasmux_init(&__wasmux_sigpending);
	int n = __wasmux_args(0, 0);
	long *p = __builtin_alloca(n + 16);
	p = (long *)(((uintptr_t)p + 15) & ~(uintptr_t)15);
	__wasmux_args(p, n);
	__libc_start_main(main3, (int)p[0], (char **)(p + 1), 0, 0, 0);
}
